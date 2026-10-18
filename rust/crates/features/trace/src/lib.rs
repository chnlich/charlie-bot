//! The Perfetto trace viewer and merged-trace cache.

mod api;
mod merge;

use std::sync::Arc;

use charliebot_registry::wiring::{Phase, Service, ServiceContext};
use charliebot_registry::{Registry, RegistryError};

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::router, api::matches)?;
  registry.service(Arc::new(MergePool))?;
  Ok(())
}

struct MergePool;

impl Service for MergePool {
  fn name(&self) -> &'static str {
    "merge_pool"
  }

  fn phase(&self) -> Phase {
    Phase::Ready
  }

  fn start<'a>(&'a self, _ctx: ServiceContext) -> futures_util::future::BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  fn stop<'a>(&'a self) -> futures_util::future::BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }
}
