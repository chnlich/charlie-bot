//! The cc-claude command, environment, translator, and context reading.

use std::collections::HashSet;
use std::env;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_registry::backend::{AgentBackend, ContextLimits, Env, LaunchArgs, Translate, TransportKind};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::transport::kill::{kill_process_group, python_exit_code};

use super::claude_launch;
use super::login_dirs;

pub const HEADLESS_DISALLOWED_TOOLS: &str = concat!(
  "Monitor,ScheduleWakeup,CronCreate,CronDelete,CronList,Agent,Workflow,TaskCreate,",
  "TaskGet,TaskUpdate,TaskList,TaskStop,TaskOutput,SendMessage,ListAgents"
);
pub const DISALLOWED_TOOLS_FLAG: &str = "--disallowed-tools";
pub const SKIP_PERMISSIONS_FLAG: &str = "--dangerously-skip-permissions";
pub const INSTRUCTIONS_FILE_NAME: &str = "CLAUDE.md";
pub const INSTRUCTIONS_WRITTEN_EVENT: &str = "claude_code_wrote_claude_md";
const SYNTHETIC_MODEL: &str = "<synthetic>";
const ONE_SHOT_DISALLOWED_TOOLS: &str = "Bash,Read,Write,Edit,Glob,Grep,Agent";

static DECLARED_WINDOW_WARNINGS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Apply the common Claude child edits registered for every agent backend.
pub fn claude_child_env(env: &mut Env) {
  env.remove("CLAUDECODE");
  env.insert("CLAUDE_CODE_DISABLE_AUTO_MEMORY".to_string(), "1".to_string());
}

/// Build the inherited environment variables for a headless Claude process.
pub fn headless_claude_env(source: &Env) -> Env {
  claude_launch::headless_claude_env(source)
}

/// Return Claude's declared window and the reserve used to derive its compact point.
pub fn claude_reading_limits() -> ContextLimits {
  let mut source = Env::new();
  for name in claude_launch::HEADLESS_FORWARDED_ENV {
    if let Ok(value) = env::var(name) {
      source.insert(name.to_string(), value);
    }
  }
  claude_reading_limits_from(&source)
}

/// Return Claude's reading limits for a supplied host environment.
pub fn claude_reading_limits_from(source: &Env) -> ContextLimits {
  let env = headless_claude_env(source);
  let raw_window =
    env.get(claude_launch::AUTO_COMPACT_WINDOW_ENV).expect("headless environment always carries the compact window");
  let window = match parse_python_positive_integer(raw_window) {
    Some(value) => value,
    None => {
      warn_unparseable_window(raw_window);
      433_000
    }
  };
  for name in [claude_launch::AUTOCOMPACT_PCT_OVERRIDE_ENV, claude_launch::MAX_CONTEXT_TOKENS_ENV] {
    if let Some(value) = env.get(name) {
      warn_degraded_window(name, value);
      return ContextLimits { declared_window: Some(window), compact_reserve: None };
    }
  }
  ContextLimits {
    declared_window: Some(window),
    compact_reserve: Some(claude_launch::COMPACT_OUTPUT_RESERVE + claude_launch::COMPACT_CONTEXT_RESERVE),
  }
}

fn parse_python_positive_integer(raw: &str) -> Option<u64> {
  let value = raw.trim();
  let (positive, digits) = match value.strip_prefix('+') {
    Some(digits) => (true, digits),
    None => match value.strip_prefix('-') {
      Some(digits) => (false, digits),
      None => (true, value),
    },
  };
  let characters = digits.chars().collect::<Vec<_>>();
  if characters.is_empty()
    || !characters.iter().enumerate().all(|(index, character)| {
      character.is_ascii_digit()
        || (*character == '_'
          && index > 0
          && index + 1 < characters.len()
          && characters[index - 1].is_ascii_digit()
          && characters[index + 1].is_ascii_digit())
    })
    || !positive
  {
    return None;
  }
  let normalized = characters.into_iter().filter(|character| *character != '_').collect::<String>();
  normalized.parse::<u64>().ok().filter(|number| *number > 0)
}

fn warn_unparseable_window(window: &str) {
  let key = format!("window:{window}");
  if remember_warning(key) {
    tracing::warn!(
      target: "charliebot_agents",
      variable = claude_launch::AUTO_COMPACT_WINDOW_ENV,
      window,
      "claude_declared_window_unparseable_window"
    );
  }
}

