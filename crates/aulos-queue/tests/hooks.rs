//! The pre-terminal hook handshake and `EngineHookStore` (DESIGN §13, §13.3, §7.1).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aulos_core::{DomainEvent, EntryBlob, HookStore, ItemId, ItemView, PortError, Status};
use aulos_queue::{EngineHookStore, PreTerminalHooks};
use support::{AlwaysPreTerminal, Harness, NeverPreTerminal, expanding};

/// A gate that claims every item and counts how often it was asked.
#[derive(Default)]
struct CountingGate {
    asked: AtomicUsize,
    label: &'static str,
}

impl PreTerminalHooks for CountingGate {
    fn label_for(&self, _view: &ItemView) -> Option<Box<str>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        (!self.label.is_empty()).then(|| Box::from(self.label))
    }
}

#[tokio::test]
async fn a_pre_terminal_hook_delays_the_terminal_write_until_hooks_finished() {
    let h = Harness::builder()
        .pre_terminal(Arc::new(AlwaysPreTerminal("Re-encoding audio")))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/remux").await;

    // The engine writes `postprocessing` with the hook's label and publishes `Finishing`, and
    // stops there: no terminal write, no `completed` frame.
    let row = h
        .until(id, "the pre-terminal phase", |i| {
            i.status == Status::Postprocessing && i.msg.as_deref() == Some("Re-encoding audio")
        })
        .await;
    assert_eq!(row.status, Status::Postprocessing);
    assert_eq!(row.finished_at, None, "the terminal write has not happened");
    h.settle().await;
    assert_eq!(
        h.item(id).await.unwrap().status,
        Status::Postprocessing,
        "and it stays there until the dispatcher answers"
    );

    h.hooks
        .until(
            "finishing",
            |e| matches!(e, DomainEvent::Finishing(v) if v.id == id),
        )
        .await;
    let finishing = h.hooks.finishing();
    assert_eq!(finishing.len(), 1, "one Finishing, hooks-only");
    assert_eq!(finishing[0].id, id);
    assert_eq!(finishing[0].status, Status::Postprocessing);
    assert!(
        h.events.completed().is_empty(),
        "no `completed` frame before the hooks are done"
    );

    // The dispatcher answers. Only now does the item finalise.
    h.handle.hooks_finished(id).await;
    let done = h.until_status(id, Status::Finished).await;
    assert!(
        done.filename.is_some(),
        "with the outcome the engine parked"
    );
    assert_eq!(done.size, Some(1_024));
    let completed = h.events.completed();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].id, id);
    assert_eq!(completed[0].percent, 100.0);
}

#[tokio::test]
async fn an_item_with_no_pre_terminal_hook_finalises_in_one_step() {
    let h = Harness::builder()
        .pre_terminal(Arc::new(NeverPreTerminal))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/plain").await;
    h.until_status(id, Status::Finished).await;
    assert!(
        h.hooks.finishing().is_empty(),
        "no `Finishing` at all when nothing applies"
    );
    assert_eq!(h.events.completed().len(), 1);
}

#[tokio::test]
async fn a_dispatcher_that_never_answers_is_finalised_by_the_timeout() {
    let h = Harness::builder()
        .pre_terminal(Arc::new(AlwaysPreTerminal("Re-encoding audio")))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/wedged").await;
    h.until_status(id, Status::Postprocessing).await;

    // The engine's own safety net: `PRE_TERMINAL_TIMEOUT_MS` past the handshake it finalises with
    // the outcome it already had, and logs at WARN.
    h.advance(Duration::from_millis(
        u64::try_from(aulos_queue::engine::PRE_TERMINAL_TIMEOUT_MS + 1_000).unwrap(),
    ))
    .await;
    let done = h.until_status(id, Status::Finished).await;
    assert!(
        done.filename.is_some(),
        "finalised with the parked outcome, not with an empty one"
    );
}

#[tokio::test]
async fn the_gate_is_asked_once_per_terminal_transition() {
    let gate = Arc::new(CountingGate {
        asked: AtomicUsize::new(0),
        label: "",
    });
    let h = Harness::builder()
        .pre_terminal(Arc::clone(&gate) as Arc<dyn PreTerminalHooks>)
        .provider(Arc::new(expanding(3)))
        .build()
        .await;
    let group = h.add("https://fake.test/playlist/three").await;
    h.until(group, "the roll-up", |i| i.status == Status::Finished)
        .await;
    h.settle().await;
    assert_eq!(
        gate.asked.load(Ordering::SeqCst),
        3,
        "once per finished child, never for the group"
    );
}

