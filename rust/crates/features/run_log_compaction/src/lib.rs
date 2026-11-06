//! The `run_log_compaction` cron handler: a finished run's log files become `.zst` once its end is a day old.

pub mod compaction;

use std::sync::Arc;

use charliebot_registry::FeatureContext;
use charliebot_registry::{Registry, RegistryError};
use futures_util::FutureExt;

/// Register the `run_log_compaction` cron handler.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.scheduled_handler("run_log_compaction", run_handler)
}

fn run_handler(context: Arc<FeatureContext>) -> futures_util::future::BoxFuture<'static, anyhow::Result<String>> {
  async move {
    let sessions_dir = context.config.get()?.sessions_dir();
    compaction::run(sessions_dir).await
  }
  .boxed()
}
