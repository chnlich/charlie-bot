//! Durable capture for processes whose stdout becomes events only after exit.
//!
//! A detached supervisor shares the agent's process group and records its actual wait status. Both server runs
//! materialize the same event log from the captured files, then deliver it through the raw-log cursor.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::Registry;
use charliebot_registry::backend::{AgentBackend, FinalStdoutOutcome, LaunchArgs};
use charliebot_store::run_records::{RAW_LOG_NAME, STDERR_LOG_NAME};

use super::{kill, spawn};

pub const SUPERVISOR_ARG: &str = "--supervise-final-stdout";
pub const STDOUT_LOG_NAME: &str = "stdout.log";
const CONTEXT_NAME: &str = "final_stdout.context.json";
const EXIT_NAME: &str = "final_stdout.exit.json";
const OUTCOME_NAME: &str = "final_stdout.outcome.json";

#[derive(Debug)]
/// A completed capture whose backend guard failed after its events were delivered.
pub struct GuardError(pub String);

impl std::fmt::Display for GuardError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    self.0.fmt(formatter)
  }
}

impl std::error::Error for GuardError {}

#[derive(serde::Serialize, serde::Deserialize)]
struct CaptureContext {
  resume_session_id: Option<String>,
  command: Vec<OsString>,
}

/// Run the agent synchronously before the server runtime or environment initialization starts.
pub fn supervise(args: &[OsString]) -> anyhow::Result<()> {
  let [directory] = args else {
    bail!("final-stdout supervisor needs a run directory");
  };
  let directory = Path::new(directory);
  let context: CaptureContext = serde_json::from_slice(&fs::read(directory.join(CONTEXT_NAME))?)?;
  let [program, arguments @ ..] = context.command.as_slice() else { bail!("supervised command is empty") };
  let stdout = fs::OpenOptions::new().append(true).open(directory.join(STDOUT_LOG_NAME))?;
  let stderr = fs::OpenOptions::new().append(true).open(directory.join(STDERR_LOG_NAME))?;
  // The agent receives group SIGTERM; the supervisor must remain to record its wait status.
  // A caught handler resets on exec, so the agent retains the default SIGTERM behavior.
  unsafe {
    nix::sys::signal::signal(nix::sys::signal::Signal::SIGTERM, nix::sys::signal::SigHandler::Handler(wait_for_agent))
  }?;
  let status = Command::new(program)
    .args(arguments)
    .stdin(Stdio::null())
    .stdout(stdout)
    .stderr(stderr)
    .status()
    .map(kill::exit_code)
    .map_err(|error| format!("starting supervised agent: {error}"));
  write_json(&directory.join(EXIT_NAME), &status)?;
  status.map(|_| ()).map_err(anyhow::Error::msg)
}

/// Prepare capture files and the supervisor command before the spawn gate opens.
pub fn prepare(
  spec: &spawn::SpawnSpec,
  directory: &Path,
  launch: &LaunchArgs,
  supervisor: Option<&Path>,
) -> anyhow::Result<spawn::SpawnSpec> {
  fs::write(directory.join(STDOUT_LOG_NAME), [])?;
  fs::write(directory.join(STDERR_LOG_NAME), [])?;
  for name in [EXIT_NAME, OUTCOME_NAME, RAW_LOG_NAME, charliebot_store::run_records::CURSOR_NAME] {
    match fs::remove_file(directory.join(name)) {
      Ok(()) => {}
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
      Err(error) => return Err(error.into()),
    }
  }
  let mut command = vec![spec.program.clone()];
  command.extend(spec.args.clone());
  write_json(
    &directory.join(CONTEXT_NAME),
    &CaptureContext { resume_session_id: launch.resume_session_id.clone(), command },
  )?;
  let mut wrapped = spec.clone();
  wrapped.program = match supervisor {
    Some(path) => path.as_os_str().to_owned(),
    None => std::env::current_exe()?.into_os_string(),
  };
  wrapped.args = vec![SUPERVISOR_ARG.into(), directory.as_os_str().to_owned()];
  #[cfg(test)]
  if supervisor.is_none() {
    wrapped.args = vec!["--exact".into(), "transport::final_stdout::tests::supervisor".into(), "--ignored".into()];
    wrapped.env.insert("CHARLIEBOT_TEST_CAPTURE_DIR".into(), directory.as_os_str().to_owned());
  }
  Ok(wrapped)
}

