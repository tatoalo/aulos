//! Subscriptions: the persisted record, the wire projection, and the command handle
//! (DESIGN §14.1).
//!
//! All three live in `aulos-core`. The record because `aulos-store` persists it; the view because
//! [`crate::event::DomainEvent::SubscriptionChanged`] carries it; and [`SubscriptionsHandle`]
//! because `aulos-api`'s `ApiState` holds it — putting the handle in `aulos-subscriptions` would
//! add the one wave-1 → wave-2 dependency edge that forces WP-16 to land before WP-14 can compile
//! (DESIGN §3).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};
use url::Url;

use crate::error::{ErrorCode, WireError};
use crate::id::{SubId, UnixMs};
use crate::paths::RelDir;
use crate::request::{SubtitleLang, SubtitleMode};
use crate::selection::Selection;

/// The persisted subscription row. Mirrors the legacy `SubscriptionInfo` dataclass field for
/// field (legacy spec §7.1), plus the three fields that fix its scheduling bugs.
#[derive(Clone, Debug, PartialEq)]
pub struct SubscriptionRecord {
    /// A legacy UUID or a freshly minted ULID.
    pub id: SubId,
    /// From `info.title | channel | playlist_title | uploader | url`.
    pub name: Box<str>,
    /// The feed URL. Trimmed; also the uniqueness key.
    pub url: Url,
    /// A disabled subscription's task parks on a `Notify`.
    pub enabled: bool,
    /// `max(1, n)` on write.
    pub check_interval_minutes: u32,
    /// What to download when a new entry appears.
    pub selection: Selection,
    /// `""` in legacy becomes `None`, so downloads land in the base dir.
    pub folder: Option<RelDir>,
    /// Prepended to the output name.
    pub custom_name_prefix: Box<str>,
    /// Whether queued entries start immediately.
    pub auto_start: bool,
    /// `0` = unlimited.
    pub playlist_item_limit: u32,
    /// Write one file per chapter.
    pub split_by_chapters: bool,
    /// The chapter output template; empty means "use the configured default".
    pub chapter_template: Box<str>,
    /// Subtitle language tag.
    pub subtitle_language: SubtitleLang,
    /// Which subtitle track to prefer.
    pub subtitle_mode: SubtitleMode,
    /// Named preset bundles, applied in order. Legacy's singular key is migrated on import.
    pub ytdl_options_presets: Vec<Box<str>>,
    /// Per-subscription yt-dlp overrides.
    pub ytdl_options_overrides: Map<String, Value>,
    /// Unix ms of the last check. **Always** updated, even on failure — the legacy bug that made
    /// a broken feed re-extract every 60 s forever was that it was not.
    pub last_checked: Option<UnixMs>,
    /// Unix ms of the next scheduled check. Persisted, so a restart keeps the schedule.
    pub next_due: Option<UnixMs>,
    /// Drives the exponential backoff. Persisted.
    pub consecutive_failures: u32,
    /// The last error text, or `None`.
    pub error: Option<Box<str>>,
    /// How many media ids this subscription has seen. Denormalised from `subscription_seen`.
    pub seen_count: u32,
}

impl SubscriptionRecord {
    /// A record with legacy defaults for everything but identity, url and selection.
    #[must_use]
    pub fn new(id: SubId, name: impl Into<Box<str>>, url: Url, selection: Selection) -> Self {
        Self {
            id,
            name: name.into(),
            url,
            enabled: true,
            check_interval_minutes: 60,
            selection,
            folder: None,
            custom_name_prefix: "".into(),
            auto_start: true,
            playlist_item_limit: 0,
            split_by_chapters: false,
            chapter_template: "".into(),
            subtitle_language: SubtitleLang::english(),
            subtitle_mode: SubtitleMode::PreferManual,
            ytdl_options_presets: Vec::new(),
            ytdl_options_overrides: Map::new(),
            last_checked: None,
            next_due: None,
            consecutive_failures: 0,
            error: None,
            seen_count: 0,
        }
    }

    /// The v2 wire projection: the legacy 13 keys plus the three additive ones.
    ///
    /// `checking` is transient (it is not in the row), so the caller passes it.
    #[must_use]
    pub fn to_view(&self, checking: bool) -> SubscriptionView {
        SubscriptionView {
            id: self.id.clone(),
            name: Arc::from(&*self.name),
            url: Arc::from(self.url.as_str()),
            enabled: self.enabled,
            check_interval_minutes: self.check_interval_minutes,
            download_type: self.selection.download_type,
            codec: self.selection.codec,
            format: self.selection.format.as_arc(),
            quality: self.selection.quality.as_arc(),
            folder: self
                .folder
                .as_ref()
                .map_or_else(|| Arc::from(""), |f| Arc::from(f.as_str())),
            last_checked: self.last_checked,
            seen_count: self.seen_count,
            error: self.error.as_ref().map(|e| Arc::from(&**e)),
            next_due: self.next_due,
            consecutive_failures: self.consecutive_failures,
            checking,
        }
    }
}

