//! Cold-cache Sonnet compaction of an account's Claude transcript.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::BufRead;
use std::path::Path;
use std::time::Duration as StdDuration;

use anyhow::Context;
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::backend::{EmitFn, Env};
use charliebot_store::metadata::UtcDateTime;
use charliebot_store::run_records::RunKind;
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

use crate::transport::cgroup::SessionCgroupConfig;
use crate::transport::kill;
use crate::transport::spawn::{ChildStdin, SpawnSetup, SpawnSpec, spawn_with_setup};

use super::{
  DISALLOWED_TOOLS_FLAG, HEADLESS_DISALLOWED_TOOLS, claude_accounts, claude_config, claude_launch, login_dirs,
};

pub const CACHE_TTL: StdDuration = StdDuration::from_secs(60 * 60);
pub const CLAUDE_COMPACTION_TIMEOUT: StdDuration = StdDuration::from_secs(900);
pub const COMPACTION_MODEL: &str = "claude-sonnet-5";
pub const FABLE_FAMILY: &str = "fable";
const COMPACTION_FAMILY: &str = "sonnet";
const COMPACT_PROMPT: &[u8] = b"/compact\n";
const BOUNDARY_MARKER: &str = "\"compact_boundary\"";
const COST_STATE_MARKER: &str = "\"cost-state\"";
const USAGE_COUNTERS: [&str; 4] = ["inputTokens", "outputTokens", "cacheReadInputTokens", "cacheCreationInputTokens"];
const EXTRA_COMPACTION_TOOLS: &str = concat!(
  "Bash,Read,Write,Edit,MultiEdit,NotebookEdit,Glob,Grep,WebSearch,WebFetch,TodoWrite,Skill,ToolSearch,",
  "KillShell,BashOutput,AskUserQuestion,ExitPlanMode,Task"
);

pub use charliebot_registry::backend::{CompactionOutcome, CompactionRun};

pub fn is_fable(model: Option<&str>) -> bool {
  model_family(model) == FABLE_FAMILY
}

pub fn cache_expired(last_request_at: Option<UtcDateTime>, now: Option<UtcDateTime>) -> bool {
  let Some(last_request_at) = last_request_at else {
    return false;
  };
  let now = claude_accounts::now_or(now);
  now.into_inner() - last_request_at.into_inner() > time::Duration::seconds(CACHE_TTL.as_secs() as i64)
}

pub fn expired_cache_compaction_wanted(
  cfg: &CharlieBotConfig,
  model: Option<&str>,
  context_tokens: Option<i64>,
  last_request_at: Option<UtcDateTime>,
  now: Option<UtcDateTime>,
) -> bool {
  is_fable(model)
    && context_tokens.is_some_and(|tokens| {
      u64::try_from(tokens).is_ok_and(|tokens| {
        tokens >= cfg.section::<claude_config::AccountsConfig>("accounts").claude_compaction.expired_cache_tokens
      })
    })
    && cache_expired(last_request_at, now)
}

pub fn relay_compaction_wanted(cfg: &CharlieBotConfig, model: Option<&str>, context_tokens: Option<i64>) -> bool {
  is_fable(model)
    && context_tokens.is_some_and(|tokens| {
      u64::try_from(tokens).is_ok_and(|tokens| {
        tokens >= cfg.section::<claude_config::AccountsConfig>("accounts").claude_compaction.relay_tokens
      })
    })
}

pub fn model_family(model: Option<&str>) -> String {
  let Some(model) = model.filter(|value| !value.is_empty()) else {
    return String::new();
  };
  let parts = model.to_lowercase().split('-').map(str::to_string).collect::<Vec<_>>();
  if parts.first().is_some_and(|part| part == "claude") && parts.len() > 1 {
    parts[1].clone()
  } else {
    parts[0].clone()
  }
}

