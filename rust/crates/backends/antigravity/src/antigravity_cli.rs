//! The antigravity command, child environment and final-output reading (`src/backends/antigravity/antigravity_cli.py`).
//! `agy --print` writes one JSON envelope to stdout, and the runner hands that stdout over once the process has exited.

use std::path::Path;
use std::sync::LazyLock;

use anyhow::bail;
use charliebot_core::event_types;
use charliebot_core::str_methods;
use charliebot_core::text_repr::value_str;
use charliebot_registry::backend::{AgentBackend, Env, FinalStdoutOutcome, LaunchArgs, Translate, TransportKind};
use regex::Regex;
use serde_json::{Map, Value, json};

use charliebot_agents::cli_base::{
  SKIP_PERMISSIONS_FLAG, framed_prompt, int_field, prepend_path_dir, strip_google_api_keys, truthy,
};
use charliebot_agents::event_builders::{make_error_event, make_result_event, make_text_event};

/// The stderr line that `agy` prints when its turn outlives `--print-timeout`.
static PRINT_TIMEOUT_RE: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"\[agy\] print timeout after (\S+) with turn in progress").expect("the print-timeout pattern is valid")
});

/// The platform notices that `agy` wraps in `<SYSTEM_MESSAGE>` blocks, which the response does not carry.
static SYSTEM_MESSAGE_RE: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"(?s)<SYSTEM_MESSAGE>.*?</SYSTEM_MESSAGE>\s*").expect("the system-message pattern is valid")
});

/// How many characters of a non-envelope stdout a message quotes.
const NON_ENVELOPE_PREVIEW_CHARS: usize = 200;

/// The `agy --print` backend of one launch.
pub struct AntigravityBackend {
  agy_bin: String,
  user_local_bin: String,
  model: String,
  print_timeout: String,
  instructions_content: Option<String>,
  resume_session_id: Option<String>,
  extra_flags: Vec<String>,
}

impl AntigravityBackend {
  /// The backend of one launch. `print_timeout` is the configured turn budget, and `1h` when the option sets none.
  pub fn from_launch(
    agy_bin: String,
    user_local_bin: String,
    model: &str,
    print_timeout: Option<String>,
    launch: LaunchArgs,
  ) -> Self {
    Self {
      agy_bin,
      user_local_bin,
      model: model.to_string(),
      print_timeout: print_timeout.unwrap_or_else(|| "1h".to_string()),
      instructions_content: launch.instructions_content.filter(|content| !content.is_empty()),
      resume_session_id: launch.resume_session_id.filter(|id| !id.is_empty()),
      extra_flags: launch.extra_flags,
    }
  }