/// `subscription` on the wire (DESIGN §14.1, PROTOCOL §5.9, §9).
///
/// The legacy `to_public_dict()` 13 keys **plus** `next_due`, `consecutive_failures` and
/// `checking`. Secrets and knobs (`custom_name_prefix`, `ytdl_options_*`, the seen set) stay
/// unexposed, as in legacy. The v1 shim emits exactly the legacy 13 and nothing more.
///
/// `last_checked` and `next_due` are **milliseconds** here; the v1 shim divides `last_checked` by
/// 1000 and emits a float, matching legacy's `time.time()`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct SubscriptionView {
    /// The subscription id.
    pub id: SubId,
    /// Display name.
    pub name: Arc<str>,
    /// The feed URL.
    pub url: Arc<str>,
    /// Whether the scheduler runs it.
    pub enabled: bool,
    /// Check interval in minutes.
    pub check_interval_minutes: u32,
    /// What to download.
    pub download_type: crate::selection::DownloadType,
    /// The codec preference.
    pub codec: crate::selection::Codec,
    /// The catalog format id.
    pub format: Arc<str>,
    /// The catalog quality id.
    pub quality: Arc<str>,
    /// `""` for the base dir — legacy emitted the empty string, not `null`.
    pub folder: Arc<str>,
    /// Unix **ms** of the last check, or `null`.
    pub last_checked: Option<UnixMs>,
    /// How many media ids have been seen.
    pub seen_count: u32,
    /// The last error text, or `null`.
    pub error: Option<Arc<str>>,
    /// Unix **ms** of the next scheduled check, or `null`. Additive in v2.
    pub next_due: Option<UnixMs>,
    /// Backoff counter. Additive in v2.
    pub consecutive_failures: u32,
    /// Whether a check is running right now. Additive in v2; never persisted.
    pub checking: bool,
}

impl SubscriptionView {
    /// The legacy 13 keys, in `to_public_dict()` order — what the v1 shim emits.
    pub const V1_KEYS: [&'static str; 13] = [
        "id",
        "name",
        "url",
        "enabled",
        "check_interval_minutes",
        "download_type",
        "codec",
        "format",
        "quality",
        "folder",
        "last_checked",
        "seen_count",
        "error",
    ];

    /// The three keys v2 adds.
    pub const V2_ADDITIONAL_KEYS: [&'static str; 3] =
        ["next_due", "consecutive_failures", "checking"];
}

/// What can be changed by `POST <p>subscriptions/update`.
///
/// Legacy accepted only these three fields, and so do we — but a bad `enabled` is a
/// `400 validation_failed` rather than a leaked 500 (DESIGN §14.3 step 10).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct SubChanges {
    /// Enable or disable the schedule.
    pub enabled: Option<bool>,
    /// New interval in minutes; clamped to `max(1, n)`.
    pub check_interval_minutes: Option<u32>,
    /// New display name.
    pub name: Option<Box<str>>,
}

impl SubChanges {
    /// Whether this patch would change anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none() && self.check_interval_minutes.is_none() && self.name.is_none()
    }
}

/// The handle returned by `POST <p>subscriptions/check`, so the route answers immediately while
/// the checks run in the background (BRIEF §12).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CheckJob {
    /// An opaque job id.
    pub job_id: Box<str>,
    /// Which subscriptions this job will check.
    pub subscriptions: Vec<SubId>,
}

/// Aggregate subscription health, for `healthz.components.subscriptions` (DESIGN §16.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct SubsHealth {
    /// How many subscriptions exist.
    pub total: u32,
    /// How many have `consecutive_failures > 0`.
    pub failing: u32,
    /// Seconds until the next scheduled check, or `null` when nothing is scheduled.
    pub next_due_in_s: Option<i64>,
}

