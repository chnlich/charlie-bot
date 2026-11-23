//! One run of one backend option outside a session: the engine behind `charliebot backend run`.
//!
//! The option's backend comes from the registry's factory, and `AgentRunner` runs it once in the given working
//! directory. The report carries the final text, the exit code and the stderr of the process.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use charliebot_core::backend_types as type_names;
use charliebot_core::config::BackendOption;
use charliebot_registry::Registry;
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

/// The session a run starts or continues: a new session under the identity's ids, or the continuation of `session_id`,
/// the session a failed run of the option named, under a new run id.
#[derive(Clone, Debug)]
pub enum RunStart {
  New(RunIdentity),
  Resume { session_id: String, run_id: String },
}

impl RunStart {
  /// The session id and the run id the run carries.
  fn ids(&self) -> (&str, &str) {
    match self {
      Self::New(identity) => (&identity.session_id, &identity.run_id),
      Self::Resume { session_id, run_id } => (session_id, run_id),
    }
  }
}

/// The launch of a run of `option`, and the session id the run knows before it starts. A new Claude-family run names
/// its session with `--session-id`; a resumed one names it through the resume style in [`resumed_launch`].
fn launch_for_option(
  option: &BackendOption,
  start: &RunStart,
  claude_config_dir: Option<&str>,
) -> (Option<String>, LaunchArgs) {
  let (session_id, run_id) = start.ids();
  let mut launch = match option.backend_type.as_str() {
    type_names::CC_CLAUDE | type_names::CC_ANTHROPIC_COMPATIBLE | type_names::CC_OPENAI_COMPATIBLE => {
      let mut launch = LaunchArgs { cgroup_session_id: Some(session_id.to_string()), ..LaunchArgs::default() };
      if let RunStart::New(_) = start {
        launch.backend_kwargs.insert("claude_session_id".to_string(), session_id.into());
      }
      launch.backend_kwargs.insert("run_id".to_string(), run_id.into());
      launch
    }
    _ => LaunchArgs::default(),
  };
  let known_session = match start {
    RunStart::New(_) => launch.cgroup_session_id.clone(),
    RunStart::Resume { session_id, .. } => Some(session_id.clone()),
  };
  if let Some(dir) = claude_config_dir {
    launch.backend_kwargs.insert("claude_config_dir".to_string(), dir.into());
  }
  (known_session, launch)
}

/// The launch of a resumed run: the option type's resume style puts the session's flags before the launch's own flags,
/// or names its native id in the launch. A type without a resume path fails and names itself.
fn resumed_launch(
  registry: &Registry,
  option: &BackendOption,
  start: &RunStart,
  mut launch: LaunchArgs,
) -> anyhow::Result<LaunchArgs> {
  let RunStart::Resume { session_id, .. } = start else { return Ok(launch) };
  let (flags, native_id) = registry.resume_parts(&option.backend_type, session_id)?;
  launch.extra_flags = [flags, launch.extra_flags].concat();
  launch.resume_session_id = native_id;
  Ok(launch)
}

/// Run `option` once with `prompt` in `cwd`, building its backend from `host`'s config and registry. `start` names the
/// session the run starts or continues. `login` joins the process environment, and the raw event log is copied to
/// `events_path`.
#[allow(clippy::too_many_arguments)]
pub async fn run_once(
  host: &CliHost,
  option: &BackendOption,
  prompt: String,
  cwd: PathBuf,
  login: Env,
  start: RunStart,
  claude_config_dir: Option<&str>,
  events_path: &Path,
) -> anyhow::Result<RunReport> {
  let (known_session, launch) = launch_for_option(option, &start, claude_config_dir);
  let launch = resumed_launch(&host.registry, option, &start, launch)?;
  let built = backend_types::build_backend(Arc::clone(&host.registry), option, &host.cfg, launch)?;
  let report = run_built(built, &host.base_env.values, prompt, cwd, login, events_path).await?;
  Ok(RunReport { session_id: known_session.or(report.session_id), ..report })
}