fn read_transcript_rows(path: &Path, marker: &str) -> anyhow::Result<Vec<Value>> {
  let file = std::fs::File::open(path).with_context(|| format!("opening transcript {}", path.display()))?;
  let mut file = std::io::BufReader::new(file);
  let mut rows = Vec::new();
  let mut bytes = Vec::new();
  loop {
    bytes.clear();
    if file.read_until(b'\n', &mut bytes)? == 0 {
      break;
    }
    let line = String::from_utf8_lossy(&bytes);
    if !line.contains(marker) {
      continue;
    }
    if let Ok(row) = serde_json::from_str::<Value>(&line)
      && row.is_object()
    {
      rows.push(row);
    }
  }
  Ok(rows)
}

pub fn count_compact_boundaries(path: &Path) -> anyhow::Result<usize> {
  Ok(
    read_transcript_rows(path, BOUNDARY_MARKER)?
      .into_iter()
      .filter(|row| row.get("subtype").and_then(Value::as_str) == Some("compact_boundary"))
      .count(),
  )
}

fn newest_cost_state_model_usage(path: &Path) -> anyhow::Result<Map<String, Value>> {
  let mut newest = Map::new();
  for row in read_transcript_rows(path, COST_STATE_MARKER)? {
    if row.get("type").and_then(Value::as_str) == Some("cost-state") {
      newest = row.get("modelUsage").and_then(Value::as_object).cloned().unwrap_or_default();
    }
  }
  Ok(newest)
}

fn snake_case_keys(meta: &Map<String, Value>) -> Map<String, Value> {
  meta
    .iter()
    .map(|(key, value)| {
      let mut converted = String::new();
      let characters = key.chars().collect::<Vec<_>>();
      for (index, character) in characters.iter().enumerate() {
        if character.is_ascii_uppercase()
          && index > 0
          && (characters[index - 1].is_ascii_lowercase() || characters[index - 1].is_ascii_digit())
        {
          converted.push('_');
        }
        converted.push(character.to_ascii_lowercase());
      }
      (converted, value.clone())
    })
    .collect()
}

fn newest_boundary_compact_metadata(path: &Path) -> anyhow::Result<Option<Value>> {
  let rows = read_transcript_rows(path, BOUNDARY_MARKER)?;
  let Some(row) = rows.into_iter().rfind(|row| row.get("subtype").and_then(Value::as_str) == Some("compact_boundary"))
  else {
    return Ok(None);
  };
  Ok(row.get("compactMetadata").and_then(Value::as_object).map(|meta| Value::Object(snake_case_keys(meta))))
}

pub fn compaction_command(session_id: &str, charliebot_session_id: &str) -> Vec<String> {
  let mut settings = Map::new();
  settings.insert(claude_launch::DISABLE_CONNECTOR_SETTINGS_KEY.to_string(), Value::Bool(true));
  settings.extend(claude_launch::compaction_context_hook_settings(charliebot_session_id));
  vec![
    "claude".to_string(),
    "-p".to_string(),
    "--resume".to_string(),
    session_id.to_string(),
    "--model".to_string(),
    COMPACTION_MODEL.to_string(),
    "--output-format".to_string(),
    "json".to_string(),
    DISALLOWED_TOOLS_FLAG.to_string(),
    format!("{},{}", HEADLESS_DISALLOWED_TOOLS, EXTRA_COMPACTION_TOOLS),
    "--settings".to_string(),
    Value::Object(settings).to_string(),
  ]
}

/// Build the compaction environment from the launch environment and selected account directory.
pub fn compaction_env_from(config_dir: &Path, inherited: &Env, run_kind: RunKind) -> anyhow::Result<Env> {
  let mut env = inherited.clone();
  env.remove(crate::launch_loop::SESSION_ID_ENV);
  super::claude_child_env(&mut env);
  let headless = claude_launch::headless_claude_env(run_kind, &env);
  env.extend(headless);
  env.remove(login_dirs::CLAUDE_CONFIG_DIR_ENV);
  let path = login_dirs::expanduser(&config_dir.to_string_lossy(), inherited.get("HOME").map(String::as_str))?;
  env.insert(login_dirs::CLAUDE_CONFIG_DIR_ENV.to_string(), path.to_string_lossy().into_owned());
  Ok(env)
}

