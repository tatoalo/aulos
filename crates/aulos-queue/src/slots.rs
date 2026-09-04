//! Slots: the global download cap, the per-provider caps, and the resolution pool (DESIGN §8.7).
//!
//! Two rules do all the work:
//!
//! - A provider that declares [`aulos_provider::Provider::own_slots`] acquires **its own** permit
//!   *instead of* the global one, exactly as legacy's `sc_semaphore` did. A queued
//!   StreamingCommunity download therefore never holds a global slot.
//! - Resolution has a pool of its own, so a 500-item playlist's metadata work can never starve
//!   downloads.

use std::collections::HashMap;
use std::sync::Arc;

use aulos_core::{Config, ProviderId};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A held slot: dropping it frees the permit (DESIGN §8.7).
pub type Slot = OwnedSemaphorePermit;

/// The three slot pools (DESIGN §8.2, §8.7).
#[derive(Debug)]
pub struct Slots {
    global: Arc<Semaphore>,
    per_provider: HashMap<ProviderId, Arc<Semaphore>>,
    resolve: Arc<Semaphore>,
    global_permits: usize,
}

impl Slots {
    /// The pools sized from the effective configuration.
    ///
    /// `MAX_CONCURRENT_DOWNLOADS` and `AULOS_RESOLVE_CONCURRENCY` are clamped to at least 1: a
    /// zero-permit semaphore is a deadlock, not a configuration.
    #[must_use]
    pub fn from_config(cfg: &Config) -> Self {
        let global = (cfg.max_concurrent_downloads.max(1)) as usize;
        Self {
            global: Arc::new(Semaphore::new(global)),
            per_provider: HashMap::new(),
            resolve: Arc::new(Semaphore::new((cfg.resolve_concurrency.max(1)) as usize)),
            global_permits: global,
        }
    }

    /// The resolution pool, as an `Arc` a spawned resolve task can hold.
    #[must_use]
    pub fn resolve_pool(&self) -> Arc<Semaphore> {
        Arc::clone(&self.resolve)
    }

    /// `MAX_CONCURRENT_DOWNLOADS`, as configured.
    #[must_use]
    pub const fn global_permits(&self) -> usize {
        self.global_permits
    }

    /// How many global download slots are free right now.
    #[must_use]
    pub fn global_available(&self) -> usize {
        self.global.available_permits()
    }

    /// How many of one provider's own slots are free, or `None` if it has none.
    #[must_use]
    pub fn provider_available(&self, id: &ProviderId) -> Option<usize> {
        self.per_provider.get(id).map(|s| s.available_permits())
    }

    /// Tries to take the slot this item needs, without blocking (DESIGN §8.7).
    ///
    /// `own_slots` is the provider's [`aulos_provider::Provider::own_slots`] answer: `Some(n)`
    /// takes from that provider's own pool — creating it on first use — and never touches the
    /// global semaphore; `None` takes a global permit.
    pub fn try_acquire(&mut self, id: &ProviderId, own_slots: Option<usize>) -> Option<Slot> {
        match own_slots {
            Some(n) => {
                let pool = self
                    .per_provider
                    .entry(id.clone())
                    .or_insert_with(|| Arc::new(Semaphore::new(n.max(1))));
                Arc::clone(pool).try_acquire_owned().ok()
            }
            None => Arc::clone(&self.global).try_acquire_owned().ok(),
        }
    }

    /// Closes every pool, so a task waiting for a resolve permit wakes up at shutdown.
    pub fn close(&self) {
        self.global.close();
        self.resolve.close();
        for pool in self.per_provider.values() {
            pool.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use aulos_core::config::{RawEnv, load};

    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let mut env = vec![("STATE_DIR", "/tmp"), ("DOWNLOAD_DIR", "/tmp")];
        env.extend_from_slice(pairs);
        load(&RawEnv::from_pairs(env)).expect("the test config must load")
    }

    fn id(s: &str) -> ProviderId {
        ProviderId::parse(s).unwrap()
    }

    #[tokio::test]
    async fn own_slots_providers_bypass_the_global_semaphore() {
        let mut slots = Slots::from_config(&cfg(&[("MAX_CONCURRENT_DOWNLOADS", "1")]));
        let sc = slots.try_acquire(&id("streamingcommunity"), Some(3));
        assert!(sc.is_some());
        assert_eq!(
            slots.global_available(),
            1,
            "the global slot was not touched"
        );
        // ...and a yt-dlp item can still start alongside it.
        let ytdlp = slots.try_acquire(&id("ytdlp"), None);
        assert!(ytdlp.is_some());
        assert_eq!(slots.global_available(), 0);
        drop((sc, ytdlp));
    }

    #[tokio::test]
    async fn the_global_cap_is_never_exceeded() {
        let mut slots = Slots::from_config(&cfg(&[("MAX_CONCURRENT_DOWNLOADS", "2")]));
        let held: Vec<_> = (0..2)
            .filter_map(|_| slots.try_acquire(&id("ytdlp"), None))
            .collect();
        assert_eq!(held.len(), 2);
        assert!(slots.try_acquire(&id("ytdlp"), None).is_none());
        drop(held);
        assert!(slots.try_acquire(&id("ytdlp"), None).is_some());
    }

    #[tokio::test]
    async fn a_provider_pool_is_capped_at_its_own_number() {
        let mut slots = Slots::from_config(&cfg(&[("MAX_CONCURRENT_DOWNLOADS", "8")]));
        let sc = id("streamingcommunity");
        let held: Vec<_> = (0..2)
            .filter_map(|_| slots.try_acquire(&sc, Some(2)))
            .collect();
        assert_eq!(held.len(), 2);
        assert!(slots.try_acquire(&sc, Some(2)).is_none());
        assert_eq!(slots.provider_available(&sc), Some(0));
        assert_eq!(slots.provider_available(&id("ytdlp")), None);
        assert_eq!(slots.global_available(), 8, "still untouched");
    }

    #[tokio::test]
    async fn a_zero_own_slots_answer_is_clamped_rather_than_deadlocking() {
        // The configuration validator already rejects `MAX_CONCURRENT_DOWNLOADS=0`
        // (`OutOfRange { min: 1 }`), so the only zero that can reach the pools is a provider
        // answering `own_slots() == Some(0)`, which must not wedge that provider forever.
        let mut slots = Slots::from_config(&cfg(&[("MAX_CONCURRENT_DOWNLOADS", "1")]));
        assert_eq!(slots.global_permits(), 1);
        let held = slots.try_acquire(&id("x"), Some(0));
        assert!(held.is_some(), "Some(0) is clamped to one permit");
        assert!(slots.try_acquire(&id("x"), Some(0)).is_none());
        assert_eq!(slots.global_available(), 1, "and never the global pool");
    }

    #[tokio::test]
    async fn closing_wakes_a_waiting_resolver() {
        let slots = Slots::from_config(&cfg(&[]));
        let pool = slots.resolve_pool();
        slots.close();
        assert!(pool.acquire_owned().await.is_err());
    }
}
