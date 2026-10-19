//! `src/features/diag`: the write-only session-switch telemetry route. The web UI posts each session-switch phase to
//! `POST /api/diag/switch-events`, and the route logs the point as one `diag_switch` line. No state is kept.

mod api;

use charliebot_registry::{Registry, RegistryError};

/// `register` (`src/features/diag/__init__.py`): the `/api/diag` router, nothing else.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)
}

#[cfg(test)]
mod tests {
  use charliebot_registry::Registry;

  use super::*;

  #[test]
  fn register_mounts_the_switch_events_router() {
    let mut registry = Registry::new();
    registry.register_package("src.features.diag", register).expect("the package registers");
    assert_eq!(registry.routers().len(), 1);
  }
}
