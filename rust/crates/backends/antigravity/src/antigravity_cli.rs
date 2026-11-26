//! The antigravity command, child environment and live input. Each turn runs `agy acp`, which speaks the Agent Client
//! Protocol on its stdio: the initial prompt and each message that arrives mid-turn are `session/prompt` requests on
//! one session (`charliebot_acp`). agy's output goes to the raw log, which the follower translates line by line, and its
//! input is the run's named pipe.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use charliebot_acp::codec::{AcpCodec, SharedSession};
use charliebot_acp::handshake::{self, HandshakeSpec, SessionStart};
use charliebot_agents::cli_base::{SKIP_PERMISSIONS_FLAG, framed_prompt, prepend_path_dir, strip_google_api_keys};
use charliebot_registry::backend::{AgentBackend, Env, LaunchArgs, MidturnTranslate, Translate, TransportKind};
use charliebot_registry::midturn_delivery::{DeliveryState, ExitedProbe};
use charliebot_store::run_records::RAW_LOG_NAME;
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::io::AsyncWrite;

/// The backend kwarg that names the Run whose live input this process takes. A manager turn that takes live input
/// carries it; a turn without it takes no mid-turn offers.
pub const MIDTURN_RUN_ID_KWARG: &str = "midturn_run_id";

/// The `agy acp` backend of one launch.
pub struct AntigravityBackend {
  agy_bin: String,
  user_local_bin: String,
  model: String,
  instructions_content: Option<String>,
  resume_session_id: Option<String>,
  extra_flags: Vec<String>,
  midturn_run_id: Option<String>,
  session: SharedSession,
  midturn: Arc<DeliveryState>,
  /// The run's log directory, set by `prepare_transport` before the first write.
  log_dir: Option<PathBuf>,
}

impl AntigravityBackend {
  /// The backend of one launch. A launch with no Run id has no mid-turn offers, and its state binds to no Run.
  pub fn from_launch(agy_bin: String, user_local_bin: String, model: &str, launch: LaunchArgs) -> anyhow::Result<Self> {
    let midturn_run_id = match launch.backend_kwargs.get(MIDTURN_RUN_ID_KWARG) {
      None | Some(Value::Null) => None,
      Some(Value::String(run_id)) => Some(run_id.clone()).filter(|run_id| !run_id.is_empty()),
      Some(_) => bail!("{MIDTURN_RUN_ID_KWARG} must be a string"),
    };
    let session: SharedSession = Arc::new(Mutex::new(None));
    let run_id = midturn_run_id.as_deref().unwrap_or_default();
    let midturn = DeliveryState::new(run_id, Box::new(AcpCodec::new(Arc::clone(&session))));
    Ok(Self {
      agy_bin,
      user_local_bin,
      model: model.to_string(),
      instructions_content: launch.instructions_content.filter(|content| !content.is_empty()),
      resume_session_id: launch.resume_session_id.filter(|id| !id.is_empty()),
      extra_flags: launch.extra_flags,
      midturn_run_id,
      session,
      midturn,
      log_dir: None,
    })
  }

  /// The ACP command: `agy acp` with the permission flag that the turn's tool calls need, then the configured flags.
  fn argv(&self) -> Vec<String> {
    let mut command = vec![self.agy_bin.clone(), "acp".to_string(), SKIP_PERMISSIONS_FLAG.to_string()];
    command.extend(self.extra_flags.iter().cloned());
    command
  }

  /// The child env: the Google API keys stripped, so the CLI bills through its OAuth login, and the user-local bin on
  /// PATH.
  fn child_env(&self, env: Env) -> Env {
    let mut env = strip_google_api_keys(&env);
    prepend_path_dir(&mut env, &self.user_local_bin);
    env
  }
}