fn warn_degraded_window(variable: &str, value: &str) {
  let key = format!("degraded:{variable}:{value}");
  if remember_warning(key) {
    tracing::warn!(
      target: "charliebot_agents",
      variable,
      value,
      reason = "forwarded but semantics not modelled; returning declared window without compaction point",
      "claude_declared_window_degraded"
    );
  }
}

fn remember_warning(key: String) -> bool {
  DECLARED_WINDOW_WARNINGS
    .get_or_init(|| Mutex::new(HashSet::new()))
    .lock()
    .expect("declared window warning registry is not poisoned")
    .insert(key)
}

/// The per-process Claude Code CLI backend.
pub struct ClaudeCodeBackend {
  model: Option<String>,
  effort: Option<String>,
  cli_binary: Option<String>,
  fast_mode: bool,
  claude_session_id: Option<String>,
  claude_config_dir: Option<String>,
  extra_flags: Vec<String>,
  instructions_file: Option<PathBuf>,
}

impl ClaudeCodeBackend {
  pub(crate) fn from_launch(
    model: Option<String>,
    effort: Option<String>,
    cli_binary: Option<String>,
    fast_mode: bool,
    claude_config_dir: Option<String>,
    launch: LaunchArgs,
  ) -> anyhow::Result<Self> {
    let claude_session_id = launch.backend_kwargs.get("claude_session_id");
    let claude_session_id = match claude_session_id {
      None | Some(Value::Null) => None,
      Some(Value::String(value)) => Some(value.clone()),
      Some(_) => bail!("claude_session_id must be a string"),
    }
    .filter(|value| !value.is_empty());
    Ok(Self {
      model,
      effort: effort.filter(|value| !value.is_empty()),
      cli_binary: cli_binary.filter(|value| !value.is_empty()),
      fast_mode,
      claude_session_id,
      claude_config_dir: claude_config_dir.filter(|value| !value.is_empty()),
      extra_flags: launch.extra_flags,
      instructions_file: launch.instructions_file,
    })
  }

  /// Build the Claude command used by the raw-log runner.
  pub fn build_command(&self, _prompt: &str) -> anyhow::Result<Vec<String>> {
    let mut command = vec![
      self.cli_binary.clone().unwrap_or_else(|| "claude".to_string()),
      "-p".to_string(),
      "--output-format".to_string(),
      "stream-json".to_string(),
      "--verbose".to_string(),
      SKIP_PERMISSIONS_FLAG.to_string(),
      DISALLOWED_TOOLS_FLAG.to_string(),
      HEADLESS_DISALLOWED_TOOLS.to_string(),
    ];
    if let Some(session_id) = &self.claude_session_id {
      command.extend(["--session-id".to_string(), session_id.clone()]);
    }
    if let Some(model) = &self.model {
      command.extend(["--model".to_string(), model.clone()]);
    }
    if let Some(effort) = &self.effort {
      command.extend(["--effort".to_string(), effort.clone()]);
    }
    let mut settings = Map::new();
    settings.insert(claude_launch::DISABLE_CONNECTOR_SETTINGS_KEY.to_string(), Value::Bool(true));
    if self.fast_mode {
      settings.insert(claude_launch::FAST_MODE_SETTINGS_KEY.to_string(), Value::Bool(true));
    }
    command.extend(["--settings".to_string(), Value::Object(settings).to_string()]);
    command.extend(self.extra_flags.clone());
    if let Some(path) = &self.instructions_file {
      command.extend(["--append-system-prompt-file".to_string(), path.to_string_lossy().into_owned()]);
    }
    Ok(command)
  }

  fn prepare_env(&self, mut env: Env) -> anyhow::Result<Env> {
    let headless = headless_claude_env(&env);
    env.extend(headless);
    if let Some(config_dir) = &self.claude_config_dir {
      let home = env.get("HOME").map(String::as_str);
      env.insert(
        login_dirs::CLAUDE_CONFIG_DIR_ENV.to_string(),
        login_dirs::expanduser(config_dir, home)?.to_string_lossy().into_owned(),
      );
    }
    Ok(env)
  }

