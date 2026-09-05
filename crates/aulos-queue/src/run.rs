//! Running a download: the job task, the stage transitions, the terminal writes, the pre-terminal
//! hook handshake and the retry policy (DESIGN §8.7, §8.8, §13).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aulos_core::{DomainEvent, FieldUpdate, FileRef, FileSlot, Item, ItemId, Status, WireError};
use aulos_provider::{
    DownloadCtx, MediaEntry, OutTmpl, Outcome, ProgressSink, Provider, ProviderError, Stage,
};
use aulos_store::{Durability, WriteOp};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cmd::EngineCmd;
use crate::engine::{Engine, PendingHooks, RunSlot};
use crate::entry::SC_PROVIDER;
use crate::slots::Slot;
use crate::watchdog::{self, Watchdog};

/// The automatic-retry base delay: `30 s × 2^attempt ± 20 %` (DESIGN §8.8).
pub const RETRY_BASE_SECS: u64 = 30;

/// The jitter applied to each backoff step, as a fraction (DESIGN §8.8).
pub const RETRY_JITTER: f64 = 0.20;

impl Engine {
    /// Marks an item `preparing`, spawns its download and arms its watchdog (DESIGN §8.7).
    pub(crate) async fn start_job(&mut self, item: &Item, provider: Arc<dyn Provider>, slot: Slot) {
        let id = item.id;
        let out_dir = self.out_dir_for(item);
        let tmp_dir = self.tmp_dir_for(id);

        if !self
            .write_status(
                id,
                Status::Preparing,
                FieldUpdate::Keep,
                FieldUpdate::Keep,
                None,
            )
            .await
        {
            return;
        }

        let cancel = self.shutdown.child_token();
        let sink = self.sink.for_item(id);
        let beat = self.beats.arm(id, self.clock.now_ms());
        let watchdog = watchdog::spawn(Watchdog {
            id,
            beat,
            stall: window(self.cfg.job_stall_secs),
            hard: window(self.cfg.job_timeout_secs),
            clock: Arc::clone(&self.clock),
            events: self.events.clone(),
            sink: self.sink.clone(),
            tx: self.tx.clone(),
            cancel: cancel.clone(),
        });

        let job = RunJob {
            id,
            provider: Arc::clone(&provider),
            entry: crate::entry::rebuild_entry(item),
            request: item.request.clone(),
            ytdl: self.ytdl.load_full(),
            out_dir,
            tmp_dir,
            outtmpl: self.outtmpl_for(item),
            cancel: cancel.clone(),
            sink,
            tx: self.tx.clone(),
        };
        drop(tokio::spawn(job.run()));
        self.running.insert(
            id,
            RunSlot {
                cancel,
                watchdog,
                slot: Some(slot),
                settled: false,
            },
        );
    }

    /// The output-name templates, as legacy built them (DESIGN §9.8, legacy `__add_download`).
    ///
    /// `custom_name_prefix` is prepended to `OUTPUT_TEMPLATE`, then a playlist or channel child
    /// replaces the whole template with `OUTPUT_TEMPLATE_PLAYLIST` / `OUTPUT_TEMPLATE_CHANNEL`
    /// when those are set — which is what legacy did, prefix and all.
    ///
    /// The playlist/channel **field pre-resolution** legacy also did (`_resolve_outtmpl_fields`)
    /// is not applied here: it lives in `aulos_provider_ytdlp::outtmpl`, and `aulos-queue` may not
    /// depend on that crate (DESIGN §3). The `ytdlp` provider applies it to these templates
    /// itself, in `download` — see `docs/INTEGRATION-NOTES.md`, WP-12.
    pub(crate) fn outtmpl_for(&self, item: &Item) -> OutTmpl {
        let base = &*self.cfg.output_template;
        let prefix = &*item.request.custom_name_prefix;
        let mut default = if prefix.is_empty() {
            base.to_owned()
        } else {
            format!("{prefix}.{base}")
        };
        let hints = crate::entry::rebuild_entry(item).hints;
        if hints.playlist_index.is_some() && !self.cfg.output_template_playlist.is_empty() {
            default = self.cfg.output_template_playlist.to_string();
        }
        if hints.channel_index.is_some() && !self.cfg.output_template_channel.is_empty() {
            default = self.cfg.output_template_channel.to_string();
        }
        OutTmpl {
            default,
            chapter: if item.request.chapter_template.is_empty() {
                self.cfg.output_template_chapter.to_string()
            } else {
                item.request.chapter_template.to_string()
            },
        }
    }