/// What the manager accepts. `aulos-subscriptions::Manager` owns the receiving half.
#[derive(Debug)]
#[non_exhaustive]
pub enum SubCmd {
    /// Create a subscription from a feed URL, resolving its name and suppressing the backfill.
    Add {
        /// The feed URL, already trimmed.
        url: Box<str>,
        /// The selection new entries are queued with.
        selection: Box<Selection>,
        /// Optional custom directory.
        folder: Option<RelDir>,
        /// Answered with the created record's view.
        ack: oneshot::Sender<Result<Box<SubscriptionView>, SubError>>,
    },
    /// Apply [`SubChanges`] to one subscription.
    Update {
        /// Which one.
        id: SubId,
        /// What to change.
        changes: Box<SubChanges>,
        /// Answered with the updated view.
        ack: oneshot::Sender<Result<Box<SubscriptionView>, SubError>>,
    },
    /// Delete subscriptions.
    Delete {
        /// Which ones.
        ids: Vec<SubId>,
        /// Answered with the ids that actually existed.
        ack: oneshot::Sender<Result<Vec<SubId>, SubError>>,
    },
    /// Check now. Returns a job handle immediately.
    Check {
        /// Which ones, or every enabled subscription when empty.
        ids: Vec<SubId>,
        /// Answered with the job handle.
        ack: oneshot::Sender<Result<CheckJob, SubError>>,
    },
    /// List every subscription's view.
    List {
        /// Answered with the current projections.
        ack: oneshot::Sender<Result<Vec<SubscriptionView>, SubError>>,
    },
    /// Aggregate health, for `healthz`.
    Health {
        /// Answered with the aggregate.
        ack: oneshot::Sender<SubsHealth>,
    },
}

/// A cheap clone over an `mpsc::Sender<SubCmd>` with **no logic** (DESIGN §14.1).
///
/// This is the whole reason `aulos-api` does not depend on `aulos-subscriptions`.
#[derive(Clone, Debug)]
pub struct SubscriptionsHandle(mpsc::Sender<SubCmd>);

impl SubscriptionsHandle {
    /// Wraps the sending half of the manager's command channel.
    #[must_use]
    pub const fn new(tx: mpsc::Sender<SubCmd>) -> Self {
        Self(tx)
    }

    /// Creates the channel and returns both halves, so wiring is one call.
    #[must_use]
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<SubCmd>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (Self(tx), rx)
    }

    /// Sends a command.
    ///
    /// # Errors
    /// [`SubError::Unavailable`] when the manager task is gone.
    pub async fn send(&self, cmd: SubCmd) -> Result<(), SubError> {
        self.0.send(cmd).await.map_err(|_| SubError::Unavailable)
    }

    /// Whether the manager is still running.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.0.is_closed()
    }
}

/// Subscription failures, including the legacy strings that must stay byte-identical.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum SubError {
    /// Legacy: `Missing URL`.
    #[error("{}", legacy::MISSING_URL)]
    MissingUrl,
    /// Legacy: `This URL is already subscribed`.
    #[error("{}", legacy::ALREADY_SUBSCRIBED)]
    AlreadySubscribed,
    /// Legacy: `Could not resolve URL`.
    #[error("{}", legacy::COULD_NOT_RESOLVE)]
    CouldNotResolve,
    /// Legacy: `This URL points to a single video, not a channel or playlist. Use Download
    /// instead.`
    #[error("{}", legacy::VIDEO_ONLY)]
    VideoOnly,
    /// No such subscription.
    #[error("no such subscription: {0}")]
    NotFound(SubId),
    /// A field-level validation failure.
    #[error("{message}")]
    Invalid {
        /// Which field.
        field: Box<str>,
        /// The message to show.
        message: Box<str>,
    },
    /// The manager task is gone.
    #[error("the subscription manager is unavailable")]
    Unavailable,
    /// Anything else, with the provider or store message attached.
    #[error("{0}")]
    Other(Box<str>),
}

impl SubError {
    /// The wire error code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::MissingUrl | Self::VideoOnly | Self::CouldNotResolve => {
                ErrorCode::ValidationFailed
            }
            Self::AlreadySubscribed => ErrorCode::Conflict,
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::Invalid { .. } => ErrorCode::ValidationFailed,
            Self::Unavailable => ErrorCode::StateUnavailable,
            Self::Other(_) => ErrorCode::Internal,
        }
    }

    /// Only an unavailable manager is worth another try.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::Unavailable)
    }

    /// The error as a wire envelope body.
    #[must_use]
    pub fn to_wire(&self) -> WireError {
        match self {
            Self::Invalid { field, message } => WireError::field(self.code(), &**field, &**message),
            other => WireError::new(other.code(), other.to_string()),
        }
    }
}

/// The legacy subscription strings, byte-identical (legacy spec §7.4, DESIGN §14.3 step 9).
pub mod legacy {
    /// `Missing URL`
    pub const MISSING_URL: &str = "Missing URL";
    /// `This URL is already subscribed`
    pub const ALREADY_SUBSCRIBED: &str = "This URL is already subscribed";
    /// `Could not resolve URL`
    pub const COULD_NOT_RESOLVE: &str = "Could not resolve URL";
    /// `This URL points to a single video, not a channel or playlist. Use Download instead.`
    pub const VIDEO_ONLY: &str =
        "This URL points to a single video, not a channel or playlist. Use Download instead.";
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::selection::{Codec, DownloadType, FormatId, QualityId};

