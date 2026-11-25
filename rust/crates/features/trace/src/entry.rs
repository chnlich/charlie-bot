//! The trace feature's entry for the package registry: `register` adds the Perfetto router and the merge-pool service.

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};

use crate::api;
use crate::merge::MergePool;

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::router, api::matches)?;
  registry.service(Arc::new(MergePool))?;
  Ok(())
}