/// Run a built backend once in `cwd`. `login` joins the process environment, and the raw event log is copied to
/// `events_path`. The report's session id is `None` unless the backend emits a `session_attached` event.
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
  let attached_session = Arc::new(std::sync::Mutex::new(None::<String>));
  let attached_slot = Arc::clone(&attached_session);
  let text_event = final_text.on_event();
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
  request.on_event = Arc::new(move |event| {
    if event.get("type").and_then(serde_json::Value::as_str) == Some(charliebot_core::event_types::SESSION_ATTACHED)
      && let Some(id) = event.get("session_id").and_then(serde_json::Value::as_str).filter(|id| !id.is_empty())
    {
      *attached_slot.lock().expect("attached session lock is not poisoned") = Some(id.to_string());
    }
    text_event(event)
  });
  let outcome = runner.run(request).await;
  let raw_log = log_dir.path().join(RAW_LOG_NAME);
  if raw_log.exists() {
    std::fs::copy(&raw_log, events_path)
      .with_context(|| format!("copying the raw event log to {}", events_path.display()))?;
  }
  let outcome = outcome.context("running the backend")?;
  let session_id = attached_session.lock().expect("attached session lock is not poisoned").clone();
  Ok(RunReport { text: final_text.text(), exit_code: outcome.exit_code, stderr_text: outcome.stderr_text, session_id })
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::sync::{Arc, Mutex};

  use anyhow::bail;
  use charliebot_core::config::CharlieBotConfig;
  use charliebot_registry::RegistryError;
  use charliebot_registry::backend::{
    AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, LaunchArgs, ResumeStyle,
  };

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

  #[test]
  fn claude_backend_types_receive_identity_and_optional_config_dir() {
    let identity = RunIdentity { session_id: "s1".into(), run_id: "r1".into() };
    let base = BackendOption {
      id: "opt".into(),
      label: "Opt".into(),
      model: "m".into(),
      prompt_overlay: None,
      billing: None,
      backend_type: type_names::CC_CLAUDE.into(),
      type_fields: Default::default(),
      visible: true,
    };
    let start = RunStart::New(identity);
    let (sid, launch) = launch_for_option(&base, &start, Some("/tmp/acct"));
    assert_eq!(sid.as_deref(), Some("s1"));
    assert_eq!(launch.backend_kwargs.get("claude_session_id"), Some(&serde_json::json!("s1")));
    assert_eq!(launch.backend_kwargs.get("claude_config_dir"), Some(&serde_json::json!("/tmp/acct")));

    let openai = BackendOption { backend_type: type_names::CC_OPENAI_COMPATIBLE.into(), ..base.clone() };
    let (sid, launch) = launch_for_option(&openai, &start, None);
    assert_eq!(sid.as_deref(), Some("s1"));
    assert_eq!(launch.backend_kwargs.get("run_id"), Some(&serde_json::json!("r1")));

    let codex = BackendOption { backend_type: type_names::CODEX.into(), ..base };
    let (sid, launch) = launch_for_option(&codex, &start, None);
    assert_eq!(sid, None);
    assert!(launch.backend_kwargs.is_empty());
  }

  /// Registers one type per resume style, under the real type names where a test needs them, and one type with no
  /// resume path.
  fn register_resume_types(registry: &mut Registry) -> Result<(), RegistryError> {
    struct NoBuild;
    impl BackendFactory for NoBuild {
      fn build(
        &self,
        _option: Option<&BackendOption>,
        _cfg: &CharlieBotConfig,
        _launch: LaunchArgs,
      ) -> anyhow::Result<Box<dyn AgentBackend>> {
        bail!("the resume tests build no backend")
      }
    }
    for (type_name, resume) in [
      (type_names::CC_CLAUDE, ResumeStyle::CliFlag),
      (type_names::CODEX, ResumeStyle::NativeId),
      ("synthetic-no-resume", ResumeStyle::Unsupported),
    ] {
      registry.backend_type(BackendTypeRegistration {
        type_name,
        traits: BackendTraits {
          resume,
          restart_reattach: false,
          preassigned_session_id: false,
          family_prefix: None,
          manual_compact: false,
          compact_hint: None,
        },
        option_model: |_| Ok(()),
        factory: Arc::new(NoBuild),
        lifecycle: None,
        translate_fallback: false,
      })?;
    }
    Ok(())
  }

  fn option_of_type(backend_type: &str) -> BackendOption {
    BackendOption {
      id: "opt".into(),
      label: "Opt".into(),
      model: "m".into(),
      prompt_overlay: None,
      billing: None,
      backend_type: backend_type.into(),
      type_fields: Default::default(),
      visible: true,
    }
  }

  /// A resumed run continues the named session through its type's resume style: a Claude-family run takes the resume
  /// flag and no new `--session-id`, and a native-id type takes the id in its launch.
  #[test]
  fn a_resumed_run_continues_the_session_through_the_types_resume_style() {
    let mut registry = Registry::new();
    registry.register_package("synthetic.backend-run", register_resume_types).unwrap();
    let start = RunStart::Resume { session_id: "old-session".into(), run_id: "new-run".into() };

    let claude = option_of_type(type_names::CC_CLAUDE);
    let (sid, launch) = launch_for_option(&claude, &start, None);
    assert_eq!(sid.as_deref(), Some("old-session"));
    assert!(!launch.backend_kwargs.contains_key("claude_session_id"), "{:?}", launch.backend_kwargs);
    assert_eq!(launch.backend_kwargs.get("run_id"), Some(&serde_json::json!("new-run")));
    let launch = resumed_launch(&registry, &claude, &start, launch).unwrap();
    assert_eq!(launch.extra_flags, ["--resume", "old-session"]);
    assert_eq!(launch.resume_session_id, None);

    let codex = option_of_type(type_names::CODEX);
    let (sid, launch) = launch_for_option(&codex, &start, None);
    assert_eq!(sid.as_deref(), Some("old-session"));
    let launch = resumed_launch(&registry, &codex, &start, launch).unwrap();
    assert!(launch.extra_flags.is_empty());
    assert_eq!(launch.resume_session_id.as_deref(), Some("old-session"));
  }

  /// A type without a resume path refuses a resumed run by name, and a new run on the same type needs no resume path.
  #[test]
  fn a_type_without_a_resume_path_refuses_a_resumed_run_by_name() {
    let mut registry = Registry::new();
    registry.register_package("synthetic.backend-run", register_resume_types).unwrap();
    let option = option_of_type("synthetic-no-resume");

    let resumed = RunStart::Resume { session_id: "old-session".into(), run_id: "new-run".into() };
    let error = resumed_launch(&registry, &option, &resumed, LaunchArgs::default()).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("synthetic-no-resume"), "{message}");
    assert!(message.contains("no resume path"), "{message}");

    let fresh = RunStart::New(RunIdentity { session_id: "s1".into(), run_id: "r1".into() });
    let launch = resumed_launch(&registry, &option, &fresh, LaunchArgs::default()).unwrap();
    assert!(launch.extra_flags.is_empty());
    assert_eq!(launch.resume_session_id, None);
  }
}
