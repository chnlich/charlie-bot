//! `web/features/drafts` holds the approval card of a message or publication in the operator's name, and the
//! operator-only send route. It also holds the per-draft send lock and the three session events
//! (`draft_registered`, `draft_sent`, `draft_failed`).
//!
//! A platform's target validation and sender live in that platform's own feature module and reach this package only
//! through the [`charliebot_registry::draft::DraftPlatform`] trait. A new platform adds only its own module.

mod api;
mod card;
mod entry;
mod lock;
mod log;
mod turn_contribution;

pub use entry::register;

#[cfg(test)]
mod test_support;
