//! The devin command, its prompt and export files, and the reading of its final output.
//!
//! `devin --print` writes the final answer to stdout and the turn's trajectory to the export file. The runner hands
//! over stdout once the process has exited, and `export` turns the trajectory into events.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use charliebot_agents::cli_base::framed_prompt;
use charliebot_agents::event_builders::make_error_event;
use charliebot_core::str_methods;
use charliebot_registry::backend::{AgentBackend, Env, FinalStdoutOutcome, LaunchArgs, Translate, TransportKind};
use serde_json::Value;

use super::export;

/// The prompt file of a turn, in the run's log directory.
const PROMPT_FILE_NAME: &str = "devin_prompt.md";
/// The export file the CLI writes after the turn, in the run's log directory.
const EXPORT_FILE_NAME: &str = "devin_export.json";

/// The `devin --print` backend of one launch.
pub struct DevinBackend {
  devin_bin: String,
  model: String,
  instructions_content: Option<String>,
  resume_session_id: Option<String>,
  extra_flags: Vec<String>,
  /// The turn's log directory. `prepare_transport` names it for a live launch; a recovered capture names it through
  /// its launch, because no transport runs again for it.
  run_dir: Option<PathBuf>,
}

impl DevinBackend {
  /// The backend of one launch. `devin_bin` is the resolved CLI and `model` is the option's model id.
  pub fn from_launch(devin_bin: String, model: &str, launch: LaunchArgs) -> Self {
    Self {
      devin_bin,
      model: model.to_string(),
      instructions_content: launch.instructions_content.filter(|content| !content.is_empty()),
      resume_session_id: launch.resume_session_id.filter(|id| !id.is_empty()),
      extra_flags: launch.extra_flags,
      run_dir: launch.log_dir,
    }
  }

  fn run_dir(&self) -> anyhow::Result<&Path> {
    self.run_dir.as_deref().context("the devin turn has no log directory")
  }

  /// The print-mode command of one turn. Each value is bound with `=`, so the optional-value flags `--export`,
  /// `--resume` and `--respect-workspace-trust` cannot take the next argument as their value. `--print` comes last,
  /// because its optional value would take the argument after it; the prompt comes from the prompt file.
  fn argv(&self, prompt_file: &Path, export_file: &Path) -> Vec<String> {
    let mut command = vec![
      self.devin_bin.clone(),
      format!("--prompt-file={}", prompt_file.display()),
      "--permission-mode=dangerous".to_string(),
      "--respect-workspace-trust=false".to_string(),
      format!("--model={}", self.model),
    ];
    if let Some(session_id) = &self.resume_session_id {
      command.push(format!("--resume={session_id}"));
    }
    command.push(format!("--export={}", export_file.display()));
    command.extend(self.extra_flags.iter().cloned());
    command.push("--print".to_string());
    command
  }

  /// The events and guard of a finished turn. A nonzero exit carries Devin's stderr, which holds its quota and login
  /// errors. A zero exit reads the export; any failure there becomes the turn's guard error, which fails the run.
  fn finish(&self, stdout: &str, stderr: &str, exit_code: i32) -> FinalStdoutOutcome {
    if exit_code != 0 {
      return finished(vec![make_error_event(&nonzero_exit_message(stderr, exit_code))], exit_code, None);
    }
    match self.turn_events(str_methods::strip(stdout)) {
      Ok(events) => finished(events, exit_code, None),
      Err(error) => {
        let message = format!("{error:#}");
        finished(vec![make_error_event(&message)], exit_code, Some(message))
      }
    }
  }

  fn turn_events(&self, answer: &str) -> anyhow::Result<Vec<Value>> {
    let export_file = self.run_dir()?.join(EXPORT_FILE_NAME);
    let text = fs::read_to_string(&export_file)
      .with_context(|| format!("devin export file {} cannot be read", export_file.display()))?;
    export::turn_events(&text, answer)
      .with_context(|| format!("devin export file {} is unusable", export_file.display()))
  }
}

impl AgentBackend for DevinBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    None
  }

  fn prepare_transport(&mut self, log_dir: &Path) -> anyhow::Result<()> {
    self.run_dir = Some(log_dir.to_path_buf());
    Ok(())
  }

  /// Writes the framed prompt into the run's log directory, then returns the command that reads it. The prompt file is
  /// the only carrier of the instructions: the type writes no instructions file in the working directory.
  fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>> {
    let run_dir = self.run_dir()?;
    let prompt_file = run_dir.join(PROMPT_FILE_NAME);
    fs::write(&prompt_file, framed_prompt(self.instructions_content.as_deref(), prompt))
      .with_context(|| format!("writing the devin prompt file {}", prompt_file.display()))?;
    Ok(self.argv(&prompt_file, &run_dir.join(EXPORT_FILE_NAME)))
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(env)
  }

  fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
    None
  }

  fn translator(&self) -> Box<dyn Translate> {
    Box::new(CapturedEvents)
  }

  fn transport(&self) -> TransportKind {
    TransportKind::FinalStdout
  }

  fn finish_final_stdout(&mut self, stdout: &str, stderr: &str, exit_code: i32) -> FinalStdoutOutcome {
    self.finish(stdout, stderr, exit_code)
  }
}