    fn record() -> SubscriptionRecord {
        SubscriptionRecord::new(
            SubId::parse("9c1f2d84-1c6e-4a1b-9f0e-2b7a1c3d4e5f").unwrap(),
            "Veritasium",
            Url::parse("https://www.youtube.com/@veritasium").unwrap(),
            Selection::new(
                DownloadType::Video,
                Codec::Auto,
                FormatId::parse("any").unwrap(),
                QualityId::parse("best").unwrap(),
            ),
        )
    }

    #[test]
    fn the_view_carries_the_legacy_thirteen_plus_three() {
        let mut r = record();
        r.last_checked = Some(1_757_000_100_000);
        r.next_due = Some(1_757_003_700_000);
        r.seen_count = 314;
        let v = serde_json::to_value(r.to_view(false)).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 16);
        for k in SubscriptionView::V1_KEYS {
            assert!(obj.contains_key(k), "{k} missing");
        }
        for k in SubscriptionView::V2_ADDITIONAL_KEYS {
            assert!(obj.contains_key(k), "{k} missing");
        }
        assert_eq!(obj["folder"], "", "legacy emitted the empty string");
        assert_eq!(obj["last_checked"], 1_757_000_100_000_i64);
        assert_eq!(obj["seen_count"], 314);
        assert_eq!(obj["checking"], false);
        assert!(obj["error"].is_null());
    }

    #[test]
    fn the_view_never_exposes_secrets_or_knobs() {
        let mut r = record();
        r.custom_name_prefix = "prefix".into();
        r.ytdl_options_presets = vec!["archive".into()];
        r.ytdl_options_overrides
            .insert("cookiefile".to_owned(), Value::String("/x".to_owned()));
        let v = serde_json::to_value(r.to_view(true)).unwrap();
        let obj = v.as_object().unwrap();
        for leaked in [
            "custom_name_prefix",
            "ytdl_options_presets",
            "ytdl_options_overrides",
            "seen_ids",
            "auto_start",
        ] {
            assert!(!obj.contains_key(leaked), "{leaked} must stay unexposed");
        }
        assert_eq!(obj["checking"], true);
    }

    #[test]
    fn a_folder_projects_as_a_plain_string() {
        let mut r = record();
        r.folder = Some(RelDir::parse("Shows").unwrap());
        let v = serde_json::to_value(r.to_view(false)).unwrap();
        assert_eq!(v["folder"], "Shows");
    }

    #[test]
    fn legacy_error_strings_are_byte_identical() {
        assert_eq!(SubError::MissingUrl.to_string(), "Missing URL");
        assert_eq!(
            SubError::AlreadySubscribed.to_string(),
            "This URL is already subscribed"
        );
        assert_eq!(
            SubError::CouldNotResolve.to_string(),
            "Could not resolve URL"
        );
        assert_eq!(
            SubError::VideoOnly.to_string(),
            "This URL points to a single video, not a channel or playlist. Use Download instead."
        );
        assert_eq!(SubError::AlreadySubscribed.code(), ErrorCode::Conflict);
        let w = SubError::Invalid {
            field: "enabled".into(),
            message: "enabled must be a boolean".into(),
        }
        .to_wire();
        assert_eq!(w.field.as_deref(), Some("enabled"));
        assert_eq!(w.code, ErrorCode::ValidationFailed);
    }

    #[tokio::test]
    async fn the_handle_is_a_logic_free_sender() {
        let (handle, mut rx) = SubscriptionsHandle::channel(4);
        assert!(handle.is_open());
        let (ack, got) = oneshot::channel();
        handle.send(SubCmd::Health { ack }).await.unwrap();
        match rx.recv().await {
            Some(SubCmd::Health { ack }) => {
                ack.send(SubsHealth {
                    total: 7,
                    failing: 1,
                    next_due_in_s: Some(412),
                })
                .unwrap();
            }
            other => panic!("unexpected {other:?}"),
        }
        let h = got.await.unwrap();
        assert_eq!((h.total, h.failing, h.next_due_in_s), (7, 1, Some(412)));

        drop(rx);
        assert!(!handle.is_open());
        let (ack, _) = oneshot::channel();
        assert_eq!(
            handle.send(SubCmd::Health { ack }).await.unwrap_err(),
            SubError::Unavailable
        );
    }

    #[test]
    fn sub_changes_only_accepts_the_legacy_three() {
        let c: SubChanges =
            serde_json::from_str(r#"{"enabled":false,"name":"x","folder":"nope"}"#).unwrap();
        assert_eq!(c.enabled, Some(false));
        assert_eq!(c.name.as_deref(), Some("x"));
        assert_eq!(c.check_interval_minutes, None);
        assert!(SubChanges::default().is_empty());
    }
}
