//! The session's usage projection: backend resolver precedence, event-fold tiers, and context state.
//!
//! The vocabulary below is the one this crate's comments use.
//!
//! - A *fold* is the running total of usage facts accumulated from one session's chat events.
//! - A *reading* is the context figure a fold resolves: chosen prompt tokens, a snapshot, or resolved values.
//! - The *reading kind* records which event kind last supplied a reading: `Unset`, `Claude`, `Snapshot`, or `Resolved`.
//! - A *tier* is an ordered source of readings; the backend resolver comes before the event-only kinds.
//! - A *compact boundary* is the system event whose subtype is `compact_boundary`, carrying the
//!   post-compaction token count.
//! - The *compaction line* is the context fill at which a compaction happens, reported as `context_compact_at`.

mod usage;

pub use usage::{SessionUsage, register, usage_run_kind};
