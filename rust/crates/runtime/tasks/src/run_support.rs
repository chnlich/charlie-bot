//! Helpers that the manager turn (`master_run`) and the task run (`worker`) share: the stderr tail, the subprocess
//! buffer limit, the resume routing of a backend type, the extra flags of a launch, and the event channel of a runner.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use charliebot_agents::runner::EventFn;
use charliebot_agents::transport::follow::SilenceCallback;
use charliebot_agents::transport::spawn;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::{EXTRA_FLAGS_BACKEND_KWARG, RecoverySilence, ResumeStyle};
use charliebot_registry::context::FeatureContext;
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};

/// One event handed from a runner to its consumer, with the channel the consumer acknowledges it on.
pub(crate) struct StreamEvent {
  pub event: Value,
  pub acknowledged: oneshot::Sender<Result<(), String>>,
}

/// The runner's event callback and the receiver its consumer reads. The callback returns only after the consumer
/// acknowledges the event, so the follow never passes an event the consumer has not processed. `stopped` and
/// `unacknowledged` are the errors of a consumer that has gone and of one that dropped its acknowledgement.
pub(crate) fn stream_event_channel(
  stopped: &'static str,
  unacknowledged: &'static str,
) -> (EventFn, mpsc::Receiver<StreamEvent>) {
  let (sender, receiver) = mpsc::channel::<StreamEvent>(1);
  let on_event: EventFn = Arc::new(move |event| {
    let sender = sender.clone();
    Box::pin(async move {
      let (ack_tx, ack_rx) = oneshot::channel();
      sender.send(StreamEvent { event, acknowledged: ack_tx }).await.map_err(|_| anyhow!(stopped))?;
      ack_rx.await.context(unacknowledged)?.map_err(anyhow::Error::msg)
    })
  });
  (on_event, receiver)
}

/// The last `spawn::STDERR_TAIL_BYTES` of a process's stderr log, trimmed; empty when the log does not exist.
pub(crate) async fn read_stderr_tail(path: &Path) -> anyhow::Result<String> {
  let path = path.to_path_buf();
  tokio::task::spawn_blocking(move || match spawn::read_stderr_tail(&path) {
    Ok(text) => Ok(text),
    Err(error) if missing_log(&error) => Ok(String::new()),
    Err(error) => Err(error),
  })
  .await
  .context("joining stderr tail reader")?
}

/// Whether `error` is the open refusing because a run that wrote no stderr left no log behind.
fn missing_log(error: &anyhow::Error) -> bool {
  error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

/// The subprocess buffer limit of the configuration, in bytes.
pub(crate) fn subprocess_buffer_bytes(cfg: &CharlieBotConfig) -> anyhow::Result<usize> {
  let megabytes = usize::try_from(cfg.server.subprocess_buffer_limit_mb).context("negative subprocess buffer limit")?;
  megabytes.checked_mul(1024 * 1024).context("subprocess buffer limit overflows usize")
}

/// The flags and the native conversation id that resuming `cc_session_id` takes on `option`'s backend type.
pub(crate) fn route_resume_session(
  features: &FeatureContext,
  option: &BackendOption,
  cc_session_id: Option<&str>,
) -> anyhow::Result<(Vec<String>, Option<String>)> {
  let registration = features
    .registry
    .backend_type_of(&option.backend_type)
    .with_context(|| format!("unknown backend type {}", option.backend_type))?;
  route_resume_session_style(registration.traits.resume, cc_session_id)
}

pub(crate) fn route_resume_session_style(
  style: ResumeStyle,
  cc_session_id: Option<&str>,
) -> anyhow::Result<(Vec<String>, Option<String>)> {
  let Some(cc_session_id) = cc_session_id.filter(|id| !id.is_empty()) else { return Ok((Vec::new(), None)) };
  match style {
    ResumeStyle::CliFlag => Ok((vec!["--resume".to_string(), cc_session_id.to_string()], None)),
    ResumeStyle::NativeId => Ok((Vec::new(), Some(cc_session_id.to_string()))),
  }
}

/// The follow's silence callback for a recovered serve turn, from the deadline the serve transport reported.
pub(crate) fn silence_callback(silence: RecoverySilence) -> SilenceCallback {
  let message = silence.message;
  Box::new(move || {
    let message = message.clone();
    Box::pin(async move { Err(std::io::Error::new(std::io::ErrorKind::TimedOut, message)) })
  })
}

/// The `extra_flags` a lifecycle put into a launch's backend arguments; empty when it put none.
pub(crate) fn extra_flags_of(backend_kwargs: &Map<String, Value>) -> anyhow::Result<Vec<String>> {
  let Some(extra_flags) = backend_kwargs.get(EXTRA_FLAGS_BACKEND_KWARG) else { return Ok(Vec::new()) };
  let extra_flags = extra_flags.as_array().context("lifecycle extra_flags must be an array")?;
  extra_flags
    .iter()
    .map(|flag| Ok(flag.as_str().context("lifecycle extra_flags entries must be strings")?.to_string()))
    .collect()
}

#[cfg(test)]
mod tests {
  use super::read_stderr_tail;
  use std::fs;

  #[tokio::test]
  async fn stderr_tail_reads_at_most_sixty_four_kibibytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("stderr.log");
    let mut raw = vec![b'x'; 1024 * 1024];
    raw.extend_from_slice(b" keep this tail \n");
    fs::write(&path, raw).unwrap();

    let tail = read_stderr_tail(&path).await.unwrap();
    assert!(tail.len() <= 64 * 1024);
    assert!(tail.ends_with("keep this tail"));
  }

  #[tokio::test]
  async fn stderr_tail_of_a_missing_log_is_empty() {
    let directory = tempfile::tempdir().unwrap();

    let tail = read_stderr_tail(&directory.path().join("stderr.log")).await.unwrap();
    assert_eq!(tail, "");
  }
}
