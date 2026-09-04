//! The Jellyfin library refresh: debounced, capped and optionally targeted (DESIGN §13.1).
//!
//! Legacy called `POST {JELLYFIN_URL}/Library/Refresh` once per completed download
//! (`__sync_jellyfin_library`), so a 500-item playlist asked Jellyfin to rescan every library 500
//! times. Here each completion arms a trailing `AULOS_JELLYFIN_DEBOUNCE_SECS` (30) timer whose
//! fire time is capped at `first_at + AULOS_JELLYFIN_MAX_WAIT_SECS` (300) — the debounce lives in
//! [`crate::dispatcher`], which is also what a community `[[hook]]` uses, so there is one
//! implementation of it rather than two.
//!
//! Three things are byte-compatible with legacy on purpose: the request shape, all **four** error
//! message strings, and the precondition text. The delivery of that last one is the only change:
//! legacy warned once per download forever, which is how a misconfigured deployment stayed broken
//! and quiet.

use std::sync::Arc;
use std::time::Duration;

use aulos_core::config::{Config, JellyfinRefreshMode};
use aulos_core::item::ItemView;
use aulos_core::status::TerminalStatus;
use serde_json::Value;

use crate::error::HookError;
use crate::hook::{Debounce, Hook, HookCtx, HookHealth};

/// `ordering` — last, so the scan sees the re-encoded file and the NFO (DESIGN §13).
pub const ORDERING: i16 = 90;

/// The `healthz` component key and the hook id.
pub const ID: &str = "jellyfin";

/// The precondition message for a blank `JELLYFIN_URL`, byte-identical to legacy.
pub const URL_REQUIRED: &str = "JELLYFIN_URL is required";
/// The precondition message for a blank `JELLYFIN_API_KEY`, byte-identical to legacy.
pub const KEY_REQUIRED: &str = "JELLYFIN_API_KEY is required";

/// The backoff between the three attempts of DESIGN §13.1.
pub const BACKOFF: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(8)];

/// The number of attempts per invocation (DESIGN §13.1).
pub const ATTEMPTS: usize = 3;

/// The non-2xx message shape, byte-identical to legacy.
#[must_use]
pub fn http_failure_message(status: u16, details: &str) -> String {
    format!("Jellyfin refresh failed with HTTP {status}: {details}")
}

/// The transport-failure message shape, byte-identical to legacy.
#[must_use]
pub fn transport_failure_message(err: &str) -> String {
    format!("Jellyfin refresh request failed: {err}")
}

/// Legacy's `details` extraction: the JSON `message` or `Message` field when the body is a JSON
/// object with one, otherwise the body verbatim.
#[must_use]
pub fn details_of(body: &str) -> String {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(body) else {
        return body.to_owned();
    };
    for key in ["message", "Message"] {
        if let Some(Value::String(s)) = map.get(key)
            && !s.is_empty()
        {
            return s.clone();
        }
    }
    body.to_owned()
}

/// The debounced, optionally targeted Jellyfin refresh (DESIGN §13.1).
#[derive(Debug)]
pub struct JellyfinHook {
    client: reqwest::Client,
    base_url: Box<str>,
    api_key: Box<str>,
    library_id: Box<str>,
    metadata_mode: JellyfinRefreshMode,
    image_mode: JellyfinRefreshMode,
    timeout: Duration,
    debounce: Debounce,
    enabled: bool,
    precondition: Option<&'static str>,
    backoff: Vec<Duration>,
}

