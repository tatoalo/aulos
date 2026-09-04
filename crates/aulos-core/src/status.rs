//! The closed status vocabulary and its legal-transition table (BRIEF §6, DESIGN §4.2).

use serde::{Deserialize, Serialize};

/// The closed eight-value status vocabulary. Groups use the same values (DESIGN §4.6, §8.6).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Waiting: for a slot when `auto_start` is true, for the user when it is false.
    Queued,
    /// Metadata extraction is in flight.
    Resolving,
    /// A slot is held; the provider is starting up.
    Preparing,
    /// Bytes are moving.
    Downloading,
    /// Merging, remuxing, subtitle conversion, or a `PreTerminal` hook (DESIGN §13).
    Postprocessing,
    /// Terminal: the file exists.
    Finished,
    /// Terminal: it failed. `error` is non-null.
    Error,
    /// Terminal: the user cancelled it.
    Canceled,
}

impl Status {
    /// `Finished | Error | Canceled`.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Error | Self::Canceled)
    }

    /// `Resolving | Preparing | Downloading | Postprocessing` — the item is doing work.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Resolving | Self::Preparing | Self::Downloading | Self::Postprocessing
        )
    }

    /// `Preparing | Downloading | Postprocessing` — the item holds a download slot.
    #[must_use]
    pub const fn is_running(self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Downloading | Self::Postprocessing
        )
    }

    /// The legacy v1 status name this maps to (DESIGN §11.5).
    ///
    /// `Queued` collapses onto `pending` for both flag values — the v1 shim splits the
    /// `queue[]`/`pending[]` arrays by `auto_start`, not by this string. `Postprocessing` maps to
    /// `downloading` because legacy showed a frozen `downloading` during ffmpeg and carried the
    /// phase in `msg`.
    ///
    /// `Canceled` has **no** legacy name: DESIGN §11.4 omits cancelled items from `GET history`
    /// entirely. `"error"` is returned as a defensive fallback so a caller that projects one
    /// anyway still emits a value from the closed legacy set.
    #[must_use]
    pub const fn v1(self) -> &'static str {
        match self {
            Self::Queued | Self::Resolving => "pending",
            Self::Preparing => "preparing",
            Self::Downloading | Self::Postprocessing => "downloading",
            Self::Finished => "finished",
            Self::Error | Self::Canceled => "error",
        }
    }

    /// The status as its wire string, without going through serde.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Resolving => "resolving",
            Self::Preparing => "preparing",
            Self::Downloading => "downloading",
            Self::Postprocessing => "postprocessing",
            Self::Finished => "finished",
            Self::Error => "error",
            Self::Canceled => "canceled",
        }
    }

    /// Every value, in declaration order. Used by exhaustiveness tests and the transition table.
    pub const ALL: [Self; 8] = [
        Self::Queued,
        Self::Resolving,
        Self::Preparing,
        Self::Downloading,
        Self::Postprocessing,
        Self::Finished,
        Self::Error,
        Self::Canceled,
    ];
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The three terminal statuses, as a type.
///
/// This is what a community `[[hook]]` manifest's `on = [...]` parses into (DESIGN §13.4) and what
/// `BatchEntry.status` carries, so a `PreTerminal` hook can read the *prospective* outcome without
/// an `Option`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalStatus {
    /// The file exists.
    Finished,
    /// It failed.
    Error,
    /// The user cancelled it.
    Canceled,
}

impl TerminalStatus {
    /// Widens back to [`Status`]. Total, so no error path.
    #[must_use]
    pub const fn as_status(self) -> Status {
        match self {
            Self::Finished => Status::Finished,
            Self::Error => Status::Error,
            Self::Canceled => Status::Canceled,
        }
    }

    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.as_status().as_str()
    }

    /// Every value, in declaration order.
    pub const ALL: [Self; 3] = [Self::Finished, Self::Error, Self::Canceled];
}

impl std::fmt::Display for TerminalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<TerminalStatus> for Status {
    fn from(t: TerminalStatus) -> Self {
        t.as_status()
    }
}

impl TryFrom<Status> for TerminalStatus {
    type Error = NotTerminal;

    fn try_from(s: Status) -> Result<Self, NotTerminal> {
        match s {
            Status::Finished => Ok(Self::Finished),
            Status::Error => Ok(Self::Error),
            Status::Canceled => Ok(Self::Canceled),
            other => Err(NotTerminal(other)),
        }
    }
}

/// [`TerminalStatus::try_from`] was handed a non-terminal [`Status`].
#[derive(Debug, thiserror::Error)]
#[error("{0} is not a terminal status")]
pub struct NotTerminal(pub Status);

