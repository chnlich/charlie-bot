//! The write-only client timing telemetry route. The web UI posts one client timing event per
//! interaction — a session switch, a message send, a socket disconnect episode, a page load or one
//! tab-minute of background traffic — to `POST /api/diag/client-events`, and the route logs the
//! event as one `diag_client` line. No state is kept.

mod api;

use charliebot_registry::{Registry, RegistryError};

/// `register`: the `/api/diag` router, nothing else.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)
}

#[cfg(test)]
mod tests {
  use charliebot_registry::Registry;

  use super::*;

  #[test]
  fn register_mounts_the_client_events_router() {
    let mut registry = Registry::new();
    registry.register_package("features.diag", register).expect("the package registers");
    assert_eq!(registry.routers().len(), 1);
  }
}
