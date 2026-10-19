//! `src/features/backup`: the scheduled home backup, its retention policy and its cron handler.

pub mod backup;
pub mod tar_writer;

use std::sync::Arc;

use charliebot_registry::context::FeatureContext;
use charliebot_registry::{Registry, RegistryError};
use futures_util::FutureExt;

/// `register` (`src/features/backup/__init__.py`): the scheduled handler `backup`, which the cron
/// scheduler fires for a task whose `handler` names it.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.scheduled_handler("backup", run_backup)
}

/// `run_scheduled_backup`: both steps run off the async runtime, and the fire returns the archive path.
fn run_backup(_context: Arc<FeatureContext>) -> futures_util::future::BoxFuture<'static, anyhow::Result<String>> {
  async move {
    let archive = tokio::task::spawn_blocking(backup::run_backup_blocking).await.expect("the backup task panicked")?;
    tracing::info!(archive = %archive.display(), "backup_handler_done");
    Ok(archive.to_string_lossy().into_owned())
  }
  .boxed()
}

#[cfg(test)]
mod lib_tests;