  async fn run_one_shot(&self, prompt: &str, system: &str, timeout: Option<Duration>) -> anyhow::Result<String> {
    let model =
      self.model.as_deref().filter(|value| !value.is_empty()).context("cc-claude one-shot requires a model")?;
    let command = one_shot_command(model, system);
    let mut child = Command::new(&command[0]);
    child.args(&command[1..]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
    child.stderr(std::process::Stdio::piped()).process_group(0);
    let mut child = child.spawn()?;
    let pid = child.id().context("claude CLI did not expose its process id")?;
    let mut stdin = child.stdin.take().context("claude CLI stdin was not piped")?;
    let prompt = prompt.as_bytes().to_vec();
    let stdin_task = tokio::spawn(async move {
      stdin.write_all(&prompt).await?;
      stdin.shutdown().await
    });
    let output_future = child.wait_with_output();
    tokio::pin!(output_future);
    let output = match timeout {
      Some(limit) => match tokio::time::timeout(limit, &mut output_future).await {
        Ok(result) => result?,
        Err(error) => {
          kill_process_group(pid, nix::sys::signal::Signal::SIGKILL);
          let _ = output_future.await;
          stdin_task.abort();
          let _ = stdin_task.await;
          return Err(error.into());
        }
      },
      None => output_future.await?,
    };
    stdin_task.await.context("waiting for claude CLI stdin writer failed")??;
    let exit_code = python_exit_code(output.status);
    if exit_code != 0 {
      let stderr = String::from_utf8(output.stderr)?;
      bail!("claude CLI failed (exit {exit_code}): {}", stderr.trim());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
  }
}

impl AgentBackend for ClaudeCodeBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    Some((INSTRUCTIONS_FILE_NAME, INSTRUCTIONS_WRITTEN_EVENT))
  }

  fn prepare_transport(&mut self, _log_dir: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
  }

  fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>> {
    ClaudeCodeBackend::build_command(self, prompt)
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    ClaudeCodeBackend::prepare_env(self, env)
  }

  fn stdin_prompt(&self, prompt: &str) -> Option<String> {
    Some(prompt.to_string())
  }

  fn translator(&self) -> Box<dyn Translate> {
    Box::new(IdentityTranslate)
  }

  fn transport(&self) -> TransportKind {
    TransportKind::RawLog
  }

  fn one_shot_text<'a>(
    &'a mut self,
    prompt: &'a str,
    system: &'a str,
    timeout: Option<Duration>,
  ) -> BoxFuture<'a, anyhow::Result<String>> {
    Box::pin(async move { self.run_one_shot(prompt, system, timeout).await })
  }
}

struct IdentityTranslate;

impl Translate for IdentityTranslate {
  fn translate(&mut self, raw: Value) -> Vec<Value> {
    vec![raw]
  }
}

fn one_shot_command(model: &str, system: &str) -> Vec<String> {
  vec![
    "claude".to_string(),
    "-p".to_string(),
    "--output-format".to_string(),
    "text".to_string(),
    "--no-session-persistence".to_string(),
    "--model".to_string(),
    model.to_string(),
    "--system-prompt".to_string(),
    system.to_string(),
    DISALLOWED_TOOLS_FLAG.to_string(),
    ONE_SHOT_DISALLOWED_TOOLS.to_string(),
  ]
}

/// Detect visible replies authored by a model outside the configured model family.
pub fn out_of_family_served_models(events: &[Value], configured_model: &str) -> Vec<String> {
  if configured_model.is_empty() {
    return Vec::new();
  }
  let mut seen = HashSet::new();
  let mut served = Vec::new();
  for event in events {
    if truthy(event.get("parent_tool_use_id")) || event.get("type").and_then(Value::as_str) != Some("assistant") {
      continue;
    }
    let Some(message) = event.get("message").and_then(Value::as_object) else {
      continue;
    };
    let Some(model) = message.get("model").and_then(Value::as_str) else {
      continue;
    };
    if model.is_empty() || model == SYNTHETIC_MODEL {
      continue;
    }
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
      continue;
    };
    if !blocks.iter().any(|block| block.get("type").and_then(Value::as_str) == Some("text")) {
      continue;
    }
    if same_model_family(model, configured_model) || !seen.insert(model.to_string()) {
      continue;
    }
    served.push(model.to_string());
  }
  served
}

fn same_model_family(served: &str, configured: &str) -> bool {
  let served = strip_date_suffix(served);
  let configured = strip_date_suffix(configured);
  let (shorter, longer) = if served.len() <= configured.len() { (served, configured) } else { (configured, served) };
  longer == shorter || longer.strip_prefix(shorter).is_some_and(|tail| tail.starts_with('-'))
}

fn strip_date_suffix(model: &str) -> &str {
  if let Some((prefix, suffix)) = model.rsplit_once('-')
    && suffix.len() == 8
    && suffix.bytes().all(|byte| byte.is_ascii_digit())
  {
    prefix
  } else {
    model
  }
}

