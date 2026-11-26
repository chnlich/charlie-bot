//! The run-log-compaction package's registration: the cron handler that compacts finished run logs.

use std::sync::Arc;

use charliebot_registry::FeatureContext;
use charliebot_registry::{Registry, RegistryError};
use futures_util::FutureExt;

use crate::compaction;

/// The registration of the `run_log_compaction` cron handler.
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
