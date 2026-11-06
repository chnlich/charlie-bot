//! `web/features/drafts` holds the approval card of a message or publication in the operator's name, and the
//! operator-only send route. It also holds the per-draft send lock and the three session events
//! (`draft_registered`, `draft_sent`, `draft_failed`).
//!
//! A platform's target validation and sender live in that platform's own feature module and reach this package only
//! through the [`charliebot_registry::draft::DraftPlatform`] trait. A new platform adds only its own module.

pub mod api;
pub mod card;
pub mod lock;
pub mod log;
mod turn_contribution;

#[cfg(test)]
mod test_support;

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};
use turn_contribution::DraftsTurnContribution;

/// `register`: the two draft routes and the three chat renderers.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.turn_contribution("drafts", Arc::new(DraftsTurnContribution))
}
