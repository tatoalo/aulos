//! Subscriptions: the record and its public projection, a per-subscription scheduler with jitter
//! and exponential backoff, bounded-concurrency checks that start shortly after boot, and the
//! check algorithm that turns newly-seen entries into queue items.
//!
//! See DESIGN §14.
