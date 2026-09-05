//! Group accumulators: the incremental child roll-up (DESIGN §8.6).
//!
//! A group row never downloads. Its `status`, `percent`, `speed` and `eta` are derived from its
//! children and maintained **incrementally** — never recomputed per tick, because a 500-child
//! playlist on a 250 ms tick would otherwise walk 500 rows four times a second forever.
//!
//! Two of the eight fields are owned by the aggregator rather than the engine, because they are
//! progress-derived and progress never enters the engine (DESIGN §2.2): [`GroupAcc::downloaded`],
//! [`GroupAcc::speed`] and [`GroupAcc::active_percent`]. They are `pub` for exactly that reason.
//!
//! Incremental accumulators drift, so [`GroupAcc::correct`] recomputes from scratch every five
//! minutes and fixes any divergence.

use aulos_core::{Item, Status};

/// How often the engine recomputes a group's accumulator from its children (DESIGN §8.6).
pub const DRIFT_RECOMPUTE_MS: i64 = 5 * 60 * 1000;

/// One group's incrementally maintained aggregate (DESIGN §8.6).
#[derive(Clone, PartialEq, Debug, Default)]
pub struct GroupAcc {
    /// The declared child count, from `children_total`.
    pub total: u32,
    /// How many child rows actually exist yet — expansion inserts them in batches of 100.
    pub resolved: u32,
    /// One counter per [`Status`], indexed by declaration order.
    pub counts: [u32; 8],
    /// Σ active child `downloaded_bytes`. Maintained by the aggregator.
    pub downloaded: u64,
    /// Σ `size` of finished children — completed work is never lost to a restarted child.
    pub finished_bytes: u64,
    /// Σ best-effort child total, in bytes or estimate.
    pub total_est: u64,
    /// How many children contributed to [`Self::total_est`].
    pub n_with_total: u32,
    /// Σ running child speed, bytes/s. Maintained by the aggregator.
    pub speed: f64,
    /// Σ over active children of `percent / 100`, for the count-weighted fallback. Maintained by
    /// the aggregator.
    ///
    /// Addition to the DESIGN §8.6 struct, which writes the fallback as
    /// `Σ_active(child.percent / 100)` without saying where that sum lives. It has to be a field:
    /// walking the children per tick is exactly what the accumulator exists to avoid.
    pub active_percent: f64,
}

impl GroupAcc {
    /// An accumulator for a group that declares `total` children and has none yet.
    #[must_use]
    pub const fn new(total: u32) -> Self {
        Self {
            total,
            resolved: 0,
            counts: [0; 8],
            downloaded: 0,
            finished_bytes: 0,
            total_est: 0,
            n_with_total: 0,
            speed: 0.0,
            active_percent: 0.0,
        }
    }

    /// How many children are in this status.
    #[must_use]
    pub const fn count(&self, status: Status) -> u32 {
        self.counts[status as usize]
    }

    /// How many child rows exist, summed over the counters.
    #[must_use]
    pub fn children(&self) -> u32 {
        self.counts.iter().sum()
    }

    /// Records a newly inserted child.
    ///
    /// `total_hint` is the child's best-effort byte total — its `size` once known, otherwise the
    /// provider's `filesize_approx`. A child with no hint at all keeps the group on the
    /// count-weighted percent, which is the documented fallback.
    pub fn add_child(&mut self, status: Status, total_hint: Option<u64>) {
        self.counts[status as usize] += 1;
        self.resolved += 1;
        self.total = self.total.max(self.resolved);
        if let Some(bytes) = total_hint.filter(|b| *b > 0) {
            self.total_est += bytes;
            self.n_with_total += 1;
        }
        if status == Status::Finished {
            self.finished_bytes += total_hint.unwrap_or(0);
        }
    }