/// One node of the transition table: a status plus, for `Queued`, its `auto_start` flag.
///
/// The pause and start edges of DESIGN §4.2 are `Queued → Queued` transitions that differ only in
/// the flag, so [`can_transition`] cannot be expressed over [`Status`] alone.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StatusEdge {
    /// The status.
    pub status: Status,
    /// `Queued`'s scheduling flag; ignored for every other status.
    pub auto_start: bool,
}

impl StatusEdge {
    /// A node with `auto_start = true` (the value that matters only for `Queued`).
    #[must_use]
    pub const fn scheduled(status: Status) -> Self {
        Self {
            status,
            auto_start: true,
        }
    }

    /// A node with `auto_start = false` — for `Queued`, the legacy *pending* bucket.
    #[must_use]
    pub const fn paused(status: Status) -> Self {
        Self {
            status,
            auto_start: false,
        }
    }

    /// The effective flag: only `Queued` distinguishes the two.
    #[must_use]
    const fn effective_auto_start(self) -> bool {
        match self.status {
            Status::Queued => self.auto_start,
            _ => true,
        }
    }
}

impl From<Status> for StatusEdge {
    fn from(status: Status) -> Self {
        Self::scheduled(status)
    }
}

/// Whether `from → to` is a legal transition (DESIGN §4.2). Anything else is a bug.
///
/// Callers on a write path should wrap this in `debug_assert!`; the engine also uses it as a real
/// guard so a release build refuses an illegal write instead of corrupting the queue.
///
/// The table, verbatim from DESIGN §4.2:
///
/// ```text
/// Resolving ──► Queued
/// Queued ──► Resolving
/// Queued ──► Preparing ──► Downloading ──► Postprocessing ──► Finished
///                      └───────────────────────────────────► Error
/// Queued(auto_start=true) ──► Queued(auto_start=false)          (pause)
/// Preparing | Downloading | Postprocessing ──► Queued(auto_start=false)
/// Queued(auto_start=false) ──► Queued(auto_start=true)          (start)
/// any non-terminal ──► Canceled
/// Error | Canceled ──► Queued
/// ```
///
/// There is deliberately **no** `Finished → Postprocessing` edge: the pre-terminal hook phase of
/// DESIGN §13 runs while the row is still `postprocessing` and does not need one.
#[must_use]
pub fn can_transition(from: impl Into<StatusEdge>, to: impl Into<StatusEdge>) -> bool {
    use Status::{
        Canceled, Downloading, Error, Finished, Postprocessing, Preparing, Queued, Resolving,
    };

    let from: StatusEdge = from.into();
    let to: StatusEdge = to.into();

    // A no-op re-write of the same node is the engine's generic "this row changed, re-diff it"
    // signal (`StatusChanged { from == to }`, DESIGN §8.1), so it is legal.
    if from.status == to.status && from.effective_auto_start() == to.effective_auto_start() {
        return true;
    }

    // Cancel: any non-terminal status can be cancelled.
    if to.status == Canceled && !from.status.is_terminal() {
        return true;
    }

    // Pause of a running job: kill it, keep the partial file, park it as `queued(false)`. This is
    // the one edge whose legality depends on the flag, so it is tested before the flat table.
    if matches!(from.status, Preparing | Downloading | Postprocessing) && to.status == Queued {
        return !to.auto_start;
    }

    // The flat table. Read it as four groups:
    //   * anything queueable → `queued`: a resolve that produced entries, the `queued → queued`
    //     flag flip, and a retry (manual, automatic, or boot recovery, DESIGN §8.9);
    //   * `queued` → `resolving` (a retry of an unresolved item) or `preparing` (the happy path);
    //   * the rest of the happy path;
    //   * a failure, which can happen once the job has been handed to a provider and also
    //     terminates a `resolving` item.
    matches!(
        (from.status, to.status),
        (Resolving | Queued | Error | Canceled, Queued)
            | (Queued, Resolving | Preparing)
            | (Preparing, Downloading)
            | (Downloading, Postprocessing)
            | (Postprocessing, Finished)
            | (
                Preparing | Downloading | Postprocessing | Resolving | Queued,
                Error
            )
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use Status::{
        Canceled, Downloading, Error, Finished, Postprocessing, Preparing, Queued, Resolving,
    };

    #[test]
    fn predicates_partition_the_vocabulary() {
        for s in Status::ALL {
            assert!(
                !(s.is_terminal() && s.is_active()),
                "{s} cannot be both terminal and active"
            );
            if s.is_running() {
                assert!(s.is_active(), "{s} holds a slot so it must be active");
            }
        }
        // `Queued` is the one status that is neither: it is waiting, not working.
        assert!(!Queued.is_terminal() && !Queued.is_active() && !Queued.is_running());
        assert_eq!(
            Status::ALL
                .iter()
                .filter(|s| !s.is_terminal() && !s.is_active())
                .collect::<Vec<_>>(),
            [&Queued]
        );
        assert_eq!(
            Status::ALL.iter().filter(|s| s.is_running()).count(),
            3,
            "exactly three statuses hold a slot"
        );
        assert_eq!(Status::ALL.iter().filter(|s| s.is_terminal()).count(), 3);
        assert_eq!(Status::ALL.iter().filter(|s| s.is_active()).count(), 4);
    }

    #[test]
    fn serde_uses_lowercase_names() {
        for s in Status::ALL {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), s);
        }
    }

    #[test]
    fn v1_projection_matches_the_legacy_vocabulary() {
        assert_eq!(Queued.v1(), "pending");
        assert_eq!(Resolving.v1(), "pending");
        assert_eq!(Preparing.v1(), "preparing");
        assert_eq!(Downloading.v1(), "downloading");
        assert_eq!(Postprocessing.v1(), "downloading");
        assert_eq!(Finished.v1(), "finished");
        assert_eq!(Error.v1(), "error");
        assert_eq!(Canceled.v1(), "error");
    }

    #[test]
    fn terminal_status_round_trips() {
        for t in TerminalStatus::ALL {
            assert_eq!(TerminalStatus::try_from(t.as_status()).unwrap(), t);
        }
        for s in Status::ALL {
            assert_eq!(TerminalStatus::try_from(s).is_ok(), s.is_terminal());
        }
    }

    #[test]
    fn legal_edges_are_accepted() {
        let legal = [
            (Resolving, Queued),
            (Queued, Resolving),
            (Queued, Preparing),
            (Preparing, Downloading),
            (Downloading, Postprocessing),
            (Postprocessing, Finished),
            (Preparing, Error),
            (Downloading, Error),
            (Postprocessing, Error),
            (Resolving, Error),
            (Queued, Error),
            (Error, Queued),
            (Canceled, Queued),
        ];
        for (a, b) in legal {
            assert!(can_transition(a, b), "{a} -> {b} must be legal");
        }
        for s in Status::ALL {
            assert_eq!(
                can_transition(s, Canceled),
                !s.is_terminal() || s == Canceled,
                "cancel from {s}"
            );
        }
    }

    #[test]
    fn pause_and_start_edges_are_accepted() {
        // pause: un-schedule a queued item
        assert!(can_transition(
            StatusEdge::scheduled(Queued),
            StatusEdge::paused(Queued)
        ));
        // start again
        assert!(can_transition(
            StatusEdge::paused(Queued),
            StatusEdge::scheduled(Queued)
        ));
        // pause a running job: park it as queued(false)
        for s in [Preparing, Downloading, Postprocessing] {
            assert!(
                can_transition(StatusEdge::scheduled(s), StatusEdge::paused(Queued)),
                "{s} -> queued(paused) must be legal"
            );
            assert!(
                !can_transition(StatusEdge::scheduled(s), StatusEdge::scheduled(Queued)),
                "{s} -> queued(auto_start) must be rejected: pausing is the only way back"
            );
        }
    }

    #[test]
    fn illegal_edges_are_rejected() {
        let illegal = [
            // The one DESIGN §13 calls out explicitly.
            (Finished, Postprocessing),
            (Finished, Downloading),
            (Finished, Preparing),
            (Finished, Queued),
            (Finished, Resolving),
            (Finished, Error),
            (Finished, Canceled),
            (Error, Downloading),
            (Error, Preparing),
            (Error, Finished),
            (Error, Canceled),
            (Canceled, Finished),
            (Canceled, Downloading),
            (Queued, Downloading),
            (Queued, Postprocessing),
            (Queued, Finished),
            (Resolving, Preparing),
            (Resolving, Downloading),
            (Resolving, Finished),
            (Preparing, Postprocessing),
            (Preparing, Finished),
            (Downloading, Finished),
            (Downloading, Preparing),
            (Postprocessing, Downloading),
            (Postprocessing, Preparing),
            (Postprocessing, Resolving),
        ];
        for (a, b) in illegal {
            assert!(!can_transition(a, b), "{a} -> {b} must be rejected");
        }
    }

    #[test]
    fn a_self_edge_is_the_re_diff_signal() {
        for s in Status::ALL {
            assert!(
                can_transition(s, s),
                "{s} -> {s} is StatusChanged{{from==to}}"
            );
        }
    }
}
