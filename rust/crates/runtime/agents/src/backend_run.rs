//! One run of one backend option outside a session: the engine behind `charliebot backend run`.
//!
//! The option's backend comes from the registry's factory, and `AgentRunner` runs it once in the given working
//! directory. The report carries the final text, the exit code and the stderr of the process.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use charliebot_core::backend_types as type_names;
use charliebot_core::config::BackendOption;
use charliebot_registry::backend::{Env, LaunchArgs};
use charliebot_store::run_records::RAW_LOG_NAME;

use crate::agent_environment::AgentBaseEnvironment;
use crate::backend_types::{self, BuiltBackend};
use crate::cli_host::CliHost;
use crate::launch_loop::ChildEnvPolicy;
use crate::one_shot::FinalText;
use crate::runner::{AgentRunner, RunRequest};

/// What one run reports.
#[derive(Debug)]
pub struct RunReport {
  /// The final text of the run, trimmed.
  pub text: String,
  /// The process's exit code, `None` when no process started.
  pub exit_code: Option<i32>,
  /// The process's stderr: the reason of a failed run.
  pub stderr_text: String,
  /// The session id the run asked the backend to use, for the backend types that take one.
  pub session_id: Option<String>,
}

/// The ids a run of its own carries: the CharlieBot session its Claude Code hooks name, the Claude session that session
/// id becomes, and the run inside that session. A Claude Code worker launch needs both; the other types take none.
#[derive(Clone, Debug)]
pub struct RunIdentity {
  pub session_id: String,
  pub run_id: String,
}

/// Run `option` once with `prompt` in `cwd`, building its backend from `host`'s config and registry. `login` joins the
/// process environment, and the raw event log is copied to `events_path`.
pub async fn run_once(
  host: &CliHost,
  option: &BackendOption,
  prompt: String,
  cwd: PathBuf,
  login: Env,
  identity: RunIdentity,
  events_path: &Path,
) -> anyhow::Result<RunReport> {
  let (session_id, launch) = match option.backend_type.as_str() {
    type_names::CC_CLAUDE | type_names::CC_ANTHROPIC_COMPATIBLE => {
      let mut launch = LaunchArgs { cgroup_session_id: Some(identity.session_id.clone()), ..LaunchArgs::default() };
      launch.backend_kwargs.insert("claude_session_id".to_string(), identity.session_id.clone().into());
      launch.backend_kwargs.insert("run_id".to_string(), identity.run_id.clone().into());
      (Some(identity.session_id), launch)
    }
    _ => (None, LaunchArgs::default()),
  };
  let built = backend_types::build_backend(Arc::clone(&host.registry), option, &host.cfg, launch)?;
  let report = run_built(built, &host.base_env.values, prompt, cwd, login, events_path).await?;
  Ok(RunReport { session_id, ..report })
}

/// Run a built backend once in `cwd`. `login` joins the process environment, and the raw event log is copied to
/// `events_path`. The report's session id is `None`: the caller names the session it built the backend for.
pub async fn run_built(
  mut built: BuiltBackend,
  base_env: &Env,
  prompt: String,
  cwd: PathBuf,
  login: Env,
  events_path: &Path,
) -> anyhow::Result<RunReport> {
  let log_dir = charliebot_core::tempdir::mkdtemp("charliebot-backend-run-")?;
  built.launch.log_dir = Some(log_dir.path().to_path_buf());
  let final_text = FinalText::default();
  let runner = AgentRunner::new(&built.backend_type, built.backend, built.launch.clone());
  let mut request = RunRequest::new(
    prompt,
    cwd,
    AgentBaseEnvironment { values: base_env.clone() },
    crate::transport::cgroup::SessionCgroupConfig::disabled(),
    built.registry,
  );
  request.child_env_policy = ChildEnvPolicy::Inherited;
  request.environment_overlay = login;
  request.on_event = final_text.on_event();
  let outcome = runner.run(request).await;
  let raw_log = log_dir.path().join(RAW_LOG_NAME);
  if raw_log.exists() {
    std::fs::copy(&raw_log, events_path)
      .with_context(|| format!("copying the raw event log to {}", events_path.display()))?;
  }
  let outcome = outcome.context("running the backend")?;
  Ok(RunReport {
    text: final_text.text(),
    exit_code: outcome.exit_code,
    stderr_text: outcome.stderr_text,
    session_id: None,
  })
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::sync::{Arc, Mutex};

  use charliebot_registry::Registry;
  use charliebot_registry::backend::LaunchArgs;

  use super::*;
  use crate::backend_types::BuiltBackend;
  use crate::one_shot::tests::ScriptedBackend;

  fn scripted(script: &str) -> BuiltBackend {
    BuiltBackend {
      backend: Box::new(ScriptedBackend {
        script: script.to_string(),
        native_answer: None,
        seen_log_dir: Arc::new(Mutex::new(None)),
      }),
      launch: LaunchArgs::default(),
      backend_type: "scripted".to_string(),
      registry: Arc::new(Registry::new()),
    }
  }

  /// A stub command line stands in for the backend: its final text is the run's report, its raw events are kept in the
  /// working directory, and a failing stub reports its exit code and stderr.
  #[tokio::test]
  async fn a_run_reports_the_final_text_the_raw_events_and_the_exit_code() {
    let root = tempfile::tempdir().unwrap();
    let base_env = crate::gh_wrapper::stub_environment(root.path(), "/bin:/usr/bin");
    let cwd = root.path().join("work");
    fs::create_dir(&cwd).unwrap();
    let answer = "printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"the answer\"}]}}' '{\"type\":\"result\"}'";
    let events = cwd.join("events.ndjson");
    let report =
      run_built(scripted(answer), &base_env, "prompt".into(), cwd.clone(), Env::new(), &events).await.unwrap();
    assert_eq!(report.text, "the answer");
    assert_eq!(report.exit_code, Some(0));
    assert!(
      fs::read_to_string(&events).unwrap().contains("the answer"),
      "the raw events stay in the working directory"
    );

    let failing = run_built(scripted("echo reason >&2; exit 3"), &base_env, "prompt".into(), cwd, Env::new(), &events)
      .await
      .unwrap();
    assert_eq!(failing.exit_code, Some(3));
    assert_eq!(failing.text, "");
    assert!(failing.stderr_text.contains("reason"), "{}", failing.stderr_text);
  }
}
