//! The Jellyfin library scan: debounced, capped, and — as of 2026-09-06 — actually able to
//! discover a new file (DESIGN §13.1).
//!
//! # The production bug this file was rewritten for
//!
//! Between the metube cutover and 2026-09-06, a set `JELLYFIN_LIBRARY_ID` made this hook call
//! `POST /Items/{id}/Refresh`. That endpoint **refreshes metadata for an item Jellyfin already
//! has**; a file that has just landed on disk has no item yet, so there is nothing for it to
//! refresh. It answers `204 No Content` — a success status for a no-op — so the fallback, which
//! was gated on the call being *rejected*, never fired, `failures_total` stayed 0, and the log
//! line said "refreshed the targeted Jellyfin library". No aulos-era download was ever indexed.
//!
//! `docs/reference/jellyfin-refresh-experiment.md` measures every candidate against Jellyfin
//! 10.10.7 and 12.0.0. The short version:
//!
//! | Mechanism | Result on both versions |
//! |---|---|
//! | `POST /Items/{id}/Refresh` (± `&recursive=true`) | `204`, **never** indexed |
//! | `POST /Library/Refresh` | `204`, indexed in ~1 s |
//! | `POST /Library/Media/Updated` with a path Jellyfin knows | `204`, indexed after the server's `LibraryMonitorDelay` (60 s default) |
//! | `POST /Library/Media/Updated` with a path it does not know | `204`, nothing |
//!
//! Neither version has a per-library scan endpoint at all: `/Library/Refresh` is the only
//! operation whose OpenAPI summary is "Starts a library scan", and it takes no parameters. So:
//!
//! - **The default is the global scan**, exactly as legacy `jellyfin_sync.py` did it.
//! - **`JELLYFIN_LIBRARY_ID` cannot scope discovery.** It is still accepted, and it now warns once
//!   at boot and scans globally regardless, rather than quietly selecting a broken path.
//! - **`JELLYFIN_PATH_MAP` is the opt-in targeted mode**, because `Library/Media/Updated` is
//!   addressed by path *as Jellyfin sees it*, which is not the path aulos wrote to.
//!
//! Note the shape of the trap in the last table row: the targeted call is silent when the map is
//! wrong, in precisely the way the old code was. Hence a fallback that fires on an uncovered path
//! as well as on a bad status, and a `healthz` detail carrying `mode` and `last_status` so a
//! deployment can be checked without a stopwatch and a Jellyfin login.
//!
//! Three things stay byte-compatible with legacy on purpose: the request shape of the global
//! scan, all **four** error message strings, and the precondition text.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aulos_core::config::{Config, JellyfinPathMap};
use aulos_core::id::UnixMs;
use aulos_core::item::ItemView;
use aulos_core::status::TerminalStatus;
use serde_json::Value;

use crate::error::HookError;
use crate::hook::{Debounce, Hook, HookCtx, HookHealth, SkipReason};

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

/// The boot WARN for a set `JELLYFIN_LIBRARY_ID`.
///
/// It is one line rather than a silent demotion because the variable is in the field: the
/// deployment that hit the production bug set it, and the operator has to be told that the value
/// is inert rather than left to assume the scan is scoped.
pub const LIBRARY_ID_INERT: &str = "JELLYFIN_LIBRARY_ID is set, but Jellyfin has no per-library \
     scan endpoint — only POST /Library/Refresh discovers new files, and it takes no parameters. \
     The id is ignored and every completion requests a global scan. For a genuinely targeted \
     scan set JELLYFIN_PATH_MAP instead";

/// What the hook asked Jellyfin to do — the `mode` field of `healthz.components.jellyfin`.
///
/// There is deliberately no `library_scan` variant: no Jellyfin version exposes a scan that can be
/// scoped to one library (`docs/reference/jellyfin-refresh-experiment.md` §2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScanMode {
    /// `POST /Library/Refresh` — the legacy request, and the only one proven to discover a new
    /// file promptly.
    GlobalScan,
    /// `POST /Library/Media/Updated` with mapped paths — targeted, opt-in via `JELLYFIN_PATH_MAP`.
    MediaUpdated,
}

impl ScanMode {
    /// The `healthz` spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GlobalScan => "global_scan",
            Self::MediaUpdated => "media_updated",
        }
    }
}

/// What the last request was and how it went, for `healthz` (DESIGN §13.1, §16.3).
#[derive(Clone, Copy, Debug)]
struct LastRequest {
    mode: ScanMode,
    at_ms: UnixMs,
    /// The HTTP status, or `None` when the request never got one (a transport failure).
    status: Option<u16>,
}

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

