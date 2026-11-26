//! The drafts package's registrations: the draft routes and the turn contribution.

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};

use crate::api;
use crate::turn_contribution::DraftsTurnContribution;

/// `register`: the two draft routes and the three chat renderers.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router_patterns(api::routes, api::ROUTE_PATTERNS)?;
  registry.turn_contribution("drafts", Arc::new(DraftsTurnContribution))
}
