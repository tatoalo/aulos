//! The HTTP/2 client that talks to Apple, and the response taxonomy it turns answers into
//! (DESIGN §25.5).
//!
//! # Why the outcome is not an error
//!
//! Three of the five things Apple can say are *instructions*, not failures: `410 Unregistered`
//! and `400 BadDeviceToken` mean "prune this token", `403 ExpiredProviderToken` means "sign a new
//! JWT". Modelling them as `Err` would make the notifier's call sites read like error handling
//! when they are really a small state machine, so [`Outcome`] is a `Result::Ok` value and
//! [`crate::ApnsError`] is reserved for a misconfigured server.
//!
//! # HTTP/2 in production, HTTP/1.1 in the tests
//!
//! Apple's gateway is HTTP/2-only, which is why this crate turns on `reqwest`'s `http2` feature.
//! reqwest negotiates the version through ALPN, so the same client speaks HTTP/1.1 to a plain
//! `http://127.0.0.1` mock gateway — that is what makes [`ApnsClient::with_base_url`] (and the
//! `APNS_BASE_URL_OVERRIDE` knob behind it) enough to test every branch below without TLS.

use std::sync::Arc;
use std::time::Duration;

use aulos_core::clock::Clock;
use aulos_core::config::Config;
use aulos_core::ports::ApnsEnvironment;
use serde_json::Value;

use crate::error::ApnsError;
use crate::health::Counters;
use crate::jwt::ProviderToken;

/// The production gateway.
pub const PRODUCTION_BASE: &str = "https://api.push.apple.com:443";

/// The sandbox gateway, for Debug and simulator builds.
pub const SANDBOX_BASE: &str = "https://api.sandbox.push.apple.com:443";

/// The per-request ceiling. A push is fire-and-forget; nothing may hang on one.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The retry ladder for a Live Activity **update**: three retries, so four requests at worst.
///
/// Updates stay short on purpose. A progress frame that could not be delivered is superseded by
/// the next one five seconds later, so spending five minutes on it would only land a stale
/// percentage on the lock screen — and the trailing-edge timer is already the retry that matters.
pub const DEFAULT_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(4),
    Duration::from_secs(16),
];

/// The retry ladder for a priority-10 push — an alert, a Live Activity start, a Live Activity end
/// (DESIGN §25.5).
///
/// Six retries covering just over eight minutes, because the failure this exists for is not a
/// busy gateway but a dead egress: the VPS runs inside a VPN namespace, and during a blip that
/// lasted a few minutes **both** APNs attempts since boot ended in `GaveUp` after
/// [`DEFAULT_BACKOFF`]'s twenty-one seconds. These three pushes are not superseded by anything —
/// an alert nobody sent is a download the user never hears about, and an `end` nobody sent is a
/// progress ring spinning on the lock screen for ever — so they are worth waiting out a tunnel.
///
/// The wait is bounded twice over: by the ladder, and by the push's own `apns-expiration`, which
/// [`ApnsClient::send`] stops at.
pub const IMMEDIATE_BACKOFF: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(4),
    Duration::from_secs(16),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
];

/// The `apns-priority` at or above which a push gets [`IMMEDIATE_BACKOFF`].
///
/// The same number as `notifier::PRIORITY_IMMEDIATE`, asserted equal by a unit test below: the
/// priority *is* the statement that this push cannot be superseded, so there is no second flag to
/// keep in sync with it.
pub const IMMEDIATE_PRIORITY: u8 = 10;

/// Which `apns-push-type` a push declares.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PushKind {
    /// A user-visible completion/failure alert.
    Alert,
    /// A Live Activity start, update or end.
    LiveActivity,
}

impl PushKind {
    /// The `apns-push-type` header value.
    #[must_use]
    pub const fn header(self) -> &'static str {
        match self {
            Self::Alert => "alert",
            Self::LiveActivity => "liveactivity",
        }
    }
}

/// One ready-to-send push: the headers that vary plus the JSON body.
///
/// The addressee is **not** in here. A Live Activity update goes to the activity's own update
/// token while an alert goes to the device token, and both may exist for the same device, so the
/// token and its environment are arguments to [`ApnsClient::send`] rather than fields of the
/// message.
#[derive(Clone, Debug)]
pub struct Push {
    /// `apns-push-type`.
    pub kind: PushKind,
    /// `apns-topic`. The device's own `bundle_id` when it reported one, `APNS_TOPIC` otherwise;
    /// a Live Activity push appends `.push-type.liveactivity`.
    pub topic: Arc<str>,
    /// `apns-priority`: 10 for alerts and Live Activity start/end, 5 for updates.
    pub priority: u8,
    /// `apns-expiration`, unix seconds. `0` means "deliver now or discard".
    pub expiration: i64,
    /// `apns-collapse-id`; the item id on alerts, so a retry replaces rather than stacks.
    pub collapse_id: Option<Arc<str>>,
    /// The body.
    pub payload: Value,
}

