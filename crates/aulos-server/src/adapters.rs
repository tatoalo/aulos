//! The three seams only the binary can close.
//!
//! Each of these exists because DESIGN §3 forbids the two crates involved from naming each other,
//! and `aulos-server` is the one crate that depends on everything (rule table, `aulos-server`
//! row = "everything"). All three are on WP-17's critical path, and two of them fail *silently*
//! when they are not wired — which is exactly why they are one small module with tests rather
//! than three closures inside [`crate::wiring`].
//!
//! | Seam | Without it |
//! |---|---|
//! | [`EngineFinalizer`] (`aulos_hooks::HookFinalizer`) | a pre-terminal hook ends in a DEBUG line and the item stays `postprocessing` **forever** |
//! | [`DispatcherPreTerminal`] (`aulos_queue::PreTerminalHooks`) | every item finalises in one step, so `audio_sync` never gets its turn and re-encodes nothing |
//! | [`SwapOptions`] (`aulos_subscriptions::check::OptionsSource`) | a subscription check keeps using the `YTDL_OPTIONS` from boot after a hot reload |

use std::sync::Arc;

use arc_swap::ArcSwap;
use aulos_core::ItemId;
use aulos_core::item::ItemView;
use aulos_core::status::TerminalStatus;
use aulos_core::ytdl_options::YtdlOptions;
use aulos_hooks::{Hook, HookFinalizer, HookPhase};
use aulos_queue::{EngineHandle, PreTerminalHooks};
use aulos_subscriptions::check::OptionsSource;

/// Turns `aulos_hooks::HookFinalizer::hooks_finished` into `EngineCmd::HooksFinished`.
///
/// `aulos-hooks` may not depend on `aulos-queue` (DESIGN §3), so it cannot construct the command,
/// and neither crate may `impl` a foreign trait for the other's type. The trait carries **only the
/// id**: `DomainEvent::Finishing` carries an `Arc<ItemView>` and no outcome, so the engine parks
/// the `Outcome` it was going to finalise with and pairs it back up here (DESIGN §13, the WP-11
/// note in `docs/INTEGRATION-NOTES.md`).
#[derive(Clone, Debug)]
pub struct EngineFinalizer(EngineHandle);

impl EngineFinalizer {
    /// Wraps the engine handle.
    #[must_use]
    pub const fn new(engine: EngineHandle) -> Self {
        Self(engine)
    }
}

#[async_trait::async_trait]
impl HookFinalizer for EngineFinalizer {
    async fn hooks_finished(&self, id: ItemId) {
        self.0.hooks_finished(id).await;
    }
}

/// Answers the engine's "is a pre-terminal hook going to run for this item, and what should the
/// `msg` say while it does?" question over the dispatcher's own hook list.
///
/// The engine cannot evaluate `aulos_hooks::Hook::applies` itself (`aulos-queue` must not depend
/// on `aulos-hooks`), so the seam is `aulos_queue::PreTerminalHooks` — one method returning the
/// label the engine writes while the phase runs. The hook list is the **same** `Vec` the
/// dispatcher was built from, so the two can never disagree about which hooks exist.
///
/// The prospective outcome is always [`TerminalStatus::Finished`], because the engine publishes
/// `Finishing` only on the success path (`EngineCmd::Failed` is a different command). If that ever
/// changes, `DomainEvent::Finishing` has to grow the outcome and so does this trait — the WP-11
/// note says the same thing from the other side.
pub struct DispatcherPreTerminal {
    hooks: Vec<Arc<dyn Hook>>,
}

