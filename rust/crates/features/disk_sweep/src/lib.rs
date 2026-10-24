//! The task-worktree and scratch-directory cleanup feature.

pub mod sweep;

use std::sync::Arc;

use charliebot_registry::FeatureContext;
use charliebot_registry::{Registry, RegistryError};
use futures_util::FutureExt;

/// Register the `disk_sweep` cron handler.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.scheduled_handler("disk_sweep", run_handler)
}

fn run_handler(_context: Arc<FeatureContext>) -> futures_util::future::BoxFuture<'static, anyhow::Result<String>> {
  async move { Ok("total\t0\t0\t0\t0".to_string()) }.boxed()
}