impl JellyfinHook {
    /// Builds the hook from the effective config, **logging a misconfiguration once, here**.
    ///
    /// With `JELLYFIN_SYNC_ENABLED=true` and a blank URL or key this logs the corresponding
    /// message at WARN exactly once (the constructor runs once, at boot), reports the component
    /// `degraded` with that message as `detail` for the life of the process, and makes
    /// [`Hook::applies`] return `false` so every completion is a silent no-op (DESIGN §13.1).
    #[must_use]
    pub fn new(cfg: &Config) -> Self {
        let base_url = cfg.jellyfin_url.trim().trim_end_matches('/');
        let api_key = cfg.jellyfin_api_key.expose().trim();
        let precondition = if !cfg.jellyfin_sync_enabled {
            None
        } else if base_url.is_empty() {
            Some(URL_REQUIRED)
        } else if api_key.is_empty() {
            Some(KEY_REQUIRED)
        } else {
            None
        };
        if let Some(message) = precondition {
            tracing::warn!(
                component = ID,
                "{message}; Jellyfin sync is enabled but cannot run, so every completion is a no-op"
            );
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped below
        let timeout_ms =
            (cfg.jellyfin_sync_timeout_seconds.max(0.1) * 1000.0).min(600_000.0) as u64;
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            library_id: cfg.jellyfin_library_id.trim().into(),
            metadata_mode: cfg.jellyfin_metadata_refresh_mode,
            image_mode: cfg.jellyfin_image_refresh_mode,
            timeout: Duration::from_millis(timeout_ms),
            debounce: Debounce::capped(
                Duration::from_secs(cfg.jellyfin_debounce_secs),
                Duration::from_secs(cfg.jellyfin_max_wait_secs),
            ),
            enabled: cfg.jellyfin_sync_enabled,
            precondition,
            backoff: BACKOFF.to_vec(),
        }
    }

    /// Replaces the retry backoff. For tests, so the three attempts do not cost 10 real seconds.
    #[must_use]
    pub fn with_backoff(mut self, backoff: &[Duration]) -> Self {
        self.backoff = backoff.to_vec();
        self
    }

    /// The message the constructor logged at boot, if any (DESIGN §13.1).
    #[must_use]
    pub const fn precondition(&self) -> Option<&'static str> {
        self.precondition
    }

    /// The global refresh URL: legacy behaviour, refreshes every library.
    #[must_use]
    pub fn global_url(&self) -> String {
        format!("{}/Library/Refresh", self.base_url)
    }

    /// The targeted refresh URL, when `JELLYFIN_LIBRARY_ID` is set (DESIGN §13.1).
    #[must_use]
    pub fn targeted_url(&self) -> Option<String> {
        if self.library_id.is_empty() {
            return None;
        }
        Some(format!(
            "{}/Items/{}/Refresh?metadataRefreshMode={}&imageRefreshMode={}&replaceAllMetadata=false&replaceAllImages=false",
            self.base_url,
            self.library_id,
            self.metadata_mode.as_str(),
            self.image_mode.as_str(),
        ))
    }

    /// One attempt. `Ok(())` on 2xx.
    async fn attempt(&self, url: &str) -> Result<(), HookError> {
        let response = self
            .client
            .post(url)
            .header("Accept", "application/json")
            .header(
                "Authorization",
                format!("MediaBrowser Token=\"{}\"", self.api_key),
            )
            .timeout(self.timeout)
            .send()
            .await;
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                return Err(HookError::transport(transport_failure_message(
                    &e.to_string(),
                )));
            }
        };
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(HookError::http_status(
            http_failure_message(status.as_u16(), &details_of(&body)),
            status.as_u16(),
        ))
    }

    /// Three attempts with the 2 s / 8 s backoff, then give up until the next completion.
    async fn attempts(
        &self,
        url: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), HookError> {
        let mut last = HookError::other("no attempt was made");
        for i in 0..ATTEMPTS {
            if cancel.is_cancelled() {
                return Err(HookError::Canceled);
            }
            match self.attempt(url).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let retryable = e.retryable();
                    last = e;
                    if !retryable || i + 1 == ATTEMPTS {
                        break;
                    }
                    let wait = self.backoff.get(i).copied().unwrap_or(Duration::ZERO);
                    tracing::warn!(
                        component = ID,
                        attempt = i + 1,
                        error = %last,
                        "retrying the Jellyfin refresh in {wait:?}"
                    );
                    tokio::select! {
                        () = cancel.cancelled() => return Err(HookError::Canceled),
                        () = tokio::time::sleep(wait) => {}
                    }
                }
            }
        }
        Err(last)
    }
}