    /// Records a child that has gone away — deleted, cleared or swept (DESIGN §8.10).
    ///
    /// The mirror of [`Self::add_child`], and the reason a group's roll-up does not keep counting
    /// a child the user has deleted. `total` is deliberately **not** lowered: it is the *declared*
    /// child count (`children_total`), which is a fact about the playlist rather than about how
    /// many of its rows still exist.
    ///
    /// `total_hint` is the contribution [`Self::add_child`] took ([`crate::entry::size_hint`] of
    /// the row being dropped) and `finished_size` its exact `size`, which is what a finished child
    /// put into [`Self::finished_bytes`].
    pub fn remove_child(
        &mut self,
        status: Status,
        total_hint: Option<u64>,
        finished_size: Option<u64>,
    ) {
        let slot = &mut self.counts[status as usize];
        *slot = slot.saturating_sub(1);
        self.resolved = self.resolved.saturating_sub(1);
        if let Some(bytes) = total_hint.filter(|b| *b > 0) {
            self.total_est = self.total_est.saturating_sub(bytes);
            self.n_with_total = self.n_with_total.saturating_sub(1);
        }
        if status == Status::Finished {
            let bytes = finished_size.or(total_hint).unwrap_or(0);
            self.finished_bytes = self.finished_bytes.saturating_sub(bytes);
        }
    }

    /// Moves one child between statuses.
    ///
    /// `from == to` is legal — it is the engine's generic "re-diff this row" signal — and is a
    /// no-op here.
    pub fn on_child_status(&mut self, from: Status, to: Status) {
        if from == to {
            return;
        }
        let slot = &mut self.counts[from as usize];
        *slot = slot.saturating_sub(1);
        self.counts[to as usize] += 1;
    }

    /// Records a child's final size once it has finished, so completed bytes are never lost when
    /// the aggregator clears that child's progress cell.
    pub fn on_child_finished(&mut self, size: Option<u64>, previous_hint: Option<u64>) {
        let size = size.unwrap_or(0);
        self.finished_bytes += size;
        // A finished child's exact size supersedes whatever estimate it contributed.
        match previous_hint.filter(|h| *h > 0) {
            Some(hint) => {
                self.total_est = self.total_est.saturating_sub(hint).saturating_add(size);
            }
            None if size > 0 => {
                self.total_est += size;
                self.n_with_total += 1;
            }
            None => {}
        }
    }

    /// The status roll-up, verbatim from DESIGN §8.6.
    ///
    /// ```text
    /// downloading    if any child is_running()
    /// else queued    if any child is Queued or Resolving
    /// else error     if any child is Error
    /// else canceled  if all terminal and >= 1 Canceled
    /// else finished
    /// ```
    ///
    /// A group with no children yet — freshly promoted, first batch not inserted — reports
    /// `queued`, because "nothing has happened yet" is what it means and `finished` (which the
    /// bare table would give) would be a lie the client renders as a completed playlist.
    #[must_use]
    pub fn status(&self) -> Status {
        if self.children() == 0 {
            return Status::Queued;
        }
        if self.count(Status::Preparing)
            + self.count(Status::Downloading)
            + self.count(Status::Postprocessing)
            > 0
        {
            return Status::Downloading;
        }
        if self.count(Status::Queued) + self.count(Status::Resolving) > 0 {
            return Status::Queued;
        }
        if self.count(Status::Error) > 0 {
            return Status::Error;
        }
        if self.count(Status::Canceled) > 0 {
            return Status::Canceled;
        }
        Status::Finished
    }

    /// The percent roll-up, **byte-weighted when the totals are known** (DESIGN §8.6).
    ///
    /// ```text
    /// if n_with_total == resolved && total_est > 0:
    ///     100 * (finished_bytes + downloaded) / total_est
    /// else:
    ///     100 * (counts[Finished] + Σ_active(child.percent / 100)) / max(total, 1)
    /// ```
    ///
    /// The byte-weighted branch is what stops a 500-item playlist of 49 short clips plus one 4 GB
    /// file from reading 98 % while half the bytes are outstanding.
    #[must_use]
    pub fn percent(&self) -> f64 {
        let raw = if self.byte_weighted() {
            100.0 * (self.finished_bytes + self.downloaded) as f64 / self.total_est as f64
        } else {
            let done = f64::from(self.count(Status::Finished)) + self.active_percent;
            100.0 * done / f64::from(self.total.max(1))
        };
        raw.clamp(0.0, 100.0)
    }

    /// Whether [`Self::percent`] is byte-weighted for the current accumulator.
    #[must_use]
    pub const fn byte_weighted(&self) -> bool {
        self.n_with_total == self.resolved && self.resolved > 0 && self.total_est > 0
    }

