//! Helpers that the manager turn (`master_run`) and the task run (`worker`) share: the stderr tail, the subprocess
//! buffer limit, the resume routing of a backend type, the extra flags of a launch, and the event channel of a runner.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use charliebot_agents::runner::EventFn;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::ResumeStyle;
use charliebot_registry::context::FeatureContext;
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};

const STDERR_TAIL_BYTES: u64 = 64 * 1024;

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

/// The last `STDERR_TAIL_BYTES` of a process's stderr log, trimmed; empty when the log does not exist.
pub(crate) fn read_stderr_tail(path: &Path) -> anyhow::Result<String> {
  let contents = match fs::read(path) {
    Ok(contents) => contents,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
    Err(error) => return Err(error).with_context(|| format!("reading stderr log {}", path.display())),
  };
  let start = contents.len().saturating_sub(STDERR_TAIL_BYTES as usize);
  Ok(String::from_utf8_lossy(&contents[start..]).trim().to_string())
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

/// The `extra_flags` a lifecycle put into a launch's backend arguments; empty when it put none.
pub(crate) fn extra_flags_of(backend_kwargs: &Map<String, Value>) -> anyhow::Result<Vec<String>> {
  let Some(extra_flags) = backend_kwargs.get("extra_flags") else { return Ok(Vec::new()) };
  let extra_flags = extra_flags.as_array().context("lifecycle extra_flags must be an array")?;
  extra_flags
    .iter()
    .map(|flag| Ok(flag.as_str().context("lifecycle extra_flags entries must be strings")?.to_string()))
    .collect()
}