    /// [`EngineCmd::Stage`]: a provider's stage transition, persisted (DESIGN §15.1).
    pub(crate) async fn handle_stage(&mut self, id: ItemId, stage: Stage, msg: Option<Box<str>>) {
        self.beats.frame(id, self.clock.now_ms());
        let Some(item) = self.cached(id) else {
            return;
        };
        if item.status.is_terminal() || self.running.get(&id).is_some_and(|r| r.settled) {
            return;
        }
        let patch = msg.map_or(FieldUpdate::Keep, FieldUpdate::Set);
        self.write_status(id, stage.status(), patch, FieldUpdate::Keep, None)
            .await;
    }

    /// [`EngineCmd::File`]: a produced auxiliary file, appended to its list (DESIGN §15.1).
    pub(crate) async fn handle_file(&mut self, id: ItemId, slot: FileSlot, file: FileRef) {
        if self.cached(id).is_none() {
            return;
        }
        if !self
            .apply(
                vec![WriteOp::PushFile {
                    id,
                    slot,
                    file: file.clone(),
                }],
                Durability::Batched,
            )
            .await
        {
            return;
        }
        self.patch(id, |item| match slot {
            FileSlot::Chapter => item.chapter_files.push(file.clone()),
            FileSlot::Subtitle => item.subtitle_files.push(file.clone()),
        });
        let status = self.cached(id).map_or(Status::Downloading, |i| i.status);
        self.publish_changed(id, status, status).await;
    }

    /// [`EngineCmd::Finished`]: the provider produced a file (DESIGN §8.7, §13).
    pub(crate) async fn handle_finished(&mut self, id: ItemId, outcome: Box<Outcome>) {
        if !self.release_job(id) {
            // The slot this job held is gone, so whatever `release_job` put back on a deque — a
            // start pressed during the kill grace — can run now.
            self.schedule().await;
            return;
        }
        let Some(item) = self.cached(id) else {
            return;
        };
        if item.status.is_terminal() {
            return;
        }

        // The pre-terminal phase: write `postprocessing`, publish `Finishing`, and finalise only
        // on `HooksFinished` (DESIGN §13). The download slot has already been released above, so a
        // pre-terminal hook never blocks the next download.
        let view = self.view(&item);
        if let Some(label) = self.pre_terminal.label_for(&view) {
            let wrote = self
                .write_status(
                    id,
                    Status::Postprocessing,
                    FieldUpdate::Set(label),
                    FieldUpdate::Keep,
                    None,
                )
                .await;
            let published = wrote.then(|| self.view_of(id)).flatten();
            if let Some(view) = published {
                let deadline_ms = self.clock.now_ms() + self.pre_terminal_timeout_ms;
                self.pending_hooks.insert(
                    id,
                    PendingHooks {
                        outcome,
                        deadline_ms,
                    },
                );
                self.events.publish(DomainEvent::Finishing(view)).await;
                self.schedule().await;
                return;
            }
        }
        self.finalise_success(id, &outcome).await;
        self.schedule().await;
    }