fn models_that_grew(result: &Value, baseline: &Map<String, Value>) -> Vec<String> {
  result
    .get("modelUsage")
    .and_then(Value::as_object)
    .into_iter()
    .flat_map(|usage| usage.iter())
    .filter_map(|(name, counters)| {
      let restored = baseline.get(name).and_then(Value::as_object);
      USAGE_COUNTERS
        .iter()
        .any(|counter| {
          let current = counters.get(*counter).and_then(Value::as_f64).unwrap_or(0.0);
          let previous = restored.and_then(|values| values.get(*counter)).and_then(Value::as_f64).unwrap_or(0.0);
          current > previous
        })
        .then(|| name.clone())
    })
    .collect()
}

fn result_object(stdout: &[u8]) -> Option<Value> {
  String::from_utf8_lossy(stdout)
    .lines()
    .rev()
    .find(|line| !line.trim().is_empty())
    .and_then(|line| serde_json::from_str::<Value>(line).ok())
    .filter(Value::is_object)
}

fn cache_read_growth(result: &Value, baseline: &Map<String, Value>, models: &[String]) -> i64 {
  models
    .iter()
    .map(|name| {
      let current =
        result.pointer(&format!("/modelUsage/{name}/cacheReadInputTokens")).and_then(Value::as_i64).unwrap_or(0);
      let previous =
        baseline.get(name).and_then(|value| value.get("cacheReadInputTokens")).and_then(Value::as_i64).unwrap_or(0);
      current - previous
    })
    .sum()
}

pub fn judge(
  returncode: i32,
  stdout: &[u8],
  baseline: &Map<String, Value>,
  before: usize,
  after: usize,
  family: &str,
) -> CompactionOutcome {
  let Some(result) = result_object(stdout) else {
    return CompactionOutcome {
      ok: false,
      error: Some(format!("exit {returncode}, no JSON result on stdout")),
      models: vec![],
      post_tokens: None,
      cache_read_tokens: None,
    };
  };
  let models = models_that_grew(&result, baseline);
  if returncode != 0 || result.get("is_error").and_then(Value::as_bool) == Some(true) {
    let detail = result
      .get("result")
      .filter(|value| json_truthy(value))
      .map_or_else(String::new, |value| value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string()))
      .trim()
      .chars()
      .take(200)
      .collect::<String>();
    // The fallback applies to the stripped text, so a blank result reads as no result, as claude_compaction.py does.
    let detail = if detail.is_empty() { "run reported an error".to_string() } else { detail };
    return CompactionOutcome {
      ok: false,
      error: Some(format!("exit {returncode}: {detail}")),
      models,
      post_tokens: None,
      cache_read_tokens: None,
    };
  }
  if after <= before {
    return CompactionOutcome {
      ok: false,
      error: Some("transcript gained no compact_boundary row".to_string()),
      models,
      post_tokens: None,
      cache_read_tokens: None,
    };
  }
  if models.is_empty() || models.iter().any(|model| model_family(Some(model)) != family) {
    return CompactionOutcome {
      ok: false,
      error: Some(format!(
        "compaction served by {}",
        if models.is_empty() { "no model".to_string() } else { models.join(", ") }
      )),
      models,
      post_tokens: None,
      cache_read_tokens: None,
    };
  }
  CompactionOutcome {
    ok: true,
    error: None,
    cache_read_tokens: Some(cache_read_growth(&result, baseline, &models)),
    models,
    post_tokens: None,
  }
}

fn json_truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(value) => *value,
    Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
    Value::String(value) => !value.is_empty(),
    Value::Array(value) => !value.is_empty(),
    Value::Object(value) => !value.is_empty(),
  }
}

#[allow(clippy::too_many_arguments)]
pub async fn compact_with_sonnet(
  session_id: &str,
  cwd: &Path,
  config_dir: &Path,
  base_env: &Env,
  pre_tokens: Option<i64>,
  emit: &EmitFn,
  cfg: &CharlieBotConfig,
  session_cgroup_id: &str,
  account_label: &str,
  trigger: &str,
  timeout_duration: StdDuration,
  run_kind: RunKind,
) -> anyhow::Result<bool> {
  compact_with_sonnet_inner(
    CompactionInput {
      session_id,
      cwd,
      config_dir,
      pre_tokens,
      emit,
      cfg,
      session_cgroup_id,
      account_label,
      trigger,
      timeout_duration,
    },
    |config_dir| compaction_env_from(config_dir, base_env, run_kind),
  )
  .await
}