impl AgentBackend for AntigravityBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    None
  }

  fn prepare_transport(&mut self, log_dir: &Path) -> anyhow::Result<()> {
    self.log_dir = Some(log_dir.to_path_buf());
    Ok(())
  }

  fn build_command(&self, _prompt: &str) -> anyhow::Result<Vec<String>> {
    Ok(self.argv())
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(self.child_env(env))
  }

  /// The initial prompt goes over ACP on stdin, after the handshake (`write_stdin_prompt`).
  fn stdin_prompt(&self, prompt: &str) -> Option<String> {
    Some(framed_prompt(self.instructions_content.as_deref(), prompt))
  }

  fn midturn_run_id(&self) -> Option<String> {
    self.midturn_run_id.clone()
  }

  fn midturn_state(&self) -> Option<Arc<DeliveryState>> {
    Some(Arc::clone(&self.midturn))
  }

  fn input_is_fifo(&self) -> bool {
    true
  }

  /// The handshake, then the initial prompt as the first `session/prompt`. The session is the stored one a turn
  /// resumes, or a new one.
  fn write_stdin_prompt(
    &self,
    mut stdin: Box<dyn AsyncWrite + Send + Unpin>,
    exited: ExitedProbe,
    prompt: &str,
    cwd: &Path,
  ) -> Option<BoxFuture<'static, anyhow::Result<()>>> {
    let log_dir = self.log_dir.clone().expect("prepare_transport sets the log directory before the first write");
    let raw_path = log_dir.join(RAW_LOG_NAME);
    let start = match &self.resume_session_id {
      Some(id) => SessionStart::Load(id.clone()),
      None => SessionStart::New,
    };
    let spec = HandshakeSpec { start, cwd: cwd.to_path_buf(), model: self.model.clone() };
    let session = Arc::clone(&self.session);
    let state = Arc::clone(&self.midturn);
    let prompt = prompt.to_string();
    Some(Box::pin(async move {
      handshake::run(stdin.as_mut(), &raw_path, &exited, &session, &spec).await.context("the agy ACP handshake")?;
      state.write_prompt(stdin, exited, &prompt).await
    }))
  }

  fn on_event<'a>(&'a self, _event: &'a Value) -> Option<BoxFuture<'a, anyhow::Result<()>>> {
    let state = Arc::clone(&self.midturn);
    Some(Box::pin(async move { state.settle_turn().await }))
  }

  fn translator(&self) -> Box<dyn Translate> {
    Box::new(MidturnTranslate(Arc::clone(&self.midturn)))
  }

  fn transport(&self) -> TransportKind {
    TransportKind::RawLog
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn backend(launch: LaunchArgs) -> AntigravityBackend {
    AntigravityBackend::from_launch("/opt/agy".to_string(), "/home/u/.local/bin".to_string(), "m", launch).unwrap()
  }

  #[test]
  fn the_turn_runs_agy_acp_with_the_permission_flag_and_no_prompt_argument() {
    let argv = backend(LaunchArgs { extra_flags: vec!["--verbose".to_string()], ..LaunchArgs::default() })
      .build_command("hello")
      .unwrap();
    assert_eq!(argv, ["/opt/agy", "acp", SKIP_PERMISSIONS_FLAG, "--verbose"]);
  }

  #[test]
  fn the_initial_prompt_is_framed_for_stdin() {
    let launch = LaunchArgs { instructions_content: Some("Be brief.".to_string()), ..LaunchArgs::default() };
    let framed = backend(launch).stdin_prompt("hello").unwrap();
    assert!(framed.ends_with("hello"), "{framed}");
    assert!(framed.contains("Be brief."), "{framed}");
  }

  #[test]
  fn a_run_without_a_run_id_takes_no_offers_and_a_run_id_names_the_process() {
    assert_eq!(backend(LaunchArgs::default()).midturn_run_id(), None);
    let launch = LaunchArgs {
      backend_kwargs: serde_json::Map::from_iter([(MIDTURN_RUN_ID_KWARG.to_string(), Value::from("run-1"))]),
      ..LaunchArgs::default()
    };
    assert_eq!(backend(launch).midturn_run_id().as_deref(), Some("run-1"));
  }

  #[test]
  fn the_child_env_loses_the_google_keys_and_gains_the_user_local_bin() {
    let env =
      Env::from([("GOOGLE_API_KEY".to_string(), "k".to_string()), ("PATH".to_string(), "/usr/bin".to_string())]);
    let env = backend(LaunchArgs::default()).child_env(env);
    assert!(!env.contains_key("GOOGLE_API_KEY"));
    assert!(env["PATH"].starts_with("/home/u/.local/bin"), "{}", env["PATH"]);
  }
}
