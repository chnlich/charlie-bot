//! The backup feature's registration: the `backup` config section and its scheduled handler.

use std::sync::Arc;

use charliebot_registry::{FeatureContext, Registry, RegistryError};
use futures_util::FutureExt;

use crate::backup::run_snapshot;
use crate::config;

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