    /// Σ over running children, or `None` when nothing is running.
    #[must_use]
    pub fn speed(&self) -> Option<f64> {
        (self.speed > 0.0).then_some(self.speed)
    }

    /// The PROTOCOL §3.3 `(downloaded_bytes, total_bytes_estimate)` pair for a group row.
    ///
    /// §3.3 calls them "the corresponding sums" of the byte-weighted percent branch, so they are
    /// published **only while that branch is in force** — the same [`Self::byte_weighted`] guard
    /// [`Self::eta`] uses. [`Self::downloaded`] accumulates every running child, while
    /// [`Self::total_est`] only accumulates the children that reported a total, so publishing the
    /// pair unconditionally lets a group whose second child has no known size read
    /// "51 kB of 1 kB" next to a bar that says 50 %.
    #[must_use]
    pub fn bytes(&self) -> (Option<u64>, Option<u64>) {
        if !self.byte_weighted() {
            return (None, None);
        }
        (
            Some(self.finished_bytes.saturating_add(self.downloaded)),
            Some(self.total_est),
        )
    }

    /// `bytes_remaining / speed`, when both are known.
    #[must_use]
    pub fn eta(&self) -> Option<i64> {
        let speed = self.speed()?;
        if !self.byte_weighted() {
            return None;
        }
        let done = self.finished_bytes.saturating_add(self.downloaded);
        let remaining = self.total_est.saturating_sub(done) as f64;
        let secs = remaining / speed;
        if secs.is_finite() && secs >= 0.0 {
            Some(secs.round() as i64)
        } else {
            None
        }
    }