impl Push {
    /// Whether this push is one of the three that nothing supersedes, and therefore walks
    /// [`IMMEDIATE_BACKOFF`] rather than [`DEFAULT_BACKOFF`].
    #[must_use]
    pub const fn is_immediate(&self) -> bool {
        self.priority >= IMMEDIATE_PRIORITY
    }
}

/// What one `send` ended in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// `200`.
    Delivered,
    /// `410 Unregistered`, `400 BadDeviceToken` or `400 DeviceTokenNotForTopic` — the caller
    /// removes the device (or the Live Activity record, when the failing token was an update
    /// token).
    TokenInvalid {
        /// The HTTP status Apple answered with.
        status: u16,
        /// Apple's `reason`.
        reason: Box<str>,
    },
    /// Any other non-retryable `4xx`: logged at WARN and dropped.
    Rejected {
        /// The HTTP status.
        status: u16,
        /// Apple's `reason`, or the body when it carried none.
        reason: Box<str>,
    },
    /// `403` with a provider-token reason that survived a remint and one retry. A
    /// misconfiguration: `APNS_KEY_ID`, `APNS_TEAM_ID` and the `.p8` do not agree.
    ProviderTokenRejected {
        /// Apple's `reason`.
        reason: Box<str>,
    },
    /// `429`, `5xx` or a transport failure that outlived the backoff ladder.
    GaveUp {
        /// The last HTTP status, or `None` when the request never completed.
        status: Option<u16>,
        /// The last reason or transport error.
        reason: Box<str>,
    },
}

impl Outcome {
    /// Whether the token this push was addressed to must be forgotten.
    #[must_use]
    pub const fn prunes_token(&self) -> bool {
        matches!(self, Self::TokenInvalid { .. })
    }

    /// Whether the push landed.
    #[must_use]
    pub const fn delivered(&self) -> bool {
        matches!(self, Self::Delivered)
    }

    /// Whether this outcome means the server is misconfigured, which is what turns `healthz`
    /// `apns` to `degraded`.
    #[must_use]
    pub const fn misconfigured(&self) -> bool {
        matches!(self, Self::ProviderTokenRejected { .. })
    }

    /// A short reason string for logs and for `healthz` `last_error`. `None` when delivered.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        match self {
            Self::Delivered => None,
            Self::TokenInvalid { status, reason } | Self::Rejected { status, reason } => {
                Some(format!("{status} {reason}"))
            }
            Self::ProviderTokenRejected { reason } => Some(format!("403 {reason}")),
            Self::GaveUp { status, reason } => Some(match status {
                Some(s) => format!("{s} {reason} (gave up)"),
                None => format!("{reason} (gave up)"),
            }),
        }
    }
}

/// The reasons that mean "this device token is dead".
const DEAD_TOKEN_REASONS: [&str; 2] = ["BadDeviceToken", "DeviceTokenNotForTopic"];

/// The reasons that mean "sign a new provider token".
const STALE_JWT_REASONS: [&str; 2] = ["InvalidProviderToken", "ExpiredProviderToken"];

/// The APNs HTTP/2 client.
pub struct ApnsClient {
    http: reqwest::Client,
    token: ProviderToken,
    clock: Arc<dyn Clock>,
    /// `Some` when `APNS_BASE_URL_OVERRIDE` is set: both environments then point at it.
    override_base: Option<Box<str>>,
    backoff: Vec<Duration>,
    immediate_backoff: Vec<Duration>,
    /// Where `retried_total` lands. A client built on its own owns a private set; the notifier
    /// swaps in its own with [`Self::with_counters`] so `healthz` sees the retries.
    counters: Arc<Counters>,
}

impl std::fmt::Debug for ApnsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApnsClient")
            .field("token", &self.token)
            .field("override_base", &self.override_base)
            .field("backoff", &self.backoff)
            .field("immediate_backoff", &self.immediate_backoff)
            .finish()
    }
}

