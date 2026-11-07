//! The scheduled home backup, its retention policy and its cron handler.

use std::path::{Path, PathBuf};

pub mod backup;
pub mod config;

use std::sync::Arc;

use charliebot_registry::context::FeatureContext;
use charliebot_registry::{Registry, RegistryError};
use charliebot_slack::slack_listener::SlackClient;
use config::BackupConfig;
use futures_util::FutureExt;

/// `register`: the `backup:` config section and the scheduled handler `backup`, which the cron
/// scheduler fires for a task whose `handler` names it.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(config::section())?;
  registry.scheduled_handler("backup", run_backup)
}

/// One failed step of the backup: the step's name and the cause.
pub(crate) struct StepFailure {
  pub step: &'static str,
  pub error: anyhow::Error,
}

/// The steps of the scheduled backup, in order: the preflight (restic on `PATH`, the password file
/// present), `restic unlock` against the repository (a server stop can kill a running backup's
/// restic process, and its leftover lock makes `forget` refuse the repository), `restic backup` of
/// the profile home, and `restic forget` with the retention policy. The first failing step stops
/// the run.
pub(crate) fn run_steps_in(home: &Path, config: &BackupConfig) -> Result<String, StepFailure> {
  let step = "preflight";
  let repository = config.repository().map_err(|error| StepFailure { step, error })?;
  let password_file = config.password_file().map_err(|error| StepFailure { step, error })?;
  backup::preflight(&password_file).map_err(|error| StepFailure { step, error })?;
  let step = "unlock";
  backup::remove_stale_locks(&repository, &password_file).map_err(|error| StepFailure { step, error })?;
  let step = "backup";
  let snapshot =
    backup::create_snapshot(home, &repository, &password_file).map_err(|error| StepFailure { step, error })?;
  let step = "forget";
  backup::apply_retention(&repository, &password_file).map_err(|error| StepFailure { step, error })?;
  Ok(format!("snapshot {snapshot}"))
}

/// The notice one failed backup sends: the step and the cause.
pub(crate) fn failure_message(failure: &StepFailure) -> String {
  format!("CharlieBot 备份在 {} 步失败：{:#}", failure.step, failure.error)
}

/// The handler's wiring for the home `home`: the steps run off the async runtime, and `notify` runs
/// once — for the first failed step only. A successful run never notifies.
pub(crate) async fn run_with_notice<N, Fut>(home: PathBuf, config: BackupConfig, notify: N) -> anyhow::Result<String>
where
  N: FnOnce(String) -> Fut,
  Fut: std::future::Future<Output = ()>,
{
  let outcome =
    tokio::task::spawn_blocking(move || run_steps_in(&home, &config)).await.expect("the backup task panicked");
  match outcome {
    Ok(summary) => {
      tracing::info!(summary = %summary, "backup_handler_done");
      Ok(summary)
    }
    Err(failure) => {
      tracing::error!(step = failure.step, error = %failure.error, "backup_handler_failed");
      notify(failure_message(&failure)).await;
      Err(failure.error)
    }
  }
}

/// The handler: run the steps, and on a failure send one Slack direct message to the configured user
/// through the bot path (`SlackClient::post_dm`), naming the step and the cause.
fn run_backup(context: Arc<FeatureContext>) -> futures_util::future::BoxFuture<'static, anyhow::Result<String>> {
  async move {
    let config = context
      .config
      .get()
      .map_err(|error| anyhow::anyhow!("the config reload failed: {error}"))?
      .section::<BackupConfig>("backup")
      .clone();
    let notify_user_id = config.notify_user_id.clone();
    let notice_context = Arc::clone(&context);
    let home = backup::charliebot_dir()?;
    run_with_notice(home, config, move |message| {
      let context = Arc::clone(&notice_context);
      let user_id = notify_user_id.clone();
      async move { send_failure_notice(&context, user_id, message).await }
    })
    .await
  }
  .boxed()
}

/// Sends the failure notice. An unresolvable notice path — no configured user, no Slack credentials,
/// a failed send — is logged and never masks the backup error.
async fn send_failure_notice(context: &FeatureContext, user_id: Option<String>, message: String) {
  let Some(user_id) = user_id else {
    tracing::error!("backup_notice_skipped_no_notify_user_id");
    return;
  };
  let client = match SlackClient::from_features(context) {
    Ok(client) => client,
    Err(error) => {
      tracing::error!(error = %error, "backup_notice_client_failed");
      return;
    }
  };
  if let Err(error) = client.post_dm(&user_id, &message).await {
    tracing::error!(error = %error, "backup_notice_send_failed");
  }
}

#[cfg(test)]
mod lib_tests;