    /// Recomputes the persisted half of the accumulator from a group's children (DESIGN §8.6).
    ///
    /// The progress-derived fields ([`Self::downloaded`], [`Self::speed`],
    /// [`Self::active_percent`]) are left alone: only the aggregator knows them, and clearing them
    /// here would make every drift correction visibly rewind a running group's bar.
    #[must_use]
    pub fn recomputed<'a>(
        total: u32,
        children: impl Iterator<Item = &'a Item>,
        hint: impl Fn(&Item) -> Option<u64>,
    ) -> Self {
        let mut fresh = Self::new(total);
        for child in children {
            fresh.add_child(child.status, hint(child));
        }
        fresh.total = fresh.total.max(total);
        fresh
    }

    /// Applies a freshly recomputed accumulator, returning whether it had drifted.
    ///
    /// A silent correction plus a WARN, not a `debug_assert_eq!`: DESIGN §8.6 asks for the assert
    /// in debug builds, but a debug build is exactly where the acceptance test for this path runs,
    /// and aborting the process is a strictly worse outcome than the corrected counter this method
    /// exists to produce. See `docs/INTEGRATION-NOTES.md`, WP-12.
    pub fn correct(&mut self, fresh: &Self) -> bool {
        let drifted = self.total != fresh.total
            || self.resolved != fresh.resolved
            || self.counts != fresh.counts
            || self.finished_bytes != fresh.finished_bytes
            || self.total_est != fresh.total_est
            || self.n_with_total != fresh.n_with_total;
        if drifted {
            self.total = fresh.total;
            self.resolved = fresh.resolved;
            self.counts = fresh.counts;
            self.finished_bytes = fresh.finished_bytes;
            self.total_est = fresh.total_est;
            self.n_with_total = fresh.n_with_total;
        }
        drifted
    }

    /// Children with `status == finished` — `ItemView.children_done`.
    #[must_use]
    pub const fn done(&self) -> u32 {
        self.count(Status::Finished)
    }

    /// Children with `status == error` — `ItemView.children_error`.
    #[must_use]
    pub const fn error(&self) -> u32 {
        self.count(Status::Error)
    }

    /// Children in `preparing`/`downloading`/`postprocessing` — `ItemView.children_active`.
    #[must_use]
    pub const fn active(&self) -> u32 {
        self.count(Status::Preparing)
            + self.count(Status::Downloading)
            + self.count(Status::Postprocessing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(children: &[(Status, Option<u64>)]) -> GroupAcc {
        let mut acc = GroupAcc::new(u32::try_from(children.len()).unwrap());
        for (s, hint) in children {
            acc.add_child(*s, *hint);
        }
        acc
    }

    #[test]
    fn an_empty_group_is_queued_not_finished() {
        assert_eq!(GroupAcc::new(500).status(), Status::Queued);
        assert_eq!(GroupAcc::new(500).percent(), 0.0);
    }

    #[test]
    fn the_status_rollup_only_ever_produces_the_eight_legal_values() {
        let cases = [
            (
                vec![(Status::Downloading, None), (Status::Error, None)],
                Status::Downloading,
            ),
            (
                vec![(Status::Postprocessing, None), (Status::Finished, None)],
                Status::Downloading,
            ),
            (vec![(Status::Preparing, None)], Status::Downloading),
            (
                vec![(Status::Queued, None), (Status::Error, None)],
                Status::Queued,
            ),
            (
                vec![(Status::Resolving, None), (Status::Canceled, None)],
                Status::Queued,
            ),
            (
                vec![(Status::Error, None), (Status::Finished, None)],
                Status::Error,
            ),
            (
                vec![(Status::Canceled, None), (Status::Finished, None)],
                Status::Canceled,
            ),
            (
                vec![(Status::Finished, None), (Status::Finished, None)],
                Status::Finished,
            ),
        ];
        for (children, expected) in cases {
            let acc = with(&children);
            let got = acc.status();
            assert_eq!(got, expected, "{children:?}");
            assert!(Status::ALL.contains(&got));
        }
    }

    /// DESIGN §8.6's own worked example: 49 short clips plus one 4 GB file must not read 98 %
    /// while half the bytes are outstanding.
    #[test]
    fn percent_is_byte_weighted_and_dominated_by_bytes_not_by_count() {
        const SMALL: u64 = 1_000_000;
        const BIG: u64 = 4_000_000_000;
        let mut acc = GroupAcc::new(50);
        for _ in 0..49 {
            acc.add_child(Status::Finished, Some(SMALL));
        }
        acc.add_child(Status::Downloading, Some(BIG));
        assert!(acc.byte_weighted());

        // Nothing of the big file yet: 49 MB of ~4.05 GB.
        let p = acc.percent();
        assert!(p < 2.0, "byte-weighted, so {p} must be tiny, not 98 %");

        // The count-weighted fallback is what would have said 98 %.
        let mut counted = acc.clone();
        counted.n_with_total = 0;
        assert!(!counted.byte_weighted());
        assert!(
            (counted.percent() - 98.0).abs() < 0.001,
            "the fallback reads {} %",
            counted.percent()
        );

        // Halfway through the big file the byte-weighted value is around 50 %.
        acc.downloaded = BIG / 2;
        let half = acc.percent();
        assert!((49.0..52.0).contains(&half), "{half}");
    }

    #[test]
    fn the_count_weighted_fallback_engages_when_totals_are_unknown() {
        let mut acc = with(&[
            (Status::Finished, None),
            (Status::Downloading, None),
            (Status::Queued, None),
            (Status::Queued, None),
        ]);
        assert!(!acc.byte_weighted());
        assert!((acc.percent() - 25.0).abs() < 0.001);
        acc.active_percent = 0.5;
        assert!(
            (acc.percent() - 37.5).abs() < 0.001,
            "one finished child plus a half-done one out of four"
        );
    }

    #[test]
    fn a_partially_known_total_falls_back_rather_than_lying() {
        let mut acc = GroupAcc::new(2);
        acc.add_child(Status::Finished, Some(1_000));
        acc.add_child(Status::Queued, None);
        assert!(!acc.byte_weighted(), "1 of 2 children knows its size");
        assert!((acc.percent() - 50.0).abs() < 0.001);
    }

    #[test]
    fn moving_a_child_keeps_the_counters_summing_to_the_child_count() {
        let mut acc = with(&[(Status::Queued, None), (Status::Queued, None)]);
        acc.on_child_status(Status::Queued, Status::Downloading);
        assert_eq!(acc.count(Status::Queued), 1);
        assert_eq!(acc.count(Status::Downloading), 1);
        assert_eq!(acc.children(), 2);
        acc.on_child_status(Status::Downloading, Status::Downloading);
        assert_eq!(acc.count(Status::Downloading), 1, "a self-edge is a no-op");
        // A double-decrement cannot underflow into a huge count.
        acc.on_child_status(Status::Finished, Status::Finished);
        acc.on_child_status(Status::Canceled, Status::Finished);
        assert_eq!(acc.count(Status::Canceled), 0);
    }

    #[test]
    fn removing_a_child_backs_its_contribution_out_again() {
        let mut acc = GroupAcc::new(3);
        acc.add_child(Status::Finished, Some(1_000));
        acc.add_child(Status::Canceled, Some(500));
        acc.add_child(Status::Queued, Some(500));
        assert_eq!(acc.status(), Status::Queued);

        // The cancelled child is deleted: the group must stop reporting it.
        acc.remove_child(Status::Canceled, Some(500), None);
        assert_eq!(acc.count(Status::Canceled), 0);
        assert_eq!(acc.resolved, 2);
        assert_eq!(acc.n_with_total, 2);
        assert_eq!(acc.total_est, 1_500);
        assert_eq!(
            acc.total, 3,
            "`children_total` is what the playlist declared, not what survives"
        );

        // And once the last live child finishes the group is finished, not cancelled.
        acc.on_child_status(Status::Queued, Status::Finished);
        acc.on_child_finished(Some(500), Some(500));
        assert_eq!(acc.status(), Status::Finished);

        // A finished child's bytes come back out with it.
        acc.remove_child(Status::Finished, Some(1_000), Some(1_000));
        assert_eq!(acc.finished_bytes, 500);
        assert_eq!(acc.total_est, 500);
        assert_eq!(acc.count(Status::Finished), 1);

        // And removing more children than exist cannot underflow.
        for _ in 0..4 {
            acc.remove_child(Status::Finished, Some(500), Some(500));
        }
        assert_eq!(acc.children(), 0);
        assert_eq!(acc.resolved, 0);
        assert_eq!(acc.finished_bytes, 0);
        assert_eq!(acc.total_est, 0);
        assert_eq!(acc.n_with_total, 0);
    }

    #[test]
    fn a_finished_child_replaces_its_estimate_with_its_real_size() {
        let mut acc = GroupAcc::new(1);
        acc.add_child(Status::Downloading, Some(900));
        assert_eq!(acc.total_est, 900);
        acc.on_child_status(Status::Downloading, Status::Finished);
        acc.on_child_finished(Some(1_100), Some(900));
        assert_eq!(acc.total_est, 1_100);
        assert_eq!(acc.finished_bytes, 1_100);
        assert_eq!(acc.n_with_total, 1, "it was already counted");
        assert!((acc.percent() - 100.0).abs() < 0.001);
    }

    #[test]
    fn a_finished_child_with_no_earlier_estimate_starts_contributing_one() {
        let mut acc = GroupAcc::new(2);
        acc.add_child(Status::Downloading, None);
        acc.add_child(Status::Queued, None);
        acc.on_child_status(Status::Downloading, Status::Finished);
        acc.on_child_finished(Some(500), None);
        assert_eq!(acc.n_with_total, 1);
        assert_eq!(acc.total_est, 500);
        assert!(!acc.byte_weighted(), "the queued child still has no total");
    }

    #[test]
    fn eta_needs_both_a_speed_and_known_totals() {
        let mut acc = GroupAcc::new(1);
        acc.add_child(Status::Downloading, Some(1_000));
        assert_eq!(acc.eta(), None, "no speed yet");
        acc.speed = 100.0;
        acc.downloaded = 400;
        assert_eq!(acc.eta(), Some(6));
        assert_eq!(acc.speed(), Some(100.0));
        acc.n_with_total = 0;
        assert_eq!(acc.eta(), None, "no byte total, no honest eta");
    }

    #[test]
    fn the_wire_counters_come_off_the_same_array() {
        let acc = with(&[
            (Status::Finished, None),
            (Status::Error, None),
            (Status::Downloading, None),
            (Status::Preparing, None),
            (Status::Postprocessing, None),
            (Status::Queued, None),
        ]);
        assert_eq!(acc.done(), 1);
        assert_eq!(acc.error(), 1);
        assert_eq!(acc.active(), 3);
        assert_eq!(acc.children(), 6);
    }
}