impl std::fmt::Debug for DispatcherPreTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatcherPreTerminal")
            .field(
                "pre_terminal",
                &self.hooks.iter().map(|h| h.id()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl DispatcherPreTerminal {
    /// Keeps only the `PreTerminal` hooks of `hooks`, in the dispatcher's run order.
    ///
    /// Filtering at construction rather than per call matters: `label_for` is on the terminal path
    /// of every single download, and the post-terminal hooks (`nfo`, `jellyfin`, every community
    /// hook) can never answer it.
    #[must_use]
    pub fn new(hooks: &[Arc<dyn Hook>]) -> Self {
        Self {
            hooks: hooks
                .iter()
                .filter(|h| h.phase() == HookPhase::PreTerminal)
                .map(Arc::clone)
                .collect(),
        }
    }

    /// Whether any pre-terminal hook is registered at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// The registered pre-terminal hook ids, in run order.
    #[must_use]
    pub fn ids(&self) -> Vec<Arc<str>> {
        self.hooks.iter().map(|h| h.id()).collect()
    }
}

impl PreTerminalHooks for DispatcherPreTerminal {
    fn label_for(&self, view: &ItemView) -> Option<Box<str>> {
        self.hooks
            .iter()
            .find(|h| h.applies(view, TerminalStatus::Finished))
            .map(|h| Box::from(&*h.id()))
    }
}

/// The live `YTDL_OPTIONS` snapshot, as `aulos-subscriptions` sees it.
///
/// `aulos-subscriptions` does not take `arc-swap` (DESIGN §3's row does not budget it and
/// `tests/arch.rs` enforces the row), so the snapshot reaches the feed checker through this
/// one-method trait. Four lines, no dependency, and a hot reload is visible to the next check.
#[derive(Debug)]
pub struct SwapOptions(Arc<ArcSwap<YtdlOptions>>);

impl SwapOptions {
    /// Wraps the process-wide snapshot.
    #[must_use]
    pub const fn new(ytdl: Arc<ArcSwap<YtdlOptions>>) -> Self {
        Self(ytdl)
    }
}

impl OptionsSource for SwapOptions {
    fn current(&self) -> Arc<YtdlOptions> {
        self.0.load_full()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aulos_core::config::RawEnv;
    use aulos_hooks::{AudioSyncHook, HookCtx, HookError, JellyfinHook, NfoHook};

    use super::*;

    fn config(pairs: &[(&str, &str)]) -> Arc<aulos_core::Config> {
        Arc::new(aulos_core::config::load(&RawEnv::from_pairs(pairs.iter().copied())).unwrap())
    }

    /// A hook whose phase and `applies` answer are fixed, so the adapter can be tested without
    /// building an `ItemView` that satisfies `audio_sync`'s real predicate.
    #[derive(Debug)]
    struct Stub {
        id: &'static str,
        phase: HookPhase,
        applies: bool,
        ordering: i16,
    }

    #[async_trait::async_trait]
    impl Hook for Stub {
        fn id(&self) -> Arc<str> {
            Arc::from(self.id)
        }
        fn ordering(&self) -> i16 {
            self.ordering
        }
        fn phase(&self) -> HookPhase {
            self.phase
        }
        fn applies(&self, _item: &ItemView, _outcome: TerminalStatus) -> bool {
            self.applies
        }
        async fn run(&self, _ctx: HookCtx<'_>) -> Result<(), HookError> {
            Ok(())
        }
    }

    fn stub(id: &'static str, phase: HookPhase, applies: bool, ordering: i16) -> Arc<dyn Hook> {
        Arc::new(Stub {
            id,
            phase,
            applies,
            ordering,
        })
    }

    /// A finished `{video, mp4, best}` row's wire projection.
    fn view() -> ItemView {
        use aulos_core::item::{Item, Kind, ViewExtras};
        use aulos_core::request::DownloadRequest;
        use aulos_core::selection::{Codec, DownloadType, FormatId, QualityId, Selection};
        use aulos_core::source::{SourceKind, SourceRef};
        use aulos_core::status::Status;

        let url: url::Url = "https://fake.test/watch?v=1".parse().unwrap();
        let selection = Selection::new(
            DownloadType::Video,
            Codec::Auto,
            FormatId::parse("mp4").unwrap(),
            QualityId::parse("best").unwrap(),
        );
        let item = Item {
            id: aulos_core::ItemId::new(),
            kind: Kind::Item,
            group_id: None,
            group_index: None,
            ord: 1,
            url: url.clone(),
            canonical_key: "ytdlp:fake.test/1".into(),
            provider: None,
            media_id: None,
            title: "A title".into(),
            status: Status::Postprocessing,
            auto_start: true,
            msg: None,
            error: None,
            request: DownloadRequest::new(url, selection),
            entry: None,
            filename: Some(aulos_core::paths::RelPath::parse("A title.mp4").unwrap()),
            size: None,
            chapter_files: Vec::new(),
            subtitle_files: Vec::new(),
            created_at: 1_757_000_000_000,
            started_at: None,
            finished_at: None,
            attempt: 0,
            source: SourceRef::bare(SourceKind::ApiV2),
            children_total: None,
            clear_after: None,
        };
        ItemView::from_item(&item, None, &ViewExtras::default())
    }

    #[test]
    fn the_pre_terminal_adapter_keeps_only_pre_terminal_hooks_in_order() {
        let hooks = vec![
            stub("second", HookPhase::PreTerminal, true, 20),
            stub("post", HookPhase::PostTerminal, true, 5),
            stub("first", HookPhase::PreTerminal, false, 10),
        ];
        let adapter = DispatcherPreTerminal::new(&hooks);
        assert_eq!(
            adapter.ids().iter().map(|i| &**i).collect::<Vec<_>>(),
            ["second", "first"],
            "the list keeps the dispatcher's order, not `ordering()`"
        );
        assert!(!adapter.is_empty());
        // The first *applicable* hook wins, and a post-terminal hook can never answer.
        assert_eq!(
            adapter.label_for(&view()).as_deref(),
            Some("second"),
            "the label is the applicable hook's id"
        );
    }

    #[test]
    fn no_applicable_pre_terminal_hook_means_no_label() {
        let hooks = vec![
            stub("audio_sync", HookPhase::PreTerminal, false, 10),
            stub("nfo", HookPhase::PostTerminal, true, 20),
        ];
        let adapter = DispatcherPreTerminal::new(&hooks);
        assert_eq!(
            adapter.label_for(&view()),
            None,
            "the engine must finalise in one step when nothing applies"
        );
    }

    #[test]
    fn an_empty_hook_list_behaves_like_no_pre_terminal_hooks_at_all() {
        let adapter = DispatcherPreTerminal::new(&[]);
        assert!(adapter.is_empty());
        assert_eq!(adapter.label_for(&view()), None);
    }

    /// The stock built-in set has exactly one pre-terminal hook, `audio_sync` — the one whose
    /// absence from this adapter would silently stop `best_remux` from ever finalising.
    #[test]
    fn the_stock_built_ins_yield_exactly_audio_sync() {
        let cfg = config(&[]);
        let hooks: Vec<Arc<dyn Hook>> = vec![
            Arc::new(AudioSyncHook::new()),
            Arc::new(NfoHook::from_config(&cfg)),
            Arc::new(JellyfinHook::new(&cfg)),
        ];
        let adapter = DispatcherPreTerminal::new(&hooks);
        assert_eq!(
            adapter.ids().iter().map(|i| &**i).collect::<Vec<_>>(),
            ["audio_sync"]
        );
    }

    #[test]
    fn the_options_source_sees_a_hot_reload() {
        let swap = Arc::new(ArcSwap::from_pointee(YtdlOptions::empty()));
        let source = SwapOptions::new(Arc::clone(&swap));
        assert!(source.current().base.is_empty());

        let mut next = YtdlOptions::empty();
        next.base
            .insert("format".to_owned(), serde_json::json!("bestaudio"));
        swap.store(Arc::new(next));
        assert_eq!(
            source.current().base["format"],
            "bestaudio",
            "the next check must see the reloaded options"
        );
    }

    /// `EngineFinalizer` is the seam that, unwired, leaves a `best_remux` item in
    /// `postprocessing` forever. `EngineHandle::new` is `pub(crate)` in `aulos-queue`, so it
    /// cannot be built here in isolation; the behaviour is asserted end-to-end through the real
    /// wiring instead, by `tests/server.rs::a_pre_terminal_hook_runs_and_the_item_still_finalises`,
    /// which injects a recording pre-terminal hook and checks both that it ran and that the item
    /// reached `finished`. What this test pins is the part that *can* be checked in isolation: the
    /// adapter really does implement the trait the dispatcher takes.
    #[test]
    fn the_finalizer_implements_the_dispatcher_seam() {
        fn assert_is_finalizer<T: HookFinalizer>() {}
        assert_is_finalizer::<EngineFinalizer>();
        fn assert_is_pre_terminal<T: PreTerminalHooks>() {}
        assert_is_pre_terminal::<DispatcherPreTerminal>();
        fn assert_is_options<T: OptionsSource>() {}
        assert_is_options::<SwapOptions>();
    }
}
