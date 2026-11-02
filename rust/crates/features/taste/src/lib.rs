//! `web/features/taste`: the user's choice precedents. The crate owns the append-only store at
//! `$CHARLIEBOT_HOME/taste/rows.jsonl`, the `taste` CLI, and the turn contribution that puts the table's index into
//! every manager and worker run.

pub mod candidates;
pub mod cli;
pub mod store;
mod turn_contribution;

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};
use turn_contribution::TasteTurn;

/// The taste package's registration: its turn contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.turn_contribution("taste", Arc::new(TasteTurn))
}