struct CompactionInput<'a> {
  session_id: &'a str,
  cwd: &'a Path,
  config_dir: &'a Path,
  pre_tokens: Option<i64>,
  emit: &'a EmitFn,
  cfg: &'a CharlieBotConfig,
  session_cgroup_id: &'a str,
  account_label: &'a str,
  trigger: &'a str,
  timeout_duration: StdDuration,
}

async fn compact_with_sonnet_inner(
  input: CompactionInput<'_>,
  make_env: impl FnOnce(&Path) -> anyhow::Result<Env>,
) -> anyhow::Result<bool> {
  let CompactionInput {
    session_id,
    cwd,
    config_dir,
    pre_tokens,
    emit,
    cfg,
    session_cgroup_id,
    account_label,
    trigger,
    timeout_duration,
  } = input;
  let env = make_env(config_dir)?;
  let outcome = run_compaction_inner(CompactionRun {
    argv: compaction_command(session_id, session_cgroup_id),
    env,
    cwd,
    config_dir,
    session_id,
    family: COMPACTION_FAMILY,
    model: COMPACTION_MODEL,
    trigger: "manual",
    failure_trigger: None,
    pre_tokens,
    emit,
    cfg,
    session_cgroup_id: Some(session_cgroup_id),
    account_label,
    timeout_duration,
  })
  .await?;
  if !outcome.ok {
    tracing::warn!(target: "charliebot_agents", session = %session_id, account = account_label, trigger, "claude_compaction_failed");
  }
  Ok(outcome.ok)
}

/// Run the supplied Claude CLI line with `/compact`, judge its final JSON result, and emit the caller's event triggers.
pub async fn run_compaction(run: CompactionRun<'_>) -> anyhow::Result<CompactionOutcome> {
  run_compaction_inner(run).await
}

