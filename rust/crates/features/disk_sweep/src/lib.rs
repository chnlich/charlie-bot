//! The task-worktree and scratch-directory cleanup feature.

mod archive;
mod config;
mod git;
mod linked;
mod process;
mod references;
pub mod sweep;

use std::sync::Arc;

use charliebot_registry::FeatureContext;
use charliebot_registry::{Registry, RegistryError};
use chrono::Utc;
use futures_util::FutureExt;

/// `register`: the `disk_sweep:` config section and the scheduled handler `disk_sweep`, which the cron scheduler fires
/// for a task whose `handler` names it.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(config::section())?;
  registry.scheduled_handler("disk_sweep", run_handler)
}

fn run_handler(context: Arc<FeatureContext>) -> futures_util::future::BoxFuture<'static, anyhow::Result<String>> {
  async move {
    let cfg = context.config.get()?;
    let roots = sweep::SweepRoots::from_config(&cfg);
    let remove_worktree = context.registry.worktree_remover()?;
    let report = sweep::run_sweep(roots, sweep::SweepMode::Apply, Utc::now(), remove_worktree).await?;
    handler_result(&report)
  }
  .boxed()
}

/// The handler's result: a run with failed rows stays an error, but the summary a clean run reports follows the
/// failure list so the scheduler's error event still carries it.
fn handler_result(report: &sweep::SweepReport) -> anyhow::Result<String> {
  if !report.failures.is_empty() {
    let failures = report
      .failures
      .iter()
      .map(|failure| format!("{}: {}", failure.path.display(), failure.reason))
      .collect::<Vec<_>>()
      .join("; ");
    anyhow::bail!("disk_sweep failed: {failures}\n{}", report.apply_summary());
  }
  Ok(report.apply_summary())
}
