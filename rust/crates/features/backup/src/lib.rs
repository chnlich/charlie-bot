//! The scheduled profile backup to the configured restic repository and its cron handler.

pub mod backup;
pub mod config;

use std::sync::Arc;

use charliebot_registry::{FeatureContext, Registry, RegistryError};
use futures_util::FutureExt;

/// `register`: the `backup` config section and the scheduled handler `backup`, which the cron scheduler fires for a
/// task whose `handler` names it.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(config::section())?;
  registry.scheduled_handler("backup", run_backup)
}

/// The handler: one snapshot and its retention pass off the async runtime, then a DM to the host owner when either
/// fails. A successful run sends nothing.
fn run_backup(context: Arc<FeatureContext>) -> futures_util::future::BoxFuture<'static, anyhow::Result<String>> {
  async move {
    let features = Arc::clone(&context);
    let result = tokio::task::spawn_blocking(move || run_snapshot(&features)).await.expect("the backup task panicked");
    if let Err(error) = &result {
      notify_failure(&context, error).await;
    }
    result
  }
  .boxed()
}

fn run_snapshot(features: &FeatureContext) -> anyhow::Result<String> {
  let cfg = features.config.get()?;
  let settings = cfg.section::<config::BackupConfig>(config::NAME).settings(&cfg.charliebot_home)?;
  settings.backup(&cfg.charliebot_home)?;
  Ok(format!("restic backup of {} to {} completed", cfg.charliebot_home.display(), settings.repository))
}

/// One DM to the host owner naming the failure. A DM that cannot be sent is logged and never replaces the backup's own
/// error.
async fn notify_failure(features: &FeatureContext, error: &anyhow::Error) {
  let text = format!("CharlieBot backup failed: {error:#}");
  let sent = match features.registry.owner_dm_sender() {
    Ok(send) => send(features, &text).await,
    Err(error) => Err(error),
  };
  match sent {
    Ok(ts) => tracing::info!(ts = %ts, "backup_failure_dm_sent"),
    Err(error) => tracing::warn!(error = %error, "backup_failure_dm_failed"),
  }
}

#[cfg(test)]
mod lib_tests;