async fn run_compaction_inner(run: CompactionRun<'_>) -> anyhow::Result<CompactionOutcome> {
  let CompactionRun {
    argv,
    env,
    cwd,
    config_dir,
    session_id,
    family,
    model,
    trigger,
    failure_trigger,
    pre_tokens,
    emit,
    cfg,
    session_cgroup_id,
    account_label,
    timeout_duration,
  } = run;
  let program = argv.first().context("compaction argv is empty")?;
  let Some(transcript) = claude_accounts::transcript_path(config_dir, session_id)? else {
    let error = format!("no transcript for {session_id} under {}", config_dir.display());
    return fail(emit, &error, session_id, account_label, model, failure_trigger).await;
  };
  let before = count_compact_boundaries(&transcript)?;
  let baseline = newest_cost_state_model_usage(&transcript)?;
  tracing::info!(target: "charliebot_agents", cc_session_id = session_id, pre_tokens, session = %session_id, account = account_label, trigger, "claude_compaction_starting");
  let spec = SpawnSpec::new(
    program,
    argv.iter().skip(1).map(OsString::from).collect(),
    Some(cwd.to_path_buf()),
    env.into_iter().map(|(name, value)| (OsString::from(name), OsString::from(value))).collect::<HashMap<_, _>>(),
    session_cgroup_id.map(str::to_string),
    SessionCgroupConfig::from_config(cfg),
  );
  let mut child = match spawn_with_setup(&spec, None, ChildStdin::Piped, SpawnSetup::ChildSideCgroup).await {
    Ok(spawned) => spawned.child,
    Err(error) => {
      let error = format!("could not start {program}: {error}");
      return fail(emit, &error, session_id, account_label, model, failure_trigger).await;
    }
  };
  let pid = child.id().expect("spawned compaction process has a pid");
  let mut stdin = child.stdin.take().expect("compaction stdin is piped");
  stdin.write_all(COMPACT_PROMPT).await?;
  drop(stdin);
  let mut stdout = child.stdout.take().expect("compaction stdout is piped");
  let mut stderr = child.stderr.take().expect("compaction stderr is piped");
  let stdout_reader = tokio::spawn(async move {
    let mut output = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut output).await?;
    Ok::<_, std::io::Error>(output)
  });
  let stderr_reader = tokio::spawn(async move {
    let mut output = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut output).await?;
    Ok::<_, std::io::Error>(output)
  });
  let status = match timeout(timeout_duration, child.wait()).await {
    Ok(result) => result?,
    Err(_) => {
      kill::kill_process_group(pid, nix::sys::signal::Signal::SIGKILL);
      child.wait().await?;
      stdout_reader.await.context("joining timed-out compaction stdout reader")??;
      stderr_reader.await.context("joining timed-out compaction stderr reader")??;
      let error = format!("timed out after {} s", timeout_duration.as_secs());
      return fail(emit, &error, session_id, account_label, model, failure_trigger).await;
    }
  };
  let stdout = stdout_reader.await.context("joining compaction stdout reader")??;
  let stderr = stderr_reader.await.context("joining compaction stderr reader")??;
  let after = count_compact_boundaries(&transcript)?;
  let mut outcome = judge(kill::python_exit_code(status), &stdout, &baseline, before, after, family);
  if !outcome.ok {
    let stderr = String::from_utf8_lossy(&stderr);
    let stderr = stderr.trim();
    let stderr_tail = stderr.chars().rev().take(300).collect::<String>().chars().rev().collect::<String>();
    let error = outcome.error.as_deref().expect("failed compaction has an error");
    tracing::warn!(
      target: "charliebot_agents",
      cc_session_id = session_id,
      error,
      models = ?outcome.models,
      stderr = %stderr_tail,
      session = %session_id,
      account = account_label,
      trigger,
      "claude_compaction_failed"
    );
    emit(crate::event_builders::make_context_compact_failed_event(Some(error), Some(model), failure_trigger)).await?;
    return Ok(outcome);
  }
  let mut metadata = newest_boundary_compact_metadata(&transcript)?;
  if let Some(pre_tokens) = pre_tokens {
    let object =
      metadata.get_or_insert_with(|| Value::Object(Map::new())).as_object_mut().expect("compact metadata is an object");
    object.insert("pre_tokens".to_string(), json!(pre_tokens));
  }
  outcome.post_tokens = metadata.as_ref().and_then(|metadata| metadata.get("post_tokens")).and_then(Value::as_i64);
  tracing::info!(
    target: "charliebot_agents",
    cc_session_id = session_id,
    pre_tokens = ?metadata.as_ref().and_then(|value| value.get("pre_tokens")),
    post_tokens = ?outcome.post_tokens,
    cache_read_tokens = ?outcome.cache_read_tokens,
    models = ?outcome.models,
    session = %session_id,
    account = account_label,
    trigger,
    "claude_compaction_done"
  );
  emit(crate::event_builders::make_context_compacted_event(trigger, metadata, Some(model))).await?;
  Ok(outcome)
}