fn truthy(value: Option<&Value>) -> bool {
  match value {
    None | Some(Value::Null) | Some(Value::Bool(false)) => false,
    Some(Value::Bool(true)) => true,
    Some(Value::Number(number)) => number.as_f64().is_none_or(|value| value != 0.0),
    Some(Value::String(value)) => !value.is_empty(),
    Some(Value::Array(values)) => !values.is_empty(),
    Some(Value::Object(values)) => !values.is_empty(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn command_has_headless_settings_and_stdin_prompt() {
    let backend = ClaudeCodeBackend::from_launch(
      Some("claude-fable-5-1".to_string()),
      Some("high".to_string()),
      Some("/synthetic/claude".to_string()),
      true,
      None,
      LaunchArgs { extra_flags: vec!["--resume".to_string(), "resume-id".to_string()], ..LaunchArgs::default() },
    )
    .unwrap();
    let command = backend.build_command("prompt").unwrap();
    let settings = command[command.iter().position(|value| value == "--settings").unwrap() + 1].clone();
    assert_eq!(command[0], "/synthetic/claude");
    assert!(command.windows(2).any(|pair| pair == ["--model", "claude-fable-5-1"]));
    assert!(command.windows(2).any(|pair| pair == ["--effort", "high"]));
    assert!(command.windows(2).any(|pair| pair == ["--resume", "resume-id"]));
    assert_eq!(settings, r#"{"disableClaudeAiConnectors":true,"fastMode":true}"#);
    assert_eq!(AgentBackend::stdin_prompt(&backend, "prompt").as_deref(), Some("prompt"));
  }

  #[test]
  fn headless_environment_forwards_host_settings_and_pins_defaults() {
    let source = Env::from([
      ("PATH".to_string(), "/synthetic/bin".to_string()),
      (claude_launch::AUTO_COMPACT_WINDOW_ENV.to_string(), "300000".to_string()),
      ("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE".to_string(), "0.7".to_string()),
    ]);
    let result = headless_claude_env(&source);
    assert_eq!(result["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"], "1");
    assert_eq!(result[claude_launch::AUTO_COMPACT_WINDOW_ENV], "300000");
    assert_eq!(result["CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"], "0.7");
    assert!(!result.contains_key(claude_launch::SIMPLE_SYSTEM_PROMPT_ENV));
  }

  #[test]
  fn reading_limits_use_host_window_and_drop_unknown_compact_reserve() {
    let source = Env::from([
      (claude_launch::AUTO_COMPACT_WINDOW_ENV.to_string(), "300000".to_string()),
      (claude_launch::MAX_CONTEXT_TOKENS_ENV.to_string(), "200000".to_string()),
    ]);
    assert_eq!(
      claude_reading_limits_from(&source),
      ContextLimits { declared_window: Some(300_000), compact_reserve: None }
    );
  }

  #[test]
  fn reading_limits_parse_python_integer_whitespace_sign_and_separators() {
    let source = Env::from([(claude_launch::AUTO_COMPACT_WINDOW_ENV.to_string(), " +300_000 ".to_string())]);
    assert_eq!(
      claude_reading_limits_from(&source),
      ContextLimits { declared_window: Some(300_000), compact_reserve: Some(33_000) }
    );
  }

  #[test]
  fn out_of_family_detector_uses_visible_main_chain_text_models_once() {
    let events = [
      serde_json::json!({"type":"assistant","message":{"model":"claude-fable-5-1","content":[{"type":"text"}]}}),
      serde_json::json!({"type":"assistant","message":{"model":"claude-opus-4-8","content":[{"type":"text"}]}}),
      serde_json::json!({"type":"assistant","message":{"model":"claude-opus-4-8","content":[{"type":"text"}]}}),
      serde_json::json!({"type":"assistant","parent_tool_use_id":"tool","message":{"model":"claude-haiku-4-5","content":[{"type":"text"}]}}),
      serde_json::json!({"type":"assistant","message":{"model":"<synthetic>","content":[{"type":"text"}]}}),
    ];
    assert_eq!(out_of_family_served_models(&events, "claude-fable-5-1"), ["claude-opus-4-8"]);
  }

  #[test]
  fn cli_native_one_shot_uses_text_mode_and_no_session_persistence() {
    let command = one_shot_command("claude-fable-5-1", "system");
    assert_eq!(
      command,
      [
        "claude",
        "-p",
        "--output-format",
        "text",
        "--no-session-persistence",
        "--model",
        "claude-fable-5-1",
        "--system-prompt",
        "system",
        "--disallowed-tools",
        ONE_SHOT_DISALLOWED_TOOLS
      ]
    );
  }
}