    /// The terminal write for a successful download (DESIGN §8.7, §8.10, §13).
    pub(crate) async fn finalise_success(&mut self, id: ItemId, outcome: &Outcome) {
        let Some(item) = self.cached(id) else {
            return;
        };
        if item.status.is_terminal() {
            return;
        }
        // A pre-terminal hook that rewrote the file already told the engine the new size through
        // the port (DESIGN §13.3); the provider's outcome predates it, so the hook's value wins and
        // the single `completed` frame carries the post-re-encode size.
        let size = if self.hook_sized.remove(&id) {
            item.size
        } else {
            outcome.size.or_else(|| {
                outcome
                    .filename
                    .as_ref()
                    .and_then(|f| std::fs::metadata(self.out_dir_for(&item).join(f.as_path())).ok())
                    .map(|m| m.len())
            })
        };

        let mut ops = vec![WriteOp::SetOutput {
            id,
            filename: outcome.filename.clone(),
            size,
        }];
        // A finished item drops its entry blob, except a StreamingCommunity one, which the NFO
        // hook still needs (DESIGN §7.5).
        let keeps_entry = item
            .provider
            .as_ref()
            .is_some_and(|p| p.as_str() == SC_PROVIDER);
        if !keeps_entry && item.entry.is_some() {
            ops.push(WriteOp::DropEntryBlob { id });
        }
        if !self.apply(ops, Durability::Batched).await {
            return;
        }
        let filename = outcome.filename.clone();
        self.patch(id, |i| {
            i.filename = filename.clone();
            i.size = size;
            if !keeps_entry {
                i.entry = None;
            }
        });

        self.terminate(id, Status::Finished, FieldUpdate::Clear)
            .await;
        self.cleanup_partials(id, true);
    }

    /// [`EngineCmd::Failed`]: the provider gave up (DESIGN §8.8).
    pub(crate) async fn handle_failed(&mut self, id: ItemId, err: ProviderError) {
        if !self.release_job(id) {
            self.schedule().await;
            return;
        }
        let Some(item) = self.cached(id) else {
            return;
        };
        if item.status.is_terminal() {
            return;
        }
        let wire = match item.provider.as_ref() {
            Some(p) => err.to_wire(p, None),
            None => WireError::new(err.code(), err.message()),
        };

        if matches!(err, ProviderError::Canceled) {
            self.terminate(id, Status::Canceled, FieldUpdate::Set(wire))
                .await;
            self.cleanup_partials(id, true);
            self.schedule().await;
            return;
        }

        let attempts_left = u32::from(item.attempt) < self.cfg.auto_retry_max;
        if err.retryable() && attempts_left {
            self.arm_auto_retry(id, wire).await;
        } else {
            self.terminate(id, Status::Error, FieldUpdate::Set(wire))
                .await;
            self.cleanup_partials(id, true);
        }
        self.schedule().await;
    }

    /// Schedules an automatic retry with the DESIGN §8.8 backoff.
    async fn arm_auto_retry(&mut self, id: ItemId, error: WireError) {
        let Some(item) = self.cached(id) else {
            return;
        };
        let delay = backoff(item.attempt);
        let at_ms = self.clock.now_ms() + i64::try_from(delay.as_millis()).unwrap_or(0);
        let now = self.clock.now_ms();
        let msg = format!("Retrying in {}s", delay.as_secs().max(1));
        let source = aulos_core::SourceRef::bare(aulos_core::SourceKind::Retry);

        // The DESIGN §7.1 retry triple, with a message: the manual path uses
        // `aulos_store::retry_ops` verbatim, and this one differs only in keeping the failure
        // visible while the backoff runs.
        let ops = vec![
            WriteOp::SetStatus {
                id,
                status: Status::Queued,
                msg: FieldUpdate::Set(msg.clone().into_boxed_str()),
                error: FieldUpdate::Set(error.clone()),
                auto_start: Some(true),
                at: now,
            },
            WriteOp::BumpAttempt { id },
            WriteOp::SetSource {
                id,
                source: source.clone(),
            },
        ];
        if !self.apply(ops, Durability::Batched).await {
            return;
        }
        let from = item.status;
        self.patch(id, |i| {
            i.status = Status::Queued;
            i.auto_start = true;
            i.msg = Some(msg.clone().into_boxed_str());
            i.error = Some(error.clone());
            i.attempt = i.attempt.saturating_add(1);
            i.source = source.clone();
            i.finished_at = None;
        });
        self.retries.push(crate::engine::PendingRetry { id, at_ms });
        self.on_child_status(id, from, Status::Queued).await;
        self.publish_changed(id, from, Status::Queued).await;
        tracing::info!(item = %id, delay = ?delay, "armed an automatic retry");
    }

