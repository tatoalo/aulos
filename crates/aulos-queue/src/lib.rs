//! The queue engine: the command/event loop, global and per-provider slots, the bounded resolution
//! pool, playlist expansion into groups, dedupe, cooperative cancellation, boot recovery, the
//! clear/delete paths and the watchdogs — plus the realtime side: the delta aggregator, the event
//! hub with its monotonic `seq` and replay ring, and the lock-free published snapshot.
//!
//! See DESIGN §8 and §15.
