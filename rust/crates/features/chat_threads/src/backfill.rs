//! The boot backfill both chat channels run once the crash-recovery step has had its chance (`backfill.run_backfill`).

use std::future::Future;

use futures_util::future::BoxFuture;
use futures_util::future::Shared;

/// Report one platform's summons lost across the restart, once crash recovery has finished.
///
/// Waits on the crash-recovery future first, so re-attach and the user-message replay have answered everything they
/// can. Whatever is still unanswered after that is lost, and gets a notice in its thread.
pub async fn run_backfill<F, Fut>(
  recovery_done: Shared<BoxFuture<'static, ()>>,
  backfill_lost_summons: F,
  platform: &str,
) -> anyhow::Result<()>
where
  F: FnOnce() -> Fut,
  Fut: Future<Output = anyhow::Result<usize>>,
{
  recovery_done.await;
  let reported = backfill_lost_summons().await?;
  tracing::info!(event = %format!("{platform}_backfill_done"), count = reported);
  Ok(())
}
