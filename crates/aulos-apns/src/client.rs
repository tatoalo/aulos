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
use crate::jwt::ProviderToken;

/// The production gateway.
pub const PRODUCTION_BASE: &str = "https://api.push.apple.com:443";

/// The sandbox gateway, for Debug and simulator builds.
pub const SANDBOX_BASE: &str = "https://api.sandbox.push.apple.com:443";

/// The per-request ceiling. A push is fire-and-forget; nothing may hang on one.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The retry ladder for `429` and `5xx`: three retries, so four requests at worst.
pub const DEFAULT_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(4),
    Duration::from_secs(16),
];

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
    /// `Some` when `APNS_BASE_URL_OVERRIDE` is set: both environments then point at it.
    override_base: Option<Box<str>>,
    backoff: Vec<Duration>,
}

impl std::fmt::Debug for ApnsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApnsClient")
            .field("token", &self.token)
            .field("override_base", &self.override_base)
            .field("backoff", &self.backoff)
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
        let token = ProviderToken::new(key_pem, key_id, team_id, clock)?;
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| ApnsError::Http(e.to_string().into()))?;
        Ok(Self {
            http,
            token,
            override_base: None,
            backoff: DEFAULT_BACKOFF.to_vec(),
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

    /// Replaces the retry ladder. Tests use millisecond delays; nothing else calls this.
    #[must_use]
    pub fn with_backoff(mut self, backoff: Vec<Duration>) -> Self {
        self.backoff = backoff;
        self
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
    /// [`DEFAULT_BACKOFF`] and then give up; anything else is rejected.
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
                    if let Some(delay) = self.backoff.get(retries).copied() {
                        retries += 1;
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
                        self.token.remint()?;
                        continue;
                    }
                    429 | 500..=599 => {
                        if let Some(delay) = self.backoff.get(retries).copied() {
                            retries += 1;
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
            .map_err(|e| Box::<str>::from(e.to_string()))?;
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
        Box::from(&trimmed[..trimmed.len().min(200)])
    }
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
}