#[async_trait::async_trait]
impl Hook for JellyfinHook {
    fn id(&self) -> Arc<str> {
        Arc::from(ID)
    }

    fn ordering(&self) -> i16 {
        ORDERING
    }

    fn debounce(&self) -> Debounce {
        self.debounce
    }

    fn timeout(&self) -> Duration {
        // Three attempts plus the backoff between them, with a little slack, so the dispatcher's
        // outer bound never fires before the hook has had its documented three tries.
        let attempts = self
            .timeout
            .saturating_mul(u32::try_from(ATTEMPTS).unwrap_or(3));
        let backoff: Duration = self.backoff.iter().copied().sum();
        attempts + backoff + Duration::from_secs(5)
    }

    /// `finished && JELLYFIN_SYNC_ENABLED`, and false whenever a precondition failed
    /// (DESIGN §13, §13.1).
    fn applies(&self, _item: &ItemView, outcome: TerminalStatus) -> bool {
        self.enabled && self.precondition.is_none() && outcome == TerminalStatus::Finished
    }

    fn health(&self) -> HookHealth {
        match (self.enabled, self.precondition) {
            (false, _) => HookHealth::disabled(),
            (true, Some(reason)) => HookHealth::degraded(reason),
            (true, None) => HookHealth::ok(),
        }
    }

