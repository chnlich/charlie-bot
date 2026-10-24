//! The task-worktree and scratch-directory cleanup feature.

mod archive;
mod git;
mod process;
pub mod sweep;

use std::sync::Arc;

use charliebot_registry::FeatureContext;
use charliebot_registry::{Registry, RegistryError};
use chrono::Utc;
use futures_util::FutureExt;

/// Register the `disk_sweep` cron handler.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.scheduled_handler("disk_sweep", run_handler)
}

fn run_handler(context: Arc<FeatureContext>) -> futures_util::future::BoxFuture<'static, anyhow::Result<String>> {
  async move {
    let cfg = context.config.get()?;
    let roots = sweep::SweepRoots::from_config(&cfg);
    let report = sweep::run_sweep(roots, true, Utc::now()).await?;
    if !report.failures.is_empty() {
      let failures = report
        .failures
        .iter()
        .map(|failure| format!("{}: {}", failure.path.display(), failure.reason))
        .collect::<Vec<_>>()
        .join("; ");
      anyhow::bail!("disk_sweep failed: {failures}");
    }
    Ok(report.total_line())
  }
  .boxed()
}