struct CapturedEvents;

impl Translate for CapturedEvents {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    Ok(vec![raw])
  }
}

fn finished(events: Vec<Value>, exit_code: i32, guard_error: Option<String>) -> FinalStdoutOutcome {
  FinalStdoutOutcome { events, exit_code, guard_error }
}

/// The message of a nonzero exit: Devin's stderr text, so its quota message reaches the turn's error.
fn nonzero_exit_message(stderr: &str, exit_code: i32) -> String {
  let text = str_methods::strip(stderr);
  if text.is_empty() {
    format!("devin exited {exit_code} with no stderr output")
  } else {
    format!("devin exited {exit_code}: {text}")
  }
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::symlink;
  use std::path::Path;
  use std::sync::{Arc, Mutex};

  use charliebot_agents::agent_environment::AgentBaseEnvironment;
  use charliebot_agents::one_shot::frame_system_prompt;
  use charliebot_agents::runner::{AgentRunner, RunOutcome, RunRequest};
  use charliebot_agents::transport::cgroup::{SessionCgroupConfig, use_fake_cgroup_hierarchy};
  use charliebot_agents::transport::final_stdout;
  use charliebot_core::cgroup::user_slice_dir;
  use charliebot_registry::Registry;
  use serde_json::Map;

  use super::*;

  const FIXTURE: &str = include_str!("../tests/fixtures/export.json");
  const ANSWER: &str = "The number is 41.";
  const SESSION_ID: &str = "devin-session-7f3a";
  const AGENTS_SLICE: &str = "charliebot-agents.slice";

  fn backend(launch: LaunchArgs) -> DevinBackend {
    DevinBackend::from_launch("/synthetic/bin/devin".to_string(), "swe-2-test", launch)
  }

  fn launch(log_dir: Option<&Path>, resume: Option<&str>) -> LaunchArgs {
    LaunchArgs {
      extra_flags: Vec::new(),
      buffer_limit: None,
      instructions_content: None,
      instructions_file: None,
      resume_session_id: resume.map(str::to_string),
      log_dir: log_dir.map(Path::to_path_buf),
      cgroup_session_id: None,
      backend_kwargs: Map::new(),
    }
  }

  /// The fake `devin` of a test: the stand-in script, linked as `devin` in `root`, with the files the test names
  /// staged beside it. The script records its argv there and reads the staged files it finds.
  fn stage_devin(root: &Path, files: &[(&str, &str)]) {
    symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_devin.sh"), root.join("devin")).unwrap();
    for (name, text) in files {
      fs::write(root.join(name), text).unwrap();
    }
  }

  /// The argv the fake `devin` recorded, without its program name.
  fn recorded_argv(root: &Path) -> Vec<String> {
    fs::read_to_string(root.join("argv.txt")).unwrap().lines().map(str::to_string).collect()
  }

  /// An agent environment whose PATH resolves `gh` to a stub wrapper, as every agent spawn requires.
  fn gh_wrapper_env(root: &Path) -> Env {
    use std::os::unix::fs::PermissionsExt;
    let wrapper_dir = root.join("gh-wrapper");
    fs::create_dir_all(&wrapper_dir).unwrap();
    let wrapper = wrapper_dir.join("gh");
    fs::write(&wrapper, "#!/bin/sh\n").unwrap();
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();
    Env::from([
      ("PATH".to_string(), format!("{}:/bin:/usr/bin", wrapper_dir.display())),
      (charliebot_agents::gh_wrapper::WRAPPER_PATH_VAR.to_string(), wrapper.display().to_string()),
    ])
  }

  /// The fake-program helper that cargo builds beside this test binary. The final-stdout supervisor is a separate
  /// process, and `CARGO_BIN_EXE_*` names only this package's binaries.
  fn fake_program_bin() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("the test binary path");
    exe
      .ancestors()
      .skip(1)
      .map(|directory| directory.join("fake_program"))
      .find(|candidate| candidate.is_file())
      .expect("cargo builds the fake-program helper into the target directory that holds the test binary")
  }

  /// One turn run through the agent runner: what the runner returned, the events it emitted before it returned, and
  /// the pid of the process it spawned.
  struct TurnRun {
    result: anyhow::Result<RunOutcome>,
    events: Vec<Value>,
    pid: Option<u32>,
  }

  async fn run_turn(root: &Path, launch: LaunchArgs, cgroup_config: SessionCgroupConfig) -> TurnRun {
    let devin_bin = root.join("devin").display().to_string();
    let runner =
      AgentRunner::new("devin", Box::new(DevinBackend::from_launch(devin_bin, "swe-2-test", launch.clone())), launch);
    let mut request = RunRequest::new(
      "Pick the number in number.txt.".into(),
      root.to_path_buf(),
      AgentBaseEnvironment { values: gh_wrapper_env(root) },
      cgroup_config,
      Arc::new(Registry::new()),
    );
    // The supervisor is the helper binary; it runs the backend's `devin` path as the child it supervises.
    request.final_stdout_supervisor = Some(fake_program_bin());
    let pid_slot = Arc::new(Mutex::new(None));
    let recorded_pid = pid_slot.clone();
    request.on_spawn = Some(Arc::new(move |identity| {
      let recorded_pid = recorded_pid.clone();
      Box::pin(async move {
        *recorded_pid.lock().unwrap() = Some(identity.pid);
        Ok(())
      })
    }));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let events = seen.clone();
    request.on_event = Arc::new(move |event| {
      let events = events.clone();
      Box::pin(async move {
        events.lock().unwrap().push(event);
        Ok(())
      })
    });
    let result = runner.run(request).await;
    let events = seen.lock().unwrap().clone();
    let pid = *pid_slot.lock().unwrap();
    TurnRun { result, events, pid }
  }

  #[test]
  fn the_first_and_resume_turn_argv_bind_every_value_with_an_equals_sign() {
    let prompt = Path::new("/synthetic/logs/devin_prompt.md");
    let export = Path::new("/synthetic/logs/devin_export.json");
    assert_eq!(
      backend(launch(None, None)).argv(prompt, export),
      [
        "/synthetic/bin/devin",
        "--prompt-file=/synthetic/logs/devin_prompt.md",
        "--permission-mode=dangerous",
        "--respect-workspace-trust=false",
        "--model=swe-2-test",
        "--export=/synthetic/logs/devin_export.json",
        "--print",
      ]
    );
    assert_eq!(
      backend(launch(None, Some(SESSION_ID))).argv(prompt, export),
      [
        "/synthetic/bin/devin",
        "--prompt-file=/synthetic/logs/devin_prompt.md",
        "--permission-mode=dangerous",
        "--respect-workspace-trust=false",
        "--model=swe-2-test",
        "--resume=devin-session-7f3a",
        "--export=/synthetic/logs/devin_export.json",
        "--print",
      ]
    );
  }

  #[test]
  fn the_prompt_file_holds_the_framed_instructions_and_the_message() {
    let root = tempfile::tempdir().unwrap();
    let prompt_file = root.path().join(PROMPT_FILE_NAME);

    let mut instructed =
      backend(LaunchArgs { instructions_content: Some("Build stuff.".to_string()), ..launch(None, None) });
    instructed.prepare_transport(root.path()).unwrap();
    let argv = instructed.build_command("do the work").unwrap();
    assert_eq!(fs::read_to_string(&prompt_file).unwrap(), frame_system_prompt("Build stuff.", "do the work"));
    assert_eq!(argv[1], format!("--prompt-file={}", prompt_file.display()));

    let mut plain = backend(launch(None, None));
    plain.prepare_transport(root.path()).unwrap();
    plain.build_command("just the message").unwrap();
    assert_eq!(fs::read_to_string(&prompt_file).unwrap(), "just the message");
  }

  #[tokio::test]
  async fn a_turn_runs_devin_in_print_mode_and_emits_the_export_events() {
    let root = tempfile::tempdir().unwrap();
    stage_devin(root.path(), &[("export.json", FIXTURE), ("answer.txt", "The number is 41.\n")]);
    let logs = root.path().join("logs");
    let run = run_turn(root.path(), launch(Some(&logs), None), SessionCgroupConfig::disabled()).await;
    assert_eq!(run.result.unwrap().exit_code, Some(0));

    let logs_text = logs.display();
    assert_eq!(
      recorded_argv(root.path()),
      [
        format!("--prompt-file={logs_text}/devin_prompt.md"),
        "--permission-mode=dangerous".to_string(),
        "--respect-workspace-trust=false".to_string(),
        "--model=swe-2-test".to_string(),
        format!("--export={logs_text}/devin_export.json"),
        "--print".to_string(),
      ]
    );
    assert_eq!(run.events, export::turn_events(FIXTURE, ANSWER).unwrap());
  }

  #[tokio::test]
  async fn a_resumed_turn_passes_the_native_session_id() {
    let root = tempfile::tempdir().unwrap();
    stage_devin(root.path(), &[("export.json", FIXTURE), ("answer.txt", ANSWER)]);
    let logs = root.path().join("logs");
    run_turn(root.path(), launch(Some(&logs), Some(SESSION_ID)), SessionCgroupConfig::disabled()).await.result.unwrap();
    assert!(recorded_argv(root.path()).contains(&format!("--resume={SESSION_ID}")));
  }

  #[tokio::test]
  async fn a_missing_export_fails_the_turn_and_names_its_path() {
    let root = tempfile::tempdir().unwrap();
    stage_devin(root.path(), &[("answer.txt", ANSWER)]);
    let logs = root.path().join("logs");
    let run = run_turn(root.path(), launch(Some(&logs), None), SessionCgroupConfig::disabled()).await;
    let error = run.result.unwrap_err().to_string();
    let export_path = logs.join(EXPORT_FILE_NAME);
    assert!(error.contains(&export_path.display().to_string()), "the error names the export path: {error}");
    assert_eq!(run.events.len(), 1);
    assert_eq!(run.events[0]["type"], "error");
  }

  #[tokio::test]
  async fn an_export_without_a_session_id_fails_the_turn() {
    let root = tempfile::tempdir().unwrap();
    let without_session = FIXTURE.replace("\"session_id\": \"devin-session-7f3a\",", "");
    stage_devin(root.path(), &[("export.json", without_session.as_str()), ("answer.txt", ANSWER)]);
    let logs = root.path().join("logs");
    let run = run_turn(root.path(), launch(Some(&logs), None), SessionCgroupConfig::disabled()).await;
    let error = run.result.unwrap_err().to_string();
    assert!(error.contains("devin export has no session_id"), "the error names the missing field: {error}");
  }

  #[tokio::test]
  async fn a_nonzero_exit_fails_the_turn_with_devins_stderr() {
    let root = tempfile::tempdir().unwrap();
    stage_devin(root.path(), &[("stderr.txt", "Error: Usage limit reached. Try again later.\n"), ("exit_code", "1")]);
    let logs = root.path().join("logs");
    let run = run_turn(root.path(), launch(Some(&logs), None), SessionCgroupConfig::disabled()).await;
    assert_eq!(run.result.unwrap().exit_code, Some(1));
    assert_eq!(run.events.len(), 1);
    assert_eq!(run.events[0]["type"], "error");
    assert_eq!(run.events[0]["message"], "devin exited 1: Error: Usage limit reached. Try again later.");
  }

  #[tokio::test]
  async fn a_devin_launch_joins_the_session_cgroup_of_its_session() {
    use_fake_cgroup_hierarchy();
    let root = tempfile::tempdir().unwrap();
    stage_devin(root.path(), &[("export.json", FIXTURE), ("answer.txt", ANSWER)]);
    let base = root.path().join("user@1001.service");
    let slice = user_slice_dir(&base, AGENTS_SLICE);
    fs::create_dir_all(&slice).unwrap();
    fs::write(slice.join("cgroup.subtree_control"), "").unwrap();
    let session_cgroup = slice.join("charliebot-sess-devincgr");
    fs::create_dir_all(&session_cgroup).unwrap();
    for name in
      ["cgroup.procs", "cgroup.threads", "cgroup.subtree_control", "memory.max", "memory.swap.max", "pids.max"]
    {
      fs::write(session_cgroup.join(name), "").unwrap();
    }
    let config = SessionCgroupConfig { base_dir: base, memory_max_mb: 64, ..SessionCgroupConfig::disabled() };
    let logs = root.path().join("logs");
    let cgroup_launch =
      LaunchArgs { cgroup_session_id: Some("devincgr-session".to_string()), ..launch(Some(&logs), None) };
    let run = run_turn(root.path(), cgroup_launch, config).await;
    run.result.unwrap();
    let pid = run.pid.expect("the spawn recorded the spawned process");
    assert_eq!(fs::read_to_string(session_cgroup.join("cgroup.procs")).unwrap().trim(), pid.to_string());
  }

  #[test]
  fn a_recovered_capture_finishes_from_the_directory_it_was_launched_with() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join(EXPORT_FILE_NAME), FIXTURE).unwrap();
    fs::write(root.path().join(final_stdout::STDOUT_LOG_NAME), ANSWER).unwrap();
    fs::write(root.path().join(charliebot_store::run_records::STDERR_LOG_NAME), "").unwrap();
    let mut recovered = backend(launch(Some(root.path()), None));
    let outcome = final_stdout::materialize(&mut recovered, root.path(), Some(0)).unwrap();
    assert_eq!(outcome.guard_error, None);
    assert_eq!(outcome.events, export::turn_events(FIXTURE, ANSWER).unwrap());
  }
}