    /// Frees the slot, watchdog and heartbeat of a finished job.
    ///
    /// Returns `false` when the engine has already written this job's outcome itself — a cancel or
    /// a pause — in which case the task's own report is discarded rather than overwriting it.
    ///
    /// A settled slot lingers in `running` for the whole `killpg` SIGTERM → SIGKILL ladder
    /// (`AULOS_KILL_GRACE_MS`, seconds). A `start` (or a `retry` after a cancel) inside that window
    /// only writes `auto_start = true` and enqueues, and `schedule()` cannot admit the row while
    /// the slot exists — so the row is re-enqueued here, the moment the blocker is gone. Both
    /// callers run `schedule()` on this path; without that the item would sit `queued` with
    /// `auto_start = true` in no deque, and nothing would ever start it again.
    pub(crate) fn release_job(&mut self, id: ItemId) -> bool {
        self.beats.disarm(id);
        let Some(mut slot) = self.running.remove(&id) else {
            return true;
        };
        drop(slot.slot.take());
        if let Some(w) = slot.watchdog.take() {
            w.abort();
        }
        if slot.settled {
            if self
                .cached(id)
                .is_some_and(|i| i.status == Status::Queued && i.auto_start)
            {
                self.enqueue(id);
            }
            return false;
        }
        true
    }

    /// Removes an item's partial files (DESIGN §8.7, §8.10).
    ///
    /// `remove_partials = false` is the pause path: the `*.part`/`*.ytdl` files stay so yt-dlp can
    /// resume from them. A StreamingCommunity job's partials are removed either way, because its
    /// m3u8 token is dead the moment the process dies.
    pub(crate) fn cleanup_partials(&self, id: ItemId, remove_partials: bool) {
        let is_sc = self
            .cached(id)
            .and_then(|i| i.provider.clone())
            .is_some_and(|p| p.as_str() == SC_PROVIDER);
        if !remove_partials && !is_sc {
            return;
        }
        let tmp = self.tmp_dir_for(id);
        if tmp.exists()
            && let Err(e) = std::fs::remove_dir_all(&tmp)
        {
            tracing::warn!(item = %id, dir = %tmp.display(), error = %e, "cannot remove the scratch directory");
        }
        // A provider that wrote its partial next to the final file rather than in the scratch
        // directory: legacy left both of these behind.
        if let Some(item) = self.cached(id)
            && let Some(name) = item.filename.as_ref()
        {
            let base = self.out_dir_for(&item).join(name.as_path());
            for suffix in ["part", "ytdl"] {
                let mut candidate = base.clone().into_os_string();
                candidate.push(".");
                candidate.push(suffix);
                remove_file_quietly(Path::new(&candidate));
            }
        }
    }
}

/// The DESIGN §8.8 backoff: `30 s × 2^attempt ± 20 %`.
#[must_use]
pub fn backoff(attempt: u16) -> Duration {
    let exp = u32::from(attempt).min(16);
    let base = RETRY_BASE_SECS.saturating_mul(1u64 << exp);
    let jitter = 1.0 + (rand::random::<f64>() * 2.0 - 1.0) * RETRY_JITTER;
    let secs = (base as f64 * jitter).max(1.0);
    Duration::from_millis((secs * 1000.0) as u64)
}