    async fn run(&self, ctx: HookCtx<'_>) -> Result<(), HookError> {
        // Unreachable through the dispatcher, which checks `applies` first; kept because the
        // legacy messages are this function's contract and a direct caller deserves them.
        if let Some(message) = self.precondition {
            return Err(HookError::config(message));
        }
        if !self.enabled {
            return Ok(());
        }

        let items = ctx.count();
        match self.targeted_url() {
            None => {
                self.attempts(&self.global_url(), ctx.cancel).await?;
                tracing::info!(component = ID, items, "refreshed every Jellyfin library");
            }
            Some(url) => match self.attempts(&url, ctx.cancel).await {
                Ok(()) => {
                    tracing::info!(
                        component = ID,
                        items,
                        library = %self.library_id,
                        "refreshed the targeted Jellyfin library"
                    );
                }
                // A mistyped `JELLYFIN_LIBRARY_ID` must not silently disable sync (DESIGN §13.1).
                Err(HookError::Http {
                    status: Some(status),
                    message,
                    ..
                }) if status == 400 || status == 404 => {
                    tracing::warn!(
                        component = ID,
                        library = %self.library_id,
                        status,
                        %message,
                        "the targeted Jellyfin refresh was rejected; JELLYFIN_LIBRARY_ID looks wrong. \
                         Falling back to a global refresh once"
                    );
                    self.attempts(&self.global_url(), ctx.cancel).await?;
                    tracing::info!(
                        component = ID,
                        items,
                        "refreshed every Jellyfin library after the targeted refresh was rejected"
                    );
                }
                Err(e) => return Err(e),
            },
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_core::config::{RawEnv, load};

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let mut env: Vec<(String, String)> = vec![
            ("STATE_DIR".to_owned(), "/tmp".to_owned()),
            ("DOWNLOAD_DIR".to_owned(), "/tmp".to_owned()),
        ];
        for (k, v) in pairs {
            env.push(((*k).to_owned(), (*v).to_owned()));
        }
        load(&RawEnv::from_pairs(env)).expect("the test config must load")
    }

    #[test]
    fn a_blank_url_is_the_first_precondition_and_its_text_is_verbatim() {
        let hook = JellyfinHook::new(&cfg(&[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_API_KEY", ""),
            ("JELLYFIN_URL", ""),
        ]));
        assert_eq!(hook.precondition(), Some("JELLYFIN_URL is required"));
        assert_eq!(
            hook.health(),
            HookHealth::degraded("JELLYFIN_URL is required")
        );
    }

    #[test]
    fn a_blank_key_is_the_second_precondition() {
        let hook = JellyfinHook::new(&cfg(&[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_URL", "http://jf.test"),
            ("JELLYFIN_API_KEY", "   "),
        ]));
        assert_eq!(hook.precondition(), Some("JELLYFIN_API_KEY is required"));
    }

    #[test]
    fn a_disabled_hook_has_no_precondition_and_is_reported_disabled() {
        let hook = JellyfinHook::new(&cfg(&[("JELLYFIN_SYNC_ENABLED", "false")]));
        assert_eq!(hook.precondition(), None);
        assert_eq!(hook.health(), HookHealth::disabled());
        assert_eq!(hook.ordering(), 90);
    }

    #[test]
    fn the_two_urls_match_the_design_exactly() {
        let hook = JellyfinHook::new(&cfg(&[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_URL", "http://jf.test:8096/"),
            ("JELLYFIN_API_KEY", "k"),
        ]));
        assert_eq!(hook.global_url(), "http://jf.test:8096/Library/Refresh");
        assert_eq!(hook.targeted_url(), None);

        let targeted = JellyfinHook::new(&cfg(&[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_URL", "http://jf.test:8096"),
            ("JELLYFIN_API_KEY", "k"),
            ("JELLYFIN_LIBRARY_ID", "abc123"),
            ("JELLYFIN_METADATA_REFRESH_MODE", "FullRefresh"),
            ("JELLYFIN_IMAGE_REFRESH_MODE", "ValidationOnly"),
        ]));
        assert_eq!(
            targeted.targeted_url().unwrap(),
            "http://jf.test:8096/Items/abc123/Refresh?metadataRefreshMode=FullRefresh\
             &imageRefreshMode=ValidationOnly&replaceAllMetadata=false&replaceAllImages=false"
        );
    }

    #[test]
    fn the_debounce_is_the_configured_window_and_cap() {
        let hook = JellyfinHook::new(&cfg(&[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_URL", "http://jf.test"),
            ("JELLYFIN_API_KEY", "k"),
        ]));
        assert_eq!(hook.debounce().window, Duration::from_secs(30));
        assert_eq!(hook.debounce().max_wait, Duration::from_secs(300));

        let tuned = JellyfinHook::new(&cfg(&[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_URL", "http://jf.test"),
            ("JELLYFIN_API_KEY", "k"),
            ("AULOS_JELLYFIN_DEBOUNCE_SECS", "5"),
            ("AULOS_JELLYFIN_MAX_WAIT_SECS", "60"),
        ]));
        assert_eq!(tuned.debounce().window, Duration::from_secs(5));
        assert_eq!(tuned.debounce().max_wait, Duration::from_secs(60));
    }

    #[test]
    fn the_two_message_shapes_are_verbatim() {
        assert_eq!(
            http_failure_message(500, "Internal Server Error"),
            "Jellyfin refresh failed with HTTP 500: Internal Server Error"
        );
        assert_eq!(
            transport_failure_message("connection refused"),
            "Jellyfin refresh request failed: connection refused"
        );
    }

    #[test]
    fn details_prefer_the_json_message_field() {
        assert_eq!(details_of(r#"{"message":"nope"}"#), "nope");
        assert_eq!(details_of(r#"{"Message":"Nope"}"#), "Nope");
        assert_eq!(
            details_of(r#"{"message":"","Message":"Second"}"#),
            "Second",
            "an empty message falls through to Message, as `or` did in Python"
        );
        assert_eq!(details_of("not json"), "not json");
        assert_eq!(details_of(r#"{"other":1}"#), r#"{"other":1}"#);
        assert_eq!(details_of(""), "");
    }

    #[test]
    fn the_outer_timeout_leaves_room_for_all_three_attempts() {
        let hook = JellyfinHook::new(&cfg(&[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_URL", "http://jf.test"),
            ("JELLYFIN_API_KEY", "k"),
            ("JELLYFIN_SYNC_TIMEOUT_SECONDS", "20"),
        ]));
        // 3 × 20 s + 2 s + 8 s + 5 s slack
        assert_eq!(hook.timeout(), Duration::from_secs(75));
    }
}