async fn fail(
  emit: &EmitFn,
  error: &str,
  session_id: &str,
  account_label: &str,
  model: &str,
  trigger: Option<&str>,
) -> anyhow::Result<CompactionOutcome> {
  tracing::warn!(target: "charliebot_agents", error, session = %session_id, account = account_label, trigger, "claude_compaction_failed");
  emit(crate::event_builders::make_context_compact_failed_event(Some(error), Some(model), trigger)).await?;
  Ok(CompactionOutcome {
    ok: false,
    error: Some(error.to_string()),
    models: Vec::new(),
    post_tokens: None,
    cache_read_tokens: None,
  })
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::os::unix::fs::symlink;
  use std::path::PathBuf;
  use std::sync::{Arc, Mutex};

  use charliebot_registry::Registry;

  use super::*;

  // Python: tests/test_claude_compaction.py:152.
  #[test]
  fn compaction_command_disables_skills_and_installs_the_compaction_context_hook() {
    let command = compaction_command("claude-session", "charlie-session");
    let disallowed_index = command.iter().position(|value| *value == DISALLOWED_TOOLS_FLAG).unwrap();
    assert_eq!(command[disallowed_index + 1], format!("{HEADLESS_DISALLOWED_TOOLS},{EXTRA_COMPACTION_TOOLS}"));
    assert!(command[disallowed_index + 1].split(',').any(|tool| tool == "Skill"));
    assert_eq!(command.iter().filter(|value| **value == "--settings").count(), 1);
    let settings = command[command.iter().position(|value| value == "--settings").unwrap() + 1].clone();
    assert_eq!(
      settings,
      concat!(
        r#"{"disableClaudeAiConnectors":true,"hooks":{"SessionStart":[{"matcher":"compact","hooks":["#,
        r#"{"type":"command","command":"charliebot compaction-context --session charlie-session --slot 1"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session charlie-session --slot 2"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session charlie-session --slot 3"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session charlie-session --slot 4"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session charlie-session --slot 5"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session charlie-session --slot 6"}]}]}}"#,
      )
    );
    let settings: Value = serde_json::from_str(&settings).unwrap();
    assert_eq!(settings["disableClaudeAiConnectors"], json!(true));
    let [session_start] = settings["hooks"]["SessionStart"].as_array().unwrap().as_slice() else {
      panic!("one SessionStart entry");
    };
    assert_eq!(session_start["matcher"], "compact");
    let commands: Vec<&str> =
      session_start["hooks"].as_array().unwrap().iter().map(|hook| hook["command"].as_str().unwrap()).collect();
    let expected: Vec<String> =
      (1..=6).map(|slot| format!("charliebot compaction-context --session charlie-session --slot {slot}")).collect();
    assert_eq!(commands, expected);
  }

  fn config(home: &Path) -> CharlieBotConfig {
    fs::create_dir_all(home).unwrap();
    fs::write(home.join("config.yaml"), "server:\n  session_memory_max_mb: 0\n").unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", crate::backends::claude_code::register).unwrap();
    registry.load_config(home).unwrap()
  }

  fn fake_env(bin: &Path, home: &Path, config_dir: &Path) -> Env {
    let inherited = Env::from([
      ("HOME".to_string(), home.to_string_lossy().into_owned()),
      ("PATH".to_string(), format!("{}:/usr/bin:/bin", bin.display())),
      ("CLAUDECODE".to_string(), "nested".to_string()),
      ("CLAUDE_CONFIG_DIR".to_string(), "/synthetic/inherited".to_string()),
      (crate::launch_loop::SESSION_ID_ENV.to_string(), "parent-session".to_string()),
    ]);
    let env = compaction_env_from(config_dir, &inherited, RunKind::ManagerTurn).unwrap();
    assert!(!env.contains_key("CLAUDECODE"));
    assert!(!env.contains_key(crate::launch_loop::SESSION_ID_ENV));
    assert_eq!(env.get("CLAUDE_CONFIG_DIR").unwrap(), &config_dir.to_string_lossy());
    assert_eq!(env.get("CLAUDE_CODE_DISABLE_AUTO_MEMORY").unwrap(), "1");
    assert!(inherited.contains_key("CLAUDECODE"));
    env
  }

  /// Stages the fake `claude` for one run: it records its argv and stdin under `root`, then plays the steps of `mode`.
  fn stage_claude(bin: &Path, root: &Path, mode: Vec<Value>) {
    let mut steps =
      vec![json!({"record_argv": root.join("arguments.txt")}), json!({"record_stdin": root.join("prompt.txt")})];
    steps.extend(mode);
    fs::write(bin.join("probe.json"), json!({"claude": steps}).to_string()).unwrap();
  }

  /// The fake-program helper that cargo builds beside this test binary. Unit tests cannot name it with
  /// `CARGO_BIN_EXE_*`, so the path comes from the target directory.
  fn fake_program_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary path");
    exe
      .ancestors()
      .skip(1)
      .map(|directory| directory.join("fake_program"))
      .find(|candidate| candidate.is_file())
      .expect("cargo builds the fake-program helper into the target directory that holds the test binary")
  }

  fn emit_sink() -> (EmitFn, Arc<Mutex<Vec<Value>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    (
      Arc::new(move |event| {
        let sink = Arc::clone(&sink);
        Box::pin(async move {
          sink.lock().unwrap().push(event);
          Ok(())
        })
      }),
      events,
    )
  }

  fn call<'a>(
    session_id: &'a str,
    cwd: &'a Path,
    config_dir: &'a Path,
    emit: &'a EmitFn,
    cfg: &'a CharlieBotConfig,
    timeout_duration: StdDuration,
  ) -> CompactionInput<'a> {
    CompactionInput {
      session_id,
      cwd,
      config_dir,
      pre_tokens: Some(125_000),
      emit,
      cfg,
      session_cgroup_id: "synthetic-session",
      account_label: "synthetic-account",
      trigger: "expired_cache",
      timeout_duration,
    }
  }

  #[tokio::test]
  async fn fake_claude_compaction_checks_env_argv_success_failure_and_timeout_notices() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let cfg = config(&home);
    let account = root.path().join("account");
    let bin = root.path().join("bin");
    let cwd = root.path().join("work");
    let transcript = account.join("projects/work/session-id.jsonl");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    fs::write(&transcript, "{\"type\":\"user\",\"message\":{}}\n").unwrap();
    symlink(fake_program_bin(), bin.join("claude")).unwrap();
    let (emit, events) = emit_sink();
    let inherited = fake_env(&bin, &home, &account);

    stage_claude(
      &bin,
      root.path(),
      vec![
        json!({"append": {"path": transcript, "text": "{\"type\":\"system\",\"subtype\":\"compact_boundary\"}\n"}}),
        json!({"stdout": "{\"is_error\":false,\"modelUsage\":{\"claude-sonnet-5\":{\"inputTokens\":1}}}\n"}),
        json!({"exit": 0}),
      ],
    );
    let success_env = inherited.clone();
    let succeeded = compact_with_sonnet_inner(
      call("session-id", &cwd, &account, &emit, &cfg, StdDuration::from_secs(30)),
      move |path| compaction_env_from(path, &success_env, RunKind::ManagerTurn),
    )
    .await
    .unwrap();
    assert!(succeeded);
    assert_eq!(fs::read_to_string(root.path().join("prompt.txt")).unwrap(), "/compact\n");
    let args = fs::read_to_string(root.path().join("arguments.txt")).unwrap();
    assert!(args.contains("--resume\nsession-id\n--model\nclaude-sonnet-5"));
    assert!(args
      .contains(concat!(
        "--settings\n{\"disableClaudeAiConnectors\":true,\"hooks\":{\"SessionStart\":[{\"matcher\":\"compact\",\"hooks\":[",
        "{\"type\":\"command\",\"command\":\"charliebot compaction-context --session synthetic-session --slot 1\"},",
        "{\"type\":\"command\",\"command\":\"charliebot compaction-context --session synthetic-session --slot 2\"},",
        "{\"type\":\"command\",\"command\":\"charliebot compaction-context --session synthetic-session --slot 3\"},",
        "{\"type\":\"command\",\"command\":\"charliebot compaction-context --session synthetic-session --slot 4\"},",
        "{\"type\":\"command\",\"command\":\"charliebot compaction-context --session synthetic-session --slot 5\"},",
        "{\"type\":\"command\",\"command\":\"charliebot compaction-context --session synthetic-session --slot 6\"}]}]}}"
      )));
    assert_eq!(events.lock().unwrap()[0]["type"], charliebot_core::event_types::CONTEXT_COMPACTED);
    assert_eq!(events.lock().unwrap()[0]["compact_metadata"]["pre_tokens"], 125_000);

    fs::write(&transcript, "{\"type\":\"user\",\"message\":{}}\n").unwrap();
    stage_claude(
      &bin,
      root.path(),
      vec![json!({"stdout": "{\"is_error\":true,\"result\":\"synthetic compaction failure\"}\n"}), json!({"exit": 7})],
    );
    let failure_env = inherited.clone();
    let failed = compact_with_sonnet_inner(
      call("session-id", &cwd, &account, &emit, &cfg, StdDuration::from_secs(30)),
      move |path| compaction_env_from(path, &failure_env, RunKind::ManagerTurn),
    )
    .await
    .unwrap();
    assert!(!failed);
    assert_eq!(events.lock().unwrap()[1]["type"], charliebot_core::event_types::CONTEXT_COMPACT_FAILED);

    fs::write(&transcript, "{\"type\":\"user\",\"message\":{}}\n").unwrap();
    stage_claude(&bin, root.path(), vec![json!({"sleep_ms": 5000}), json!({"exit": 0})]);
    let timeout_env = inherited;
    let timed_out = compact_with_sonnet_inner(
      call("session-id", &cwd, &account, &emit, &cfg, StdDuration::from_millis(100)),
      move |path| compaction_env_from(path, &timeout_env, RunKind::ManagerTurn),
    )
    .await
    .unwrap();
    assert!(!timed_out);
    assert_eq!(events.lock().unwrap()[2]["type"], charliebot_core::event_types::CONTEXT_COMPACT_FAILED);
  }

  // Python: tests/test_claude_idle_compaction.py:309.
  #[tokio::test]
  async fn generic_runner_judges_the_last_stream_json_result_and_returns_post_readings() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let cfg = config(&home);
    let account = root.path().join("account");
    let bin = root.path().join("bin");
    let cwd = root.path().join("work");
    let transcript = account.join("projects/work/session-id.jsonl");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    fs::write(
      &transcript,
      "{\"type\":\"cost-state\",\"modelUsage\":{\"claude-fable-5-1\":{\"inputTokens\":50,\"cacheReadInputTokens\":10}}}\n",
    )
    .unwrap();
    symlink(fake_program_bin(), bin.join("claude")).unwrap();
    let (emit, events) = emit_sink();
    let env = Env::from([
      ("HOME".to_string(), home.to_string_lossy().into_owned()),
      ("PATH".to_string(), format!("{}:/usr/bin:/bin", bin.display())),
    ]);
    stage_claude(
      &bin,
      root.path(),
      vec![
        json!({
          "append": {
            "path": transcript,
            "text": "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"compactMetadata\":{\"postTokens\":9111}}\n"
          }
        }),
        json!({
          "stdout": "{\"type\":\"assistant\",\"message\":{}}\n{\"type\":\"result\",\"is_error\":false,\"modelUsage\":{\"claude-fable-5-1\":{\"inputTokens\":51,\"cacheReadInputTokens\":128}}}\n"
        }),
        json!({"exit": 0}),
      ],
    );

    let outcome = run_compaction(CompactionRun {
      argv: vec!["claude".to_string(), "-p".to_string(), "--output-format".to_string(), "stream-json".to_string()],
      env,
      cwd: &cwd,
      config_dir: &account,
      session_id: "session-id",
      family: "fable",
      model: "claude-fable-5-1",
      trigger: "idle",
      failure_trigger: Some("idle"),
      pre_tokens: Some(120_000),
      emit: &emit,
      cfg: &cfg,
      session_cgroup_id: Some("synthetic-session"),
      account_label: "acct-a",
      timeout_duration: StdDuration::from_secs(30),
    })
    .await
    .unwrap();

    assert!(outcome.ok);
    assert_eq!(outcome.post_tokens, Some(9_111));
    assert_eq!(outcome.cache_read_tokens, Some(118));
    assert_eq!(events.lock().unwrap()[0]["trigger"], "idle");
    assert_eq!(events.lock().unwrap()[0]["compact_metadata"]["pre_tokens"], 120_000);
  }

  // Python: src/backends/claude_code/claude_compaction.py:273.
  #[test]
  fn a_failed_run_whose_result_is_blank_reports_the_fallback_text() {
    let outcome = judge(1, br#"{"is_error":true,"result":"   "}"#, &Map::new(), 0, 0, "fable");

    assert_eq!(outcome.error.as_deref(), Some("exit 1: run reported an error"));
  }
}