/// The debounced Jellyfin library scan, with the opt-in targeted mode (DESIGN §13.1).
#[derive(Debug)]
pub struct JellyfinHook {
    client: reqwest::Client,
    base_url: Box<str>,
    api_key: Box<str>,
    library_id: Box<str>,
    path_map: JellyfinPathMap,
    timeout: Duration,
    debounce: Debounce,
    enabled: bool,
    precondition: Option<&'static str>,
    backoff: Vec<Duration>,
    /// What the last request was and how it went. `None` until the first one.
    last: Mutex<Option<LastRequest>>,
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
        let library_id: Box<str> = cfg.jellyfin_library_id.trim().into();
        // Once, at boot: the variable is accepted but cannot do what its name suggests. Saying so
        // here is the whole difference between this and the bug — the old code honoured it
        // silently and produced a 204 that meant nothing.
        if cfg.jellyfin_sync_enabled && precondition.is_none() && !library_id.is_empty() {
            tracing::warn!(component = ID, library = %library_id, "{LIBRARY_ID_INERT}");
        }
        let path_map = cfg.jellyfin_path_map.clone();
        if cfg.jellyfin_sync_enabled && precondition.is_none() && !path_map.is_empty() {
            tracing::info!(
                component = ID,
                pairs = path_map.len(),
                "JELLYFIN_PATH_MAP is set; completions whose paths it covers request a targeted \
                 scan via POST /Library/Media/Updated, and anything it does not cover falls back \
                 to a global scan"
            );
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped below
        let timeout_ms =
            (cfg.jellyfin_sync_timeout_seconds.max(0.1) * 1000.0).min(600_000.0) as u64;
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            library_id,
            path_map,
            timeout: Duration::from_millis(timeout_ms),
            debounce: Debounce::capped(
                Duration::from_secs(cfg.jellyfin_debounce_secs),
                Duration::from_secs(cfg.jellyfin_max_wait_secs),
            ),
            enabled: cfg.jellyfin_sync_enabled,
            precondition,
            backoff: BACKOFF.to_vec(),
            last: Mutex::new(None),
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

    /// The global scan URL — `POST /Library/Refresh`, byte-identical to legacy.
    ///
    /// The only endpoint on any tested Jellyfin that discovers a file it has never seen.
    #[must_use]
    pub fn global_url(&self) -> String {
        format!("{}/Library/Refresh", self.base_url)
    }

    /// The targeted-notification URL — `POST /Library/Media/Updated`.
    #[must_use]
    pub fn media_updated_url(&self) -> String {
        format!("{}/Library/Media/Updated", self.base_url)
    }

    /// The configured path map, empty when the targeted mode is off.
    #[must_use]
    pub const fn path_map(&self) -> &JellyfinPathMap {
        &self.path_map
    }

    /// The mode this hook will ask for when every produced path is covered by the map.
    #[must_use]
    pub fn configured_mode(&self) -> ScanMode {
        if self.path_map.is_empty() {
            ScanMode::GlobalScan
        } else {
            ScanMode::MediaUpdated
        }
    }

    /// The absolute paths this invocation's batch produced, in batch order.
    ///
    /// Every entry, not just the representative item's: a debounced batch is the whole point, and
    /// a 50-item playlist must notify Jellyfin about 50 files.
    fn produced_paths(ctx: &HookCtx<'_>) -> Vec<String> {
        ctx.batch
            .iter()
            .filter_map(|e| {
                let root = ctx.cfg.paths.root_for(e.download_type);
                crate::dispatcher::file_path(root, e.filename.as_deref())
            })
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    /// The `Library/Media/Updated` body for `paths`, all `UpdateType: "Created"`.
    #[must_use]
    pub fn media_updated_body(paths: &[String]) -> String {
        let updates: Vec<Value> = paths
            .iter()
            .map(|p| serde_json::json!({ "Path": p, "UpdateType": "Created" }))
            .collect();
        serde_json::json!({ "Updates": updates }).to_string()
    }

    /// Records what was just asked of Jellyfin, for `healthz`.
    fn record(&self, mode: ScanMode, at_ms: UnixMs, status: Option<u16>) {
        if let Ok(mut last) = self.last.lock() {
            *last = Some(LastRequest {
                mode,
                at_ms,
                status,
            });
        }
    }

    /// One attempt. `Ok(status)` on 2xx, so the caller can record the exact code.
    async fn attempt(&self, url: &str, body: Option<&str>) -> Result<u16, HookError> {
        let mut req = self
            .client
            .post(url)
            .header("Accept", "application/json")
            .header(
                "Authorization",
                format!("MediaBrowser Token=\"{}\"", self.api_key),
            )
            .timeout(self.timeout);
        if let Some(body) = body {
            req = req
                .header("Content-Type", "application/json")
                .body(body.to_owned());
        }
        let response = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                return Err(HookError::transport(transport_failure_message(
                    &e.to_string(),
                )));
            }
        };
        let status = response.status();
        if status.is_success() {
            return Ok(status.as_u16());
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
        body: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<u16, HookError> {
        let mut last = HookError::other("no attempt was made");
        for i in 0..ATTEMPTS {
            if cancel.is_cancelled() {
                return Err(HookError::Canceled);
            }
            match self.attempt(url, body).await {
                Ok(status) => return Ok(status),
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
        // outer bound never fires before the hook has had its documented three tries. With the
        // targeted mode armed there can be two such rounds — the notification, then the global
        // scan it falls back to — and a budget for only one would cut the fallback off, which is
        // the failure the whole rewrite exists to prevent.
        let attempts = self
            .timeout
            .saturating_mul(u32::try_from(ATTEMPTS).unwrap_or(3));
        let backoff: Duration = self.backoff.iter().copied().sum();
        let round = attempts + backoff;
        let rounds = if self.path_map.is_empty() { 1 } else { 2 };
        round.saturating_mul(rounds) + Duration::from_secs(5)
    }

    /// `finished && JELLYFIN_SYNC_ENABLED`, and false whenever a precondition failed
    /// (DESIGN §13, §13.1).
    fn applies(&self, item: &ItemView, outcome: TerminalStatus) -> bool {
        self.skip_reason(item, outcome).is_none()
    }

    fn skip_reason(&self, _item: &ItemView, outcome: TerminalStatus) -> Option<SkipReason> {
        if !self.enabled {
            return Some(SkipReason::new("JELLYFIN_SYNC_ENABLED is false"));
        }
        if let Some(message) = self.precondition {
            return Some(SkipReason::owned(message));
        }
        if outcome != TerminalStatus::Finished {
            return Some(SkipReason::owned(format!(
                "the outcome is {outcome}, not finished"
            )));
        }
        None
    }

    /// The component detail. `mode` is the mode the **last** request used, or the configured one
    /// before there has been a request; `last_status` is the HTTP code Jellyfin answered with, and
    /// is `null` when the request never got one.
    ///
    /// These three exist because of the production bug: a hook that asked the wrong endpoint and a
    /// hook that asked the right one reported byte-identical health. `mode` and `last_status`
    /// together say what was actually requested and what came back, which is checkable from
    /// `/healthz` alone.
    fn health(&self) -> HookHealth {
        let base = match (self.enabled, self.precondition) {
            (false, _) => return HookHealth::disabled(),
            (true, Some(reason)) => HookHealth::degraded(reason),
            (true, None) => HookHealth::ok(),
        };
        let last = self.last.lock().ok().and_then(|l| *l);
        let base = base.with(
            "mode",
            last.map_or(self.configured_mode(), |l| l.mode).as_str(),
        );
        let base = if self.library_id.is_empty() {
            base
        } else {
            // Visible in `/healthz`, not just in a boot line nobody scrolls back to.
            base.with("library_id_ignored", true)
        };
        match last {
            None => base,
            Some(l) => base
                .with("last_request_at", l.at_ms)
                .with("last_status", l.status.map_or(Value::Null, Value::from)),
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

        // The opt-in targeted mode. It is tried only when the map covers **every** path this
        // batch produced: a global scan is a superset of what the notification would have asked
        // for, so a partially-mapped batch is served correctly and more cheaply by one global
        // scan than by a notification that would miss some of it.
        if !self.path_map.is_empty() {
            let paths = Self::produced_paths(&ctx);
            let mapped: Option<Vec<String>> = paths.iter().map(|p| self.path_map.map(p)).collect();
            match mapped {
                Some(mapped) if !mapped.is_empty() => {
                    let body = Self::media_updated_body(&mapped);
                    match self
                        .attempts(&self.media_updated_url(), Some(&body), ctx.cancel)
                        .await
                    {
                        Ok(status) => {
                            self.record(ScanMode::MediaUpdated, ctx.clock.now_ms(), Some(status));
                            tracing::info!(
                                component = ID,
                                items,
                                paths = mapped.len(),
                                status,
                                "requested a Jellyfin library scan (paths={} via Media/Updated)",
                                mapped.len()
                            );
                            return Ok(());
                        }
                        // Canceled means shutdown, not a Jellyfin problem: retrying it against
                        // another endpoint would just be a second cancelled request.
                        Err(HookError::Canceled) => return Err(HookError::Canceled),
                        Err(e) => {
                            // Never gated on "rejected": ANY failure falls through to the scan
                            // that is proven to work. The old code's fallback fired on 400/404
                            // only, and 204 was the answer it actually got.
                            tracing::warn!(
                                component = ID,
                                error = %e,
                                "the targeted Jellyfin notification failed; falling back to a \
                                 global library scan"
                            );
                        }
                    }
                }
                Some(_) => {
                    tracing::warn!(
                        component = ID,
                        items,
                        "this batch produced no file path to notify Jellyfin about; falling back \
                         to a global library scan"
                    );
                }
                None => {
                    let example = paths
                        .iter()
                        .find(|p| self.path_map.map(p).is_none())
                        .map_or("", String::as_str);
                    tracing::warn!(
                        component = ID,
                        items,
                        path = example,
                        "JELLYFIN_PATH_MAP does not cover this path, so Jellyfin would not \
                         recognise it; falling back to a global library scan"
                    );
                }
            }
        }

        let status = match self.attempts(&self.global_url(), None, ctx.cancel).await {
            Ok(status) => status,
            Err(e) => {
                // A transport failure has no status; a non-2xx does. Either way the attempt is
                // recorded, so `healthz` never shows a stale success for a request that failed.
                self.record(ScanMode::GlobalScan, ctx.clock.now_ms(), e.status());
                return Err(e);
            }
        };
        self.record(ScanMode::GlobalScan, ctx.clock.now_ms(), Some(status));
        // "requested", not "refreshed": all this call establishes is that Jellyfin accepted the
        // request. Whether the scan then found the file is Jellyfin's business, and claiming
        // otherwise is what kept the production bug invisible for two days.
        tracing::info!(
            component = ID,
            items,
            status,
            "requested a Jellyfin library scan (global)"
        );
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_core::config::{RawEnv, load};
    use aulos_core::health::ComponentStatus;

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
        // Updated 2026-09-06: `health()` now also carries `mode`, so compare the status and the
        // reason rather than the whole struct.
        let health = hook.health();
        assert_eq!(health.status, ComponentStatus::Degraded);
        assert_eq!(health.detail["detail"], "JELLYFIN_URL is required");
        assert_eq!(health.detail["mode"], "global_scan");
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
    fn the_scan_url_is_the_legacy_one_and_a_library_id_does_not_change_it() {
        let hook = JellyfinHook::new(&cfg(&[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_URL", "http://jf.test:8096/"),
            ("JELLYFIN_API_KEY", "k"),
        ]));
        assert_eq!(hook.global_url(), "http://jf.test:8096/Library/Refresh");
        assert_eq!(
            hook.media_updated_url(),
            "http://jf.test:8096/Library/Media/Updated"
        );
        assert_eq!(hook.configured_mode(), ScanMode::GlobalScan);

        // The regression: a set library id used to select `Items/{id}/Refresh`, which cannot
        // discover a new file. It must not change the request at all now.
        let with_id = JellyfinHook::new(&cfg(&[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_URL", "http://jf.test:8096"),
            ("JELLYFIN_API_KEY", "k"),
            ("JELLYFIN_LIBRARY_ID", "ca4fc2dadb00fcd7e929d2d0a49151b8"),
            ("JELLYFIN_METADATA_REFRESH_MODE", "FullRefresh"),
            ("JELLYFIN_IMAGE_REFRESH_MODE", "ValidationOnly"),
        ]));
        assert_eq!(with_id.global_url(), "http://jf.test:8096/Library/Refresh");
        assert_eq!(with_id.configured_mode(), ScanMode::GlobalScan);
        assert_eq!(
            with_id.health().detail["library_id_ignored"],
            true,
            "healthz says the id is inert, not just the boot log"
        );
    }

    #[test]
    fn a_path_map_arms_the_targeted_mode() {
        let hook = JellyfinHook::new(&cfg(&[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_URL", "http://jf.test:8096"),
            ("JELLYFIN_API_KEY", "k"),
            ("JELLYFIN_PATH_MAP", "/downloads=/data/videos"),
        ]));
        assert_eq!(hook.configured_mode(), ScanMode::MediaUpdated);
        assert_eq!(hook.health().detail["mode"], "media_updated");
        assert!(
            !hook.health().detail.contains_key("last_request_at"),
            "nothing was requested yet, so nothing is claimed"
        );
        assert_eq!(
            hook.path_map().map("/downloads/tube/a.mp4").as_deref(),
            Some("/data/videos/tube/a.mp4")
        );
    }

    #[test]
    fn the_media_updated_body_is_the_documented_shape() {
        let body = JellyfinHook::media_updated_body(&[
            "/data/videos/a.mp4".to_owned(),
            "/data/videos/b.mp4".to_owned(),
        ]);
        assert_eq!(
            body,
            r#"{"Updates":[{"Path":"/data/videos/a.mp4","UpdateType":"Created"},"#.to_owned()
                + r#"{"Path":"/data/videos/b.mp4","UpdateType":"Created"}]}"#
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