/// Convert completed capture files to a durable event log. The cached outcome preserves guard and exit semantics.
pub fn materialize(
  backend: &mut dyn AgentBackend,
  directory: &Path,
  observed_exit: Option<i32>,
) -> anyhow::Result<FinalStdoutOutcome> {
  let outcome = match read_outcome(directory)? {
    Some(outcome) => outcome,
    None => {
      let stdout = fs::read(directory.join(STDOUT_LOG_NAME))?;
      let stderr = spawn::read_stderr_tail(&directory.join(STDERR_LOG_NAME))?;
      let code = process_exit_code(directory, observed_exit)?;
      let outcome = backend.finish_final_stdout(&String::from_utf8_lossy(&stdout), &stderr, code);
      write_json(&directory.join(OUTCOME_NAME), &outcome)?;
      outcome
    }
  };
  let raw_path = directory.join(RAW_LOG_NAME);
  if !raw_path.exists() {
    let mut bytes = Vec::new();
    for event in &outcome.events {
      serde_json::to_writer(&mut bytes, event)?;
      bytes.push(b'\n');
    }
    let text = String::from_utf8(bytes)?;
    charliebot_core::fsutil::atomic_write_text_synced(&raw_path, &text)?;
    fs::OpenOptions::new()
      .write(true)
      .open(raw_path)?
      .set_modified(fs::metadata(directory.join(STDOUT_LOG_NAME))?.modified()?)?;
  }
  Ok(outcome)
}

/// Wait for a captured process and prepare the raw log before recovery follows its persisted cursor.
pub async fn recover(
  registry: std::sync::Arc<Registry>,
  option: Option<&BackendOption>,
  cfg: &CharlieBotConfig,
  directory: &Path,
  is_alive: impl Fn() -> bool,
  cancelled: impl Fn() -> bool,
  poll_interval: Duration,
) -> anyhow::Result<()> {
  let context = match fs::read(directory.join(CONTEXT_NAME)) {
    Ok(bytes) => bytes,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
    Err(error) => return Err(error.into()),
  };
  while is_alive() {
    if cancelled() {
      return Ok(());
    }
    tokio::time::sleep(poll_interval).await;
  }
  let context: CaptureContext = serde_json::from_slice(&context)?;
  let launch = LaunchArgs { resume_session_id: context.resume_session_id, ..LaunchArgs::default() };
  let option = option.context("a captured final-stdout run needs its backend option")?;
  let mut built = crate::backend_types::build_backend(registry, option, cfg, launch)?;
  materialize(built.backend.as_mut(), directory, None)?;
  Ok(())
}

/// The cached parser outcome, when this directory holds a completed final-stdout capture.
pub fn read_outcome(directory: &Path) -> anyhow::Result<Option<FinalStdoutOutcome>> {
  match fs::read(directory.join(OUTCOME_NAME)) {
    Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error.into()),
  }
}

/// The agent's wait status, using the observed group exit when the supervisor could not record it.
/// A recovered process with no status ends with minus one.
pub fn process_exit_code(directory: &Path, observed_exit: Option<i32>) -> anyhow::Result<i32> {
  match fs::read(directory.join(EXIT_NAME)) {
    Ok(bytes) => serde_json::from_slice::<Result<i32, String>>(&bytes)?.map_err(anyhow::Error::msg),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(observed_exit.unwrap_or(-1)),
    Err(error) => Err(error.into()),
  }
}

extern "C" fn wait_for_agent(_signal: libc::c_int) {}

/// The capture's last output write, including a process that ended before event projection.
pub fn completion_path(directory: &Path) -> std::path::PathBuf {
  if is_capture(directory) { directory.join(STDOUT_LOG_NAME) } else { directory.join(RAW_LOG_NAME) }
}

/// Whether this run uses durable final-stdout capture.
pub fn is_capture(directory: &Path) -> bool {
  directory.join(CONTEXT_NAME).exists()
}

/// Final-stdout answers have no line-size limit; raw streaming backends use the configured limit.
pub fn event_buffer_limit(directory: &Path, configured: usize) -> usize {
  if is_capture(directory) { usize::MAX } else { configured }
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> anyhow::Result<()> {
  let text = serde_json::to_string(value)?;
  charliebot_core::fsutil::atomic_write_text_synced(path, &text)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  #[test]
  #[ignore = "subprocess entry point for captured runs"]
  fn supervisor() {
    super::supervise(&[std::env::var_os("CHARLIEBOT_TEST_CAPTURE_DIR").unwrap()]).unwrap();
  }
}