impl ApnsClient {
    /// Builds a client from an already-loaded signing key.
    ///
    /// # Errors
    /// [`ApnsError::KeyFormat`] if the PEM is not an ES256 key, [`ApnsError::MissingKeyId`] /
    /// [`ApnsError::MissingTeamId`] if either identifier is blank, or [`ApnsError::Http`] if the
    /// TLS backend will not start.
    pub fn new(
        key_pem: &[u8],
        key_id: &str,
        team_id: &str,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ApnsError> {
        let token = ProviderToken::new(key_pem, key_id, team_id, Arc::clone(&clock))?;
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| ApnsError::Http(e.to_string().into()))?;
        Ok(Self {
            http,
            token,
            clock,
            override_base: None,
            backoff: DEFAULT_BACKOFF.to_vec(),
            immediate_backoff: IMMEDIATE_BACKOFF.to_vec(),
            counters: Counters::new(),
        })
    }

    /// Builds a client from the `APNS_*` settings, reading `APNS_KEY_FILE` from disk.
    ///
    /// The caller has already decided that APNs is enabled; this does not look at
    /// `APNS_ENABLED`.
    ///
    /// # Errors
    /// [`ApnsError::MissingKeyFile`] when the path is empty, [`ApnsError::KeyFile`] when it cannot
    /// be read, plus everything [`Self::new`] and [`Self::with_base_url`] report.
    pub fn from_config(cfg: &Config, clock: Arc<dyn Clock>) -> Result<Self, ApnsError> {
        let path = cfg
            .apns_key_file
            .as_ref()
            .ok_or(ApnsError::MissingKeyFile)?;
        let pem = std::fs::read(path).map_err(|source| ApnsError::KeyFile {
            path: path.clone(),
            source,
        })?;
        let client = Self::new(&pem, &cfg.apns_key_id, &cfg.apns_team_id, clock)?;
        if cfg.apns_base_url_override.is_empty() {
            Ok(client)
        } else {
            client.with_base_url(&cfg.apns_base_url_override)
        }
    }

    /// Points both environments at one base URL. This is `APNS_BASE_URL_OVERRIDE`, and it exists
    /// for the tests — an operator has no reason to set it.
    ///
    /// # Errors
    /// [`ApnsError::BaseUrl`] if the value does not parse as an absolute URL.
    pub fn with_base_url(mut self, base: &str) -> Result<Self, ApnsError> {
        let trimmed = base.trim_end_matches('/');
        reqwest::Url::parse(trimmed).map_err(|e| ApnsError::BaseUrl(e.to_string().into()))?;
        self.override_base = Some(trimmed.into());
        Ok(self)
    }

    /// Replaces **both** retry ladders with the same delays. Tests use millisecond values;
    /// nothing else calls this.
    ///
    /// Both, deliberately: a suite that shortened only the update ladder would spend
    /// [`IMMEDIATE_BACKOFF`]'s eight minutes on the first alert that met a `503`.
    #[must_use]
    pub fn with_backoff(self, backoff: Vec<Duration>) -> Self {
        self.with_backoff_ladders(backoff.clone(), backoff)
    }

    /// Replaces the two ladders separately — the only way to tell them apart in a test.
    #[must_use]
    pub fn with_backoff_ladders(
        mut self,
        updates: Vec<Duration>,
        immediate: Vec<Duration>,
    ) -> Self {
        self.backoff = updates;
        self.immediate_backoff = immediate;
        self
    }

    /// Points the client's `retried_total` at the notifier's counter set.
    #[must_use]
    pub fn with_counters(mut self, counters: Arc<Counters>) -> Self {
        self.counters = counters;
        self
    }

    /// The counter set this client records retries into. For `healthz` and for the tests.
    #[must_use]
    pub fn counters(&self) -> &Arc<Counters> {
        &self.counters
    }

    /// The ladder one push walks, and how long it may still wait.
    ///
    /// `None` ends the retry loop: either the ladder is spent, or the next delay would land past
    /// the push's own `apns-expiration`, at which point Apple is contractually required to drop
    /// the message anyway. `expiration == 0` means "deliver now or discard" and carries no
    /// deadline to check — that is the Live Activity update, whose ladder is short for other
    /// reasons.
    fn next_delay(&self, push: &Push, retries: usize) -> Option<Duration> {
        let ladder = if push.is_immediate() {
            &self.immediate_backoff
        } else {
            &self.backoff
        };
        let delay = *ladder.get(retries)?;
        if push.expiration != 0 {
            let now = self.clock.now_ms().div_euclid(1_000);
            let secs = i64::try_from(delay.as_secs()).unwrap_or(i64::MAX);
            if now.saturating_add(secs) >= push.expiration {
                return None;
            }
        }
        Some(delay)
    }