/// The delay window a `0 = off` configuration turns into `None`.
fn window(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Best-effort unlink with a WARN (DESIGN §8.10).
fn remove_file_quietly(path: &Path) {
    if !path.exists() {
        return;
    }
    if let Err(e) = std::fs::remove_file(path) {
        tracing::warn!(path = %path.display(), error = %e, "cannot remove");
    }
}

/// One download, running off the engine task.
struct RunJob {
    id: ItemId,
    provider: Arc<dyn Provider>,
    entry: MediaEntry,
    request: aulos_core::DownloadRequest,
    ytdl: Arc<aulos_core::YtdlOptions>,
    out_dir: PathBuf,
    tmp_dir: PathBuf,
    outtmpl: OutTmpl,
    cancel: CancellationToken,
    sink: ProgressSink,
    tx: mpsc::Sender<EngineCmd>,
}

impl RunJob {
    /// Calls `download` and reports the result exactly once.
    async fn run(self) {
        let Self {
            id,
            provider,
            entry,
            request,
            ytdl,
            out_dir,
            tmp_dir,
            outtmpl,
            cancel,
            sink,
            tx,
        } = self;
        let mut guard = ReportGuard {
            id,
            tx: tx.clone(),
            armed: true,
        };
        // The two directories, created on this task rather than on the engine's: the download
        // roots are bind-mounted volumes on the VPS, and a slow or hung mount must cost this job
        // its own start, never every other queue command behind it (DESIGN §8.2).
        for dir in [&out_dir, &tmp_dir] {
            let dir = dir.clone();
            let created = tokio::task::spawn_blocking(move || {
                std::fs::create_dir_all(&dir).map_err(|e| (dir, e))
            })
            .await;
            if let Ok(Err((dir, e))) = created {
                tracing::warn!(item = %id, dir = %dir.display(), error = %e, "cannot create");
            }
        }

        let ctx = DownloadCtx {
            item_id: id,
            entry: &entry,
            request: &request,
            ytdl_options: ytdl,
            out_dir,
            tmp_dir,
            outtmpl,
            cancel,
        };
        let result = provider.download(ctx, sink).await;
        guard.armed = false;
        let cmd = match result {
            Ok(outcome) => EngineCmd::Finished {
                id,
                outcome: Box::new(outcome),
            },
            Err(err) => EngineCmd::Failed {
                id,
                err: Box::new(err),
            },
        };
        let _ = tx.send(cmd).await;
    }
}

/// Reports a failure if the job task is dropped without producing one.
///
/// A provider that panics, or a runtime that drops the task at shutdown, would otherwise leave the
/// item `downloading` with its slot held until the next restart.
struct ReportGuard {
    id: ItemId,
    tx: mpsc::Sender<EngineCmd>,
    armed: bool,
}

impl Drop for ReportGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self.tx.try_send(EngineCmd::Failed {
            id: self.id,
            err: Box::new(ProviderError::Other(
                "the download task ended without reporting a result".to_owned(),
            )),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_curve_is_thirty_times_two_to_the_n_within_twenty_percent() {
        for attempt in 0u16..4 {
            let nominal = RETRY_BASE_SECS * (1 << u32::from(attempt));
            let low = nominal as f64 * 0.8;
            let high = nominal as f64 * 1.2;
            for _ in 0..64 {
                let secs = backoff(attempt).as_secs_f64();
                assert!(
                    (low..=high).contains(&secs),
                    "attempt {attempt}: {secs}s outside {low}..={high}"
                );
            }
        }
    }

    #[test]
    fn the_backoff_saturates_rather_than_overflowing() {
        assert!(backoff(u16::MAX).as_secs() > 0);
    }

    #[test]
    fn zero_means_the_timer_is_off() {
        assert_eq!(window(0), None);
        assert_eq!(window(900), Some(Duration::from_secs(900)));
    }
}