  /// The `--print` command. The framed prompt is the value of `--print=`, one argv element, so a prompt that begins
  /// with a dash stays a prompt.
  fn argv(&self, prompt: &str) -> Vec<String> {
    let mut command = vec![
      self.agy_bin.clone(),
      format!("--print={}", framed_prompt(self.instructions_content.as_deref(), prompt)),
      "--print-timeout".to_string(),
      self.print_timeout.clone(),
      SKIP_PERMISSIONS_FLAG.to_string(),
      "--output-format".to_string(),
      "json".to_string(),
      "--model".to_string(),
      self.model.clone(),
    ];
    if let Some(session_id) = &self.resume_session_id {
      command.extend(["--conversation".to_string(), session_id.clone()]);
    }
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

  /// The tail of `run` (`antigravity_cli.py:109-165`): the events that the exit and the envelope make, and the guard
  /// failure the run ends with, when there is one. A failing guard keeps the exit code the process had.
  fn finish(&self, stdout: &str, stderr: &str, exit_code: i32) -> FinalStdoutOutcome {
    let stdout_text = str_methods::strip(stdout);
    if exit_code != 0 {
      let message = nonzero_exit_message(stdout_text, exit_code);
      return finished(vec![make_error_event(&message)], exit_code, None);
    }

    // The envelope is the only machine-readable source of the conversation id and the usage, so stdout that is not an
    // envelope breaches the contract.
    let Some(envelope) = parse_envelope(stdout_text) else {
      let preview: String = stdout_text.chars().take(NON_ENVELOPE_PREVIEW_CHARS).collect();
      return finished(
        vec![make_error_event(&format!("agy exited 0 with non-envelope stdout: {preview}"))],
        exit_code,
        Some(format!("antigravity envelope guard: non-json stdout (exit 0): {preview}")),
      );
    };

    if let Some(timeout) = PRINT_TIMEOUT_RE.captures(stderr).map(|captures| captures[1].to_string()) {
      let message =
        format!("agy print timeout after {timeout}: the turn was still running; agy returned partial output");
      return finished(vec![make_error_event(&message)], 1, None);
    }

    if envelope.status != "SUCCESS" {
      let message = ["error", "response"]
        .iter()
        .filter_map(|key| envelope.fields.get(*key))
        .find(|value| truthy(value))
        .map_or_else(|| format!("Antigravity status {}", envelope.status), value_str);
      return finished(vec![make_error_event(&message)], 1, None);
    }

    let Some(conversation_id) = envelope.fields.get("conversation_id").filter(|value| truthy(value)) else {
      return finished(
        vec![make_error_event("agy SUCCESS envelope missing conversation_id")],
        exit_code,
        Some("antigravity envelope guard: SUCCESS envelope missing conversation_id".to_string()),
      );
    };

    if let Some(anchor) = &self.resume_session_id
      && conversation_id.as_str() != Some(anchor.as_str())
    {
      let observed = value_str(conversation_id);
      return finished(
        vec![make_error_event(&format!("agy resume envelope id {observed} does not match anchor {anchor}"))],
        exit_code,
        Some(format!("antigravity envelope guard: resume envelope id {observed} does not match anchor {anchor}")),
      );
    }

    // The session-attach event comes first, so the manager adopts the conversation id as its anchor. Then the text,
    // then the usage.
    let mut events = vec![json!({"type": event_types::SESSION_ATTACHED, "session_id": conversation_id})];
    let response = match envelope.fields.get("response") {
      None => "",
      Some(Value::String(response)) => response,
      Some(other) => {
        let guard = format!("antigravity envelope guard: response must be a string, got {other}");
        return finished(events, exit_code, Some(guard));
      }
    };
    events.push(make_text_event(&strip_platform_notifications(response)));
    match usage_event(&envelope.fields) {
      Ok(event) => events.push(event),
      Err(error) => return finished(events, exit_code, Some(format!("{error:#}"))),
    }
    finished(events, exit_code, None)
  }
}

impl AgentBackend for AntigravityBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    None
  }

  fn prepare_transport(&mut self, _log_dir: &Path) -> anyhow::Result<()> {
    Ok(())
  }

  fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>> {
    Ok(self.argv(prompt))
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(self.child_env(env))
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

/// An `agy` JSON envelope: an object whose `status` is a string. The other fields stay in `fields`.
struct Envelope {
  status: String,
  fields: Map<String, Value>,
}

/// `_parse_envelope`: the stdout as an envelope, or `None` when it is not one.
fn parse_envelope(stdout: &str) -> Option<Envelope> {
  match serde_json::from_str::<Value>(stdout) {
    Ok(Value::Object(mut fields)) => match fields.remove("status") {
      Some(Value::String(status)) => Some(Envelope { status, fields }),
      _ => None,
    },
    _ => None,
  }
}

/// The message of a nonzero exit: the envelope's error when it names one, else the stdout, else the exit code.
fn nonzero_exit_message(stdout_text: &str, exit_code: i32) -> String {
  let envelope_error = parse_envelope(stdout_text)
    .and_then(|envelope| envelope.fields.get("error").and_then(Value::as_str).map(str::to_string));
  match envelope_error {
    Some(error) => format!("agy exited {exit_code}: {error}"),
    None if stdout_text.is_empty() => format!("Antigravity CLI exited with code {exit_code}"),
    None => stdout_text.to_string(),
  }
}

fn strip_platform_notifications(text: &str) -> String {
  SYSTEM_MESSAGE_RE.replace_all(text, "").into_owned()
}

/// The result event of the envelope's usage: the input count, the output count with its thinking tokens, and the cache
/// reads. The cost is the int 0, as Python's default carries it.
fn usage_event(fields: &Map<String, Value>) -> anyhow::Result<Value> {
  let empty = Map::new();
  let usage = match fields.get("usage") {
    None => &empty,
    Some(value) if !truthy(value) => &empty,
    Some(Value::Object(usage)) => usage,
    Some(other) => bail!("antigravity envelope usage must be a JSON object, got {other}"),
  };
  Ok(make_result_event(
    int_field(usage, "input_tokens")?,
    int_field(usage, "output_tokens")? + int_field(usage, "thinking_tokens")?,
    int_field(usage, "cache_read_tokens")?,
    0,
    json!(0),
    None,
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_agents::agent_environment::AgentBaseEnvironment;
  use charliebot_agents::one_shot::frame_system_prompt;
  use charliebot_agents::runner::{AgentRunner, RunRequest};
  use charliebot_agents::transport::cgroup::SessionCgroupConfig;
  use charliebot_agents::transport::{cursor, final_stdout, follow, identity, kill, recovery};
  use std::fs;
  use std::os::unix::fs::symlink;
  use std::process::{Command, Stdio};
  use std::sync::{Arc, Mutex};
  use std::time::Duration;

  const SUCCESS: &str = r#"{"status":"SUCCESS","conversation_id":"conv-abc","num_turns":2,"response":"the answer","usage":{"input_tokens":10,"output_tokens":12,"thinking_tokens":11,"cache_read_tokens":3}}"#;

  fn capture_backend(root: &Path) -> AntigravityBackend {
    AntigravityBackend::from_launch(
      root.join("agy").to_string_lossy().into_owned(),
      root.to_string_lossy().into_owned(),
      "test",
      None,
      LaunchArgs::default(),
    )
  }

  async fn capture_run(root: &Path) -> anyhow::Result<(charliebot_agents::runner::RunOutcome, Vec<Value>)> {
    let runner = AgentRunner::new(
      "antigravity",
      Box::new(capture_backend(root)),
      LaunchArgs { log_dir: Some(root.join("logs")), ..LaunchArgs::default() },
    );
    let mut request = RunRequest::new(
      "test".into(),
      root.to_path_buf(),
      AgentBaseEnvironment { values: Env::from([("PATH".into(), "/bin:/usr/bin".into())]) },
      SessionCgroupConfig::disabled(),
      Arc::new(charliebot_registry::Registry::new()),
    );
    let pid_file = root.join("pid");
    // The supervisor runs as another process, so it is the helper binary rather than this test binary: an agent crate's
    // test binary handles no `--supervise-final-stdout` entry point of its own.
    request.final_stdout_supervisor = Some(fake_program_bin());
    request.gate_child_until_on_spawn();
    request.on_spawn = Some(Arc::new(move |identity| {
      let path = pid_file.clone();
      Box::pin(async move {
        fs::write(path, identity.pid.to_string())?;
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
    let outcome = runner.run(request).await?;
    let events = seen.lock().unwrap().clone();
    Ok((outcome, events))
  }

  #[test]
  #[ignore = "subprocess server for restart tests"]
  fn capturing_server() {
    let root = std::env::var_os("CHARLIEBOT_TEST_AGY_ROOT").unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(capture_run(Path::new(&root))).unwrap();
  }

  async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
      while !condition() {
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .unwrap();
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

  fn stage_capture(root: &Path) {
    fs::write(root.join("reply.json"), SUCCESS).unwrap();
    symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/final_stdout_agy.sh"), root.join("agy"))
      .unwrap();
  }

  async fn cut_server(root: &Path) -> u32 {
    let mut server = Command::new(std::env::current_exe().unwrap())
      .args(["--exact", "antigravity_cli::tests::capturing_server", "--ignored"])
      .env("CHARLIEBOT_TEST_AGY_ROOT", root)
      .stdout(Stdio::null())
      .stderr(Stdio::inherit())
      .spawn()
      .unwrap();
    wait_until(|| root.join("started").exists()).await;
    let pid = fs::read_to_string(root.join("pid")).unwrap().parse().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
    assert!(identity::read_pid_stat(pid).is_some_and(|stat| stat.state != "Z"));
    let agent_pid = fs::read_to_string(root.join("started")).unwrap().parse().unwrap();
    assert!(identity::read_pid_stat(agent_pid).is_some_and(|stat| stat.state != "Z"));
    pid
  }

  async fn drain_capture(root: &Path, alive: impl FnMut() -> bool) -> Vec<Value> {
    let directory = root.join("logs");
    let mut backend = capture_backend(root);
    let outcome = final_stdout::materialize(&mut backend, &directory, None).unwrap();
    assert_eq!(outcome.exit_code, 0);
    assert!(outcome.guard_error.is_none());
    let cursor_path = directory.join(charliebot_store::run_records::CURSOR_NAME);
    let mut events = Vec::new();
    follow::tail_follow_events(
      &directory.join(charliebot_store::run_records::RAW_LOG_NAME),
      |event| Ok(vec![event]),
      |event| {
        events.push(event);
        std::future::ready(Ok(()))
      },
      alive,
      Some(&cursor_path),
      cursor::read_raw_cursor(&cursor_path),
      Duration::ZERO,
      usize::MAX,
      follow::TAIL_POLL_INTERVAL,
      None,
      Duration::ZERO,
    )
    .await
    .unwrap();
    events
  }

  #[tokio::test]
  async fn final_stdout_survives_server_death_and_matches_uninterrupted_events_and_usage() {
    let uninterrupted = tempfile::tempdir().unwrap();
    stage_capture(uninterrupted.path());
    fs::write(uninterrupted.path().join("release"), "").unwrap();
    let (expected, events) = capture_run(uninterrupted.path()).await.unwrap();
    let restarted = tempfile::tempdir().unwrap();
    stage_capture(restarted.path());
    let pid = cut_server(restarted.path()).await;
    assert!(!restarted.path().join("logs/final_stdout.exit.json").exists());
    fs::write(restarted.path().join("release"), "").unwrap();
    wait_until(|| restarted.path().join("logs/final_stdout.exit.json").exists()).await;
    wait_until(|| identity::read_pid_stat(pid).is_none_or(|stat| stat.state == "Z")).await;
    let adopted = drain_capture(restarted.path(), || false).await;
    assert_eq!(adopted, events);
    let (_, _, code) =
      recovery::scan_result_exit(&restarted.path().join("logs/agent.raw.ndjson"), |event| Ok(vec![event])).unwrap();
    assert_eq!(Some(code), expected.exit_code);
    assert_eq!(
      fs::read(restarted.path().join("logs/stdout.log")).unwrap(),
      fs::read(uninterrupted.path().join("logs/stdout.log")).unwrap()
    );
    assert_eq!(
      fs::read(restarted.path().join("logs").join(charliebot_store::run_records::STDERR_LOG_NAME)).unwrap(),
      fs::read(uninterrupted.path().join("logs").join(charliebot_store::run_records::STDERR_LOG_NAME)).unwrap()
    );
    assert!(drain_capture(restarted.path(), || false).await.is_empty());
  }

  #[tokio::test]
  async fn final_stdout_answer_written_during_downtime_uses_the_dead_process_completion_time() {
    let root = tempfile::tempdir().unwrap();
    stage_capture(root.path());
    let pid = cut_server(root.path()).await;
    fs::write(root.path().join("release"), "").unwrap();
    wait_until(|| root.path().join("logs/final_stdout.exit.json").exists()).await;
    wait_until(|| identity::read_pid_stat(pid).is_none_or(|stat| stat.state == "Z")).await;
    let raw = root.path().join("logs/agent.raw.ndjson");
    assert!(!raw.exists());
    let completed = recovery::raw_completion_time(&raw).unwrap();
    let expected =
      time::OffsetDateTime::from(fs::metadata(root.path().join("logs/stdout.log")).unwrap().modified().unwrap());
    assert_eq!(completed, expected);
    let events = drain_capture(root.path(), || false).await;
    assert!(events.iter().any(|event| event.to_string().contains("the answer")));
    assert_eq!(recovery::raw_completion_time(&raw), Some(completed));
  }

  #[tokio::test]
  async fn final_stdout_stop_after_server_death_ends_the_process_group() {
    let root = tempfile::tempdir().unwrap();
    stage_capture(root.path());
    let pid = cut_server(root.path()).await;
    kill::kill_group_escalating(pid, || identity::read_pid_stat(pid).is_some_and(|stat| stat.state != "Z")).await;
    let agent_pid = fs::read_to_string(root.path().join("started")).unwrap().parse().unwrap();
    assert!(identity::read_pid_stat(pid).is_none_or(|stat| stat.state == "Z"));
    wait_until(|| identity::read_pid_stat(agent_pid).is_none_or(|stat| stat.state == "Z")).await;
    assert_eq!(final_stdout::process_exit_code(&root.path().join("logs"), None).unwrap(), -15);
  }

  fn backend(launch: LaunchArgs) -> AntigravityBackend {
    AntigravityBackend::from_launch(
      "/usr/bin/agy".to_string(),
      "/synthetic/local-bin".to_string(),
      "Gemini Pro (test)",
      None,
      launch,
    )
  }

  #[test]
  fn argv_passes_the_prompt_as_the_print_value_and_splices_the_conversation() {
    let resumed = backend(LaunchArgs {
      resume_session_id: Some("conv-abc".to_string()),
      extra_flags: vec!["--sandbox".to_string()],
      ..LaunchArgs::default()
    });
    let expected: Vec<&str> = vec![
      "/usr/bin/agy",
      "--print=continue",
      "--print-timeout",
      "1h",
      "--dangerously-skip-permissions",
      "--output-format",
      "json",
      "--model",
      "Gemini Pro (test)",
      "--conversation",
      "conv-abc",
      "--sandbox",
    ];
    assert_eq!(resumed.argv("continue"), expected);
    assert_eq!(backend(LaunchArgs::default()).argv("--dash-prefixed prompt")[1], "--print=--dash-prefixed prompt");
  }

  #[test]
  fn the_instructions_are_framed_into_the_print_value() {
    let instructed =
      backend(LaunchArgs { instructions_content: Some("Build stuff.".to_string()), ..LaunchArgs::default() });
    assert_eq!(
      instructed.argv("do the work")[1],
      format!("--print={}", frame_system_prompt("Build stuff.", "do the work"))
    );
  }

  #[test]
  fn the_child_env_loses_the_google_keys_and_gains_the_user_local_bin() {
    let env = Env::from([
      ("PATH".to_string(), "/usr/bin".to_string()),
      ("GEMINI_API_KEY".to_string(), "secret".to_string()),
      ("GOOGLE_API_KEY".to_string(), "secret".to_string()),
      ("OTHER_VAR".to_string(), "keep".to_string()),
    ]);
    let prepared = backend(LaunchArgs::default()).child_env(env);
    assert_eq!(prepared.get("PATH").map(String::as_str), Some("/synthetic/local-bin:/usr/bin"));
    assert_eq!(prepared.get("OTHER_VAR").map(String::as_str), Some("keep"));
    assert!(!prepared.contains_key("GEMINI_API_KEY") && !prepared.contains_key("GOOGLE_API_KEY"));
  }

  #[test]
  fn a_success_envelope_becomes_the_session_text_and_usage_events() {
    let outcome = backend(LaunchArgs::default()).finish(SUCCESS, "", 0);
    assert_eq!(outcome.guard_error, None);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
      outcome.events,
      vec![
        json!({"type": "session_attached", "session_id": "conv-abc"}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "the answer"}]}}),
        json!({
          "type": "result",
          "result": "",
          "usage": {
            "input_tokens": 10,
            "output_tokens": 23,
            "cache_read_input_tokens": 3,
            "cache_creation_input_tokens": 0
          },
          "total_cost_usd": 0
        }),
      ]
    );
  }

  #[test]
  fn a_print_timeout_in_stderr_fails_a_success_envelope_with_exit_one() {
    let stderr = "[agy] print timeout after 2s with turn in progress; returning partial output\n";
    let outcome = backend(LaunchArgs::default()).finish(SUCCESS, stderr, 0);
    assert_eq!(outcome.events.len(), 1);
    assert_eq!(outcome.events[0]["type"], "error");
    assert!(outcome.events[0]["message"].as_str().unwrap().contains("2s"));
    assert_eq!((outcome.exit_code, outcome.guard_error), (1, None));
  }

  #[test]
  fn a_failed_status_and_a_nonzero_exit_each_report_their_error_with_the_right_exit_code() {
    let failed = backend(LaunchArgs::default()).finish(r#"{"status":"ERROR","error":"quota exhausted"}"#, "", 0);
    assert_eq!(
      failed.events,
      vec![json!({"type": "error", "message": "quota exhausted", "content": "quota exhausted"})]
    );
    assert_eq!((failed.exit_code, failed.guard_error), (1, None));

    let named = backend(LaunchArgs::default()).finish(r#"{"status":"ERROR","error":"bad model"}"#, "", 1);
    assert_eq!(named.events[0]["message"], "agy exited 1: bad model");
    assert_eq!((named.exit_code, named.guard_error), (1, None));

    let silent = backend(LaunchArgs::default()).finish("", "", 137);
    assert_eq!(silent.events[0]["message"], "Antigravity CLI exited with code 137");
    assert_eq!((silent.exit_code, silent.guard_error), (137, None));
  }

  #[test]
  fn a_contract_breach_on_a_zero_exit_names_a_guard_failure_and_keeps_the_exit_code() {
    let not_json = backend(LaunchArgs::default()).finish("plain text", "", 0);
    assert!(not_json.guard_error.unwrap().contains("non-json stdout"));
    assert_eq!(not_json.exit_code, 0);

    let no_conversation = backend(LaunchArgs::default()).finish(r#"{"status":"SUCCESS","response":"x"}"#, "", 0);
    assert!(no_conversation.guard_error.unwrap().contains("conversation_id"));
    assert_eq!(no_conversation.events.len(), 1);

    let resumed = backend(LaunchArgs { resume_session_id: Some("other".to_string()), ..LaunchArgs::default() });
    let mismatch = resumed.finish(SUCCESS, "", 0);
    assert!(mismatch.guard_error.unwrap().contains("conv-abc"));
    assert_eq!(mismatch.events.len(), 1);
  }

  #[test]
  fn platform_notices_are_stripped_from_the_response() {
    let text = "<SYSTEM_MESSAGE>\nnotice\n</SYSTEM_MESSAGE>\n\nhello";
    assert_eq!(strip_platform_notifications(text), "hello");
  }
}
