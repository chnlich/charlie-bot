//! Stopping the one task a service stores: abort it, wait for it, and read a cancelled join as the expected end.

use tokio::task::JoinHandle;

/// Cancel `task` and wait for it. A task that ended by cancellation is the expected end, and its join error is not
/// reported; a task that panicked is.
pub async fn cancel_and_wait(task: JoinHandle<()>) -> anyhow::Result<()> {
  task.abort();
  match task.await {
    Ok(()) => Ok(()),
    Err(error) if error.is_cancelled() => Ok(()),
    Err(error) => Err(error.into()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn a_cancelled_task_ends_the_stop_cleanly() {
    cancel_and_wait(tokio::spawn(std::future::pending::<()>())).await.unwrap();
  }

  #[tokio::test]
  async fn a_finished_task_ends_the_stop_cleanly() {
    cancel_and_wait(tokio::spawn(async {})).await.unwrap();
  }

  #[tokio::test]
  async fn a_panicked_task_is_the_stops_error() {
    let task = tokio::spawn(async { panic!("synthetic task panic") });
    tokio::task::yield_now().await;
    let error = cancel_and_wait(task).await.unwrap_err();
    assert!(error.to_string().contains("synthetic task panic"), "{error}");
  }
}
