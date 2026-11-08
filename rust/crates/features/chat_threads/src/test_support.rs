//! Test fixtures shared by platform feature crates.

use std::sync::Arc;
use std::time::Duration;

use charliebot_registry::FeatureContext;
use charliebot_store::{ChatEvent, StoreResult};
use charliebot_tasks::completion::TaskCompletion;
use charliebot_tasks::dispatch::{DispatchExecutor, Dispatcher};
use futures_util::future::BoxFuture;

struct NoopExecutor;

impl DispatchExecutor for NoopExecutor {
  fn launch<'a>(
    &'a self,
    _session_id: &'a str,
    _pending_inputs: &'a [ChatEvent],
    _launch_run_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<Option<String>>> {
    Box::pin(async { Ok(None) })
  }
}

/// Install the dispatcher a platform test home needs without launching agent processes.
pub fn install_test_dispatcher(features: &Arc<FeatureContext>) {
  let completion = Arc::new(TaskCompletion::new(Arc::clone(features)));
  features.startup.insert(Arc::clone(&completion)).expect("the completion owner inserts once");
  let sequence: Arc<dyn charliebot_registry::sequence::SequenceCompletion> = completion.clone();
  let run: Arc<dyn charliebot_tasks::dispatch::RunFinishedCompletion> = completion;
  let dispatcher = Dispatcher::new(Arc::clone(&features.store), Arc::clone(features), sequence, run);
  dispatcher.set_executor(Some(Arc::new(NoopExecutor)));
  features.startup.insert(dispatcher).expect("the task dispatcher inserts once");
}

/// Restores the production keyed-post retry delays when dropped (`post_with_retry`).
pub struct KeyedPostRetryDelays;

impl Drop for KeyedPostRetryDelays {
  fn drop(&mut self) {
    crate::thread_entry::clear_keyed_post_retry_delays();
  }
}

/// Replaces the keyed-post retry delays `[1 s, 4 s]` with `delays` until the guard drops, so a test
/// waits out a retry over a real loopback peer without real seconds.
pub fn keyed_post_retry_delays_for_test(delays: [Duration; 2]) -> KeyedPostRetryDelays {
  crate::thread_entry::install_keyed_post_retry_delays(delays);
  KeyedPostRetryDelays
}