#[tokio::test]
async fn hooks_finished_for_an_item_that_is_not_waiting_is_a_no_op() {
    let h = Harness::new().await;
    let id = h.add("https://fake.test/watch/plain").await;
    h.until_status(id, Status::Finished).await;
    h.handle.hooks_finished(id).await;
    h.handle.hooks_finished(ItemId::new()).await;
    h.settle().await;
    assert_eq!(
        h.item(id).await.unwrap().status,
        Status::Finished,
        "and does not disturb the item"
    );
    assert_eq!(h.events.completed().len(), 1, "nor duplicate its frame");
}

#[tokio::test]
async fn set_size_lands_as_a_write_a_cache_update_and_a_delta() {
    let h = Harness::builder()
        .pre_terminal(Arc::new(AlwaysPreTerminal("Re-encoding audio")))
        .build()
        .await;
    let store: Arc<dyn HookStore> =
        Arc::new(EngineHookStore::new(h.handle.clone(), h.store.clone()));
    let id = h.add("https://fake.test/watch/remux").await;
    h.until_status(id, Status::Postprocessing).await;
    h.events.clear();

    // This is the regression test for the writeback that used to bypass the engine.
    store.set_size(id, 4_242).await.expect("set_size");
    let row = h.until(id, "the new size", |i| i.size == Some(4_242)).await;
    assert_eq!(row.size, Some(4_242), "persisted");

    h.events
        .until("re-diff", |e| {
            matches!(e, DomainEvent::StatusChanged { id: got, view, .. }
                if *got == id && view.size == Some(4_242))
        })
        .await;
    let deltas = h.events.changes(id);
    let carried = deltas
        .iter()
        .rev()
        .find(|v| v.size == Some(4_242))
        .expect("a frame carrying the new size");
    assert_eq!(
        carried.status,
        Status::Postprocessing,
        "the `from == to` re-diff signal, not a status change"
    );
    assert!(
        h.events.all().iter().any(|e| matches!(
            &**e,
            DomainEvent::StatusChanged { from, to, .. } if from == to
        )),
        "and it is published as a StatusChanged whose from equals its to"
    );

    // The terminal frame then carries the post-re-encode size, not the provider's.
    h.handle.hooks_finished(id).await;
    let completed = h.until_status(id, Status::Finished).await;
    assert_eq!(
        completed.size,
        Some(4_242),
        "no client is ever handed a size a hook is about to change"
    );
}

#[tokio::test]
async fn drop_entry_blob_and_entry_blob_go_the_documented_ways() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(2)))
        .pre_terminal(Arc::new(AlwaysPreTerminal("Writing NFO")))
        .build()
        .await;
    let store: Arc<dyn HookStore> =
        Arc::new(EngineHookStore::new(h.handle.clone(), h.store.clone()));
    let group = h.add("https://fake.test/playlist/nfo").await;
    let child = h
        .until_all("a child in the pre-terminal phase", |rows| {
            rows.iter()
                .any(|i| i.group_id == Some(group) && i.status == Status::Postprocessing)
        })
        .await
        .into_iter()
        .find(|i| i.group_id == Some(group) && i.status == Status::Postprocessing)
        .expect("a child");

    // `entry_blob` reads through the store.
    let blob: Option<EntryBlob> = store.entry_blob(child.id).await.expect("a read");
    assert!(
        blob.is_some(),
        "a playlist child keeps its hints so `outtmpl` survives a restart"
    );

    // `drop_entry_blob` goes through the engine.
    store.drop_entry_blob(child.id).await.expect("a drop");
    let row = h
        .until(child.id, "the blob to go", |i| i.entry.is_none())
        .await;
    assert_eq!(row.entry, None);
    assert_eq!(
        store.entry_blob(child.id).await.expect("a read"),
        None,
        "and the read agrees"
    );
}

#[tokio::test]
async fn a_port_call_for_a_deleted_row_is_not_found() {
    let h = Harness::new().await;
    let store: Arc<dyn HookStore> =
        Arc::new(EngineHookStore::new(h.handle.clone(), h.store.clone()));
    let ghost = ItemId::new();
    assert_eq!(
        store.set_size(ghost, 1).await,
        Err(PortError::NotFound(ghost))
    );
    assert_eq!(
        store.drop_entry_blob(ghost).await,
        Err(PortError::NotFound(ghost))
    );
    // The read reports the missing row rather than the absent blob, which is what
    // `Store::entry_blob` documents: `Ok(None)` means "this row has no blob".
    assert_eq!(
        store.entry_blob(ghost).await,
        Err(PortError::NotFound(ghost))
    );
}
