//! The four derived scheduling classes (DESIGN §8.2).
//!
//! Priority is never *requested*. It is derived from where the item came from, which is why a link
//! you just pasted starts next instead of queueing behind 486 playlist children. Within a class the
//! order is `ord` ascending — FIFO, i.e. legacy behaviour.

use aulos_core::SourceKind;

/// The scheduling class of a queued item (DESIGN §8.2).
///
/// The derived [`Ord`] is the scan order: `Retry` first, `Bulk` last.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Priority {
    /// A retry, manual or automatic.
    Retry = 0,
    /// A direct add from API v2, API v1 or Telegram that is not part of a group.
    Interactive = 1,
    /// A subscription check produced it.
    Subscription = 2,
    /// A playlist/channel child — any group member.
    Bulk = 3,
}

impl Priority {
    /// Every class, in scan order.
    pub const ALL: [Self; 4] = [
        Self::Retry,
        Self::Interactive,
        Self::Subscription,
        Self::Bulk,
    ];

    /// How many classes there are — the length of the engine's ready-deque array.
    pub const COUNT: usize = 4;

    /// The index of this class in the ready-deque array.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// A stable name for logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Interactive => "interactive",
            Self::Subscription => "subscription",
            Self::Bulk => "bulk",
        }
    }

    /// The DESIGN §8.2 table, as a function.
    ///
    /// The tests read it as four rules applied in order: a retry is a retry whatever else it is; a
    /// group member is `Bulk`; a subscription's own single video is `Subscription`; everything else
    /// is `Interactive`.
    ///
    /// `retried` is the caller's `item.attempt > 0`, and it exists because a retry no longer
    /// overwrites `item.source` (DESIGN §4.4): the origin has to survive so per-origin routing can
    /// still see who asked, which means the *fact of being a retry* has to arrive some other way.
    /// `SourceKind::Retry` is still honoured for rows an older build wrote.
    ///
    /// `SourceKind::Restart` — which the table omits because boot recovery re-queues items that
    /// already had a class — falls through to the group rule, so restarting a 500-child playlist
    /// does not promote 500 children to `Interactive`. Recovery bumps `attempt` only on the
    /// handful of rows that were actually in flight, so only those resume at `Retry`; the queued
    /// backlog keeps `attempt == 0` and stays `Bulk`.
    #[must_use]
    pub const fn of(source: SourceKind, in_group: bool, retried: bool) -> Self {
        match source {
            _ if retried => Self::Retry,
            SourceKind::Retry => Self::Retry,
            _ if in_group => Self::Bulk,
            SourceKind::Subscription => Self::Subscription,
            SourceKind::ApiV1
            | SourceKind::ApiV2
            | SourceKind::Ios
            | SourceKind::Telegram
            | SourceKind::Restart => Self::Interactive,
        }
    }
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scan_order_is_the_derived_ord() {
        let mut v = vec![
            Priority::Bulk,
            Priority::Retry,
            Priority::Subscription,
            Priority::Interactive,
        ];
        v.sort_unstable();
        assert_eq!(v, Priority::ALL);
        assert!(Priority::Retry < Priority::Interactive);
        for (i, p) in Priority::ALL.into_iter().enumerate() {
            assert_eq!(p.index(), i);
        }
        assert_eq!(Priority::COUNT, Priority::ALL.len());
    }

    #[test]
    fn the_design_table_holds() {
        // A retry wins over everything, group member or not — and the fact now arrives as the
        // `retried` flag, whatever the item's origin says.
        for k in SourceKind::ALL {
            assert_eq!(Priority::of(k, false, true), Priority::Retry, "{k} retried");
            assert_eq!(
                Priority::of(k, true, true),
                Priority::Retry,
                "{k} retried, in a group"
            );
        }
        // A row an older build re-attributed to `retry` is still a retry.
        assert_eq!(
            Priority::of(SourceKind::Retry, false, false),
            Priority::Retry
        );
        assert_eq!(
            Priority::of(SourceKind::Retry, true, false),
            Priority::Retry
        );
        // Any group member is Bulk.
        for k in [
            SourceKind::ApiV1,
            SourceKind::ApiV2,
            SourceKind::Ios,
            SourceKind::Telegram,
            SourceKind::Subscription,
            SourceKind::Restart,
        ] {
            assert_eq!(
                Priority::of(k, true, false),
                Priority::Bulk,
                "{k} in a group"
            );
        }
        // A subscription's own single video.
        assert_eq!(
            Priority::of(SourceKind::Subscription, false, false),
            Priority::Subscription
        );
        // Direct adds — the iOS app enters exactly where a pasted link does.
        for k in [
            SourceKind::ApiV1,
            SourceKind::ApiV2,
            SourceKind::Ios,
            SourceKind::Telegram,
            SourceKind::Restart,
        ] {
            assert_eq!(Priority::of(k, false, false), Priority::Interactive, "{k}");
        }
    }

    #[test]
    fn every_class_has_a_distinct_name() {
        let mut names: Vec<_> = Priority::ALL.iter().map(|p| p.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Priority::COUNT);
        assert_eq!(Priority::Bulk.to_string(), "bulk");
    }
}