    /// The gateway for one device environment, honouring the override.
    #[must_use]
    pub fn base_url(&self, env: ApnsEnvironment) -> &str {
        if let Some(base) = &self.override_base {
            return base;
        }
        match env {
            ApnsEnvironment::Sandbox => SANDBOX_BASE,
            ApnsEnvironment::Production => PRODUCTION_BASE,
        }
    }

    /// The provider token cache, for `healthz` and for tests.
    #[must_use]
    pub const fn provider_token(&self) -> &ProviderToken {
        &self.token
    }

    /// Sends one push to one token, applying the DESIGN §25.5 response policy.
    ///
    /// The ladder is: `200` stops; a dead-token reason stops; a stale-JWT `403` reminds **once**
    /// and retries without consuming a backoff slot; `429`/`5xx`/transport walk
    /// [`DEFAULT_BACKOFF`] — or [`IMMEDIATE_BACKOFF`] for a priority-10 push — and then give up;
    /// anything else is rejected. A retry that would land past the push's own `apns-expiration` is
    /// not taken.
    ///
    /// # Errors
    /// [`ApnsError::Mint`] if the provider token cannot be signed at all — the one condition under
    /// which no request can be made.
    pub async fn send(
        &self,
        push: &Push,
        token: &str,
        env: ApnsEnvironment,
    ) -> Result<Outcome, ApnsError> {
        let url = format!("{}/3/device/{token}", self.base_url(env));
        let mut retries = 0usize;
        let mut reminted = false;

        loop {
            let bearer = self.token.bearer()?;
            match self.attempt(&url, &bearer, push).await {
                Err(transport) => {
                    if let Some(delay) = self.next_delay(push, retries) {
                        retries += 1;
                        self.counters.retried();
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Ok(Outcome::GaveUp {
                        status: None,
                        reason: transport,
                    });
                }
                Ok((status, reason)) => match status {
                    200 => return Ok(Outcome::Delivered),
                    410 => return Ok(Outcome::TokenInvalid { status, reason }),
                    400 if DEAD_TOKEN_REASONS.contains(&&*reason) => {
                        return Ok(Outcome::TokenInvalid { status, reason });
                    }
                    403 if STALE_JWT_REASONS.contains(&&*reason) => {
                        if reminted {
                            return Ok(Outcome::ProviderTokenRejected { reason });
                        }
                        reminted = true;
                        // Compare-and-swap: if a sibling push task already rotated the token,
                        // reuse theirs rather than minting a second one Apple would rate-limit.
                        self.token.remint_if_current(&bearer)?;
                        continue;
                    }
                    429 | 500..=599 => {
                        if let Some(delay) = self.next_delay(push, retries) {
                            retries += 1;
                            self.counters.retried();
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        return Ok(Outcome::GaveUp {
                            status: Some(status),
                            reason,
                        });
                    }
                    _ => return Ok(Outcome::Rejected { status, reason }),
                },
            }
        }
    }

    /// One request. `Ok((status, reason))` for any completed response, `Err(text)` for a
    /// transport failure or a timeout.
    async fn attempt(
        &self,
        url: &str,
        bearer: &str,
        push: &Push,
    ) -> Result<(u16, Box<str>), Box<str>> {
        let mut req = self
            .http
            .post(url)
            .header("authorization", format!("bearer {bearer}"))
            .header("apns-topic", push.topic.as_ref())
            .header("apns-push-type", push.kind.header())
            .header("apns-priority", push.priority.to_string())
            .header("apns-expiration", push.expiration.to_string());
        if let Some(id) = &push.collapse_id {
            req = req.header("apns-collapse-id", id.as_ref());
        }

        let resp = req
            .json(&push.payload)
            .send()
            .await
            // `without_url` is load-bearing, not tidiness: reqwest's `Display` appends
            // " for url (...)" and the URL is `/3/device/<device token>`. That string becomes
            // `Outcome::GaveUp { reason }`, then `healthz`'s `apns.last_error` — which is served
            // outside the auth layer. A DNS or TLS blip must not publish a device token.
            .map_err(|e| Box::<str>::from(e.without_url().to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok((status, reason_of(status, &body)))
    }
}

/// Apple answers a failure with `{"reason": "..."}`; a `200` has an empty body.
///
/// A body that is not the documented shape is kept verbatim (truncated), because the one time
/// this matters is a proxy in front of the gateway answering HTML, and "unexpected body" in the
/// log is useless while the first line of the HTML is not.
fn reason_of(status: u16, body: &str) -> Box<str> {
    if status == 200 {
        return Box::from("");
    }
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(body)
        && let Some(Value::String(reason)) = map.get("reason")
    {
        return Box::from(reason.as_str());
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        Box::from("no reason")
    } else {
        Box::from(&trimmed[..floor_char_boundary(trimmed, REASON_MAX)])
    }
}

/// The largest slice of an undocumented body kept in a log line and in `healthz`.
const REASON_MAX: usize = 200;

/// The largest index `<= max` that `s` may be split at.
///
/// `&s[..s.len().min(200)]` panics when byte 200 lands inside a multi-byte character, and the
/// branch that reaches here exists for a proxy answering HTML — precisely the body most likely to
/// carry a localised quote or an em dash. The panic would unwind inside a spawned push task, so
/// the cost of getting this wrong is not a bad log line but a leaked in-flight slot.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if s.len() <= max {
        return s.len();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_reason_is_read_out_of_the_documented_body_shape() {
        assert_eq!(
            &*reason_of(410, r#"{"reason":"Unregistered"}"#),
            "Unregistered"
        );
        assert_eq!(&*reason_of(200, ""), "");
        assert_eq!(&*reason_of(500, ""), "no reason");
        assert_eq!(
            &*reason_of(502, "<html>bad gateway</html>"),
            "<html>bad gateway</html>"
        );
    }

    #[test]
    fn a_long_body_is_truncated_on_a_character_boundary() {
        // A captive portal or a localised CDN error page is exactly the body that reaches the
        // verbatim branch, and exactly the one with a multi-byte character in it. Slicing at a
        // fixed byte 200 used to panic — inside a spawned push task, which leaked its slot.
        let body = format!("{}\u{20ac} and more", "a".repeat(199));
        let reason = reason_of(502, &body);
        assert_eq!(
            &*reason,
            "a".repeat(199),
            "the euro sign straddles byte 200"
        );
        assert!(reason.len() <= REASON_MAX);

        // A multi-byte character that ends exactly on the boundary is kept whole.
        let body = format!("{}\u{20ac}{}", "a".repeat(197), "b".repeat(50));
        assert_eq!(
            &*reason_of(502, &body),
            format!("{}\u{20ac}", "a".repeat(197))
        );

        // And an all-ASCII overlong body is still cut at exactly 200.
        assert_eq!(reason_of(502, &"z".repeat(500)).len(), REASON_MAX);
        assert_eq!(floor_char_boundary("short", 200), 5);
    }

    #[test]
    fn outcomes_classify_themselves() {
        let invalid = Outcome::TokenInvalid {
            status: 410,
            reason: "Unregistered".into(),
        };
        assert!(invalid.prunes_token());
        assert!(!invalid.delivered());
        assert_eq!(invalid.last_error().as_deref(), Some("410 Unregistered"));
        assert!(Outcome::Delivered.last_error().is_none());
        assert!(
            Outcome::ProviderTokenRejected {
                reason: "ExpiredProviderToken".into()
            }
            .misconfigured()
        );
        assert_eq!(
            Outcome::GaveUp {
                status: Some(503),
                reason: "ServiceUnavailable".into()
            }
            .last_error()
            .as_deref(),
            Some("503 ServiceUnavailable (gave up)")
        );
    }

    #[test]
    fn the_push_type_headers_are_apples_spelling() {
        assert_eq!(PushKind::Alert.header(), "alert");
        assert_eq!(PushKind::LiveActivity.header(), "liveactivity");
    }

    #[test]
    fn the_immediate_priority_is_the_one_the_notifier_stamps() {
        // The ladder is chosen off `apns-priority` alone, so the two constants have to agree or
        // every alert would quietly fall back to the three-delay ladder.
        assert_eq!(IMMEDIATE_PRIORITY, crate::notifier::PRIORITY_IMMEDIATE);
        assert_ne!(crate::notifier::PRIORITY_THROTTLED, IMMEDIATE_PRIORITY);
    }

    #[test]
    fn the_long_ladder_is_taken_only_by_a_priority_ten_push() {
        let push = |priority: u8| Push {
            kind: PushKind::Alert,
            topic: Arc::from("com.tatoalo.aulos"),
            priority,
            expiration: 0,
            collapse_id: None,
            payload: Value::Null,
        };
        assert!(push(10).is_immediate());
        assert!(!push(5).is_immediate());
    }
}
