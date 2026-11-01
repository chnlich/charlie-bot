//! Claude CLI command settings and headless environment names.

use charliebot_registry::backend::Env;
use charliebot_store::run_records::RunKind;
use serde_json::{Map, Value, json};

pub const AUTO_COMPACT_WINDOW_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
pub const AUTOCOMPACT_PCT_OVERRIDE_ENV: &str = "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE";
pub const MAX_CONTEXT_TOKENS_ENV: &str = "CLAUDE_CODE_MAX_CONTEXT_TOKENS";
pub const THINKING_DISPLAY_UPDATES_ENV: &str = "CLAUDE_CODE_THINKING_DISPLAY_UPDATES";
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
pub const CLAUDE_CODE_OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
pub const SIMPLE_SYSTEM_PROMPT_ENV: &str = "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT";
pub const AUTO_COMPACT_DEFAULT: u64 = 433_000;
pub const COMPACT_OUTPUT_RESERVE: u64 = 20_000;
pub const COMPACT_CONTEXT_RESERVE: u64 = 13_000;
pub const DISABLE_CONNECTOR_SETTINGS_KEY: &str = "disableClaudeAiConnectors";
pub const SYNC_CLAUDE_AI_SKILLS_SETTINGS_KEY: &str = "syncClaudeAiSkills";
pub const DISABLE_BUNDLED_SKILLS_SETTINGS_KEY: &str = "disableBundledSkills";
pub const FAST_MODE_SETTINGS_KEY: &str = "fastMode";

/// The PreToolUse hook event: the backend registers `charliebot wait-guard` under it, and the CLI's answer must name it.
pub const PRE_TOOL_USE_HOOK_EVENT: &str = "PreToolUse";
/// The SessionStart hook event: the backend registers `charliebot compaction-context` under it, and the CLI's answer
/// names it.
pub const SESSION_START_HOOK_EVENT: &str = "SessionStart";

pub const HEADLESS_FORWARDED_ENV: [&str; 4] =
  [AUTOCOMPACT_PCT_OVERRIDE_ENV, MAX_CONTEXT_TOKENS_ENV, AUTO_COMPACT_WINDOW_ENV, SIMPLE_SYSTEM_PROMPT_ENV];

/// The declared Claude window for a run kind at the Python baseline.
pub fn declared_window_by_run_kind(run_kind: RunKind) -> u64 {
  match run_kind {
    RunKind::ManagerTurn | RunKind::Work | RunKind::Review | RunKind::Iteration | RunKind::ScheduledStep => {
      AUTO_COMPACT_DEFAULT
    }
  }
}

/// Quote *value* the way Python's `shlex.quote` does, for the hook command line.
fn shlex_quote(value: &str) -> String {
  if value.is_empty() {
    return "''".to_string();
  }
  let safe = |character: char| character.is_ascii_alphanumeric() || character == '_' || "@%+=:,./-".contains(character);
  if value.chars().all(safe) {
    return value.to_string();
  }
  format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// The SessionStart hook that restores a manager session's read skills after compaction.
pub fn compaction_context_hook_settings(session_id: &str) -> Map<String, Value> {
  let hooks: Vec<Value> = (1..=6)
    .map(|slot| {
      json!({
        "type": "command",
        "command": format!("charliebot compaction-context --session {} --slot {slot}", shlex_quote(session_id)),
      })
    })
    .collect();
  let mut settings = Map::new();
  settings.insert("hooks".to_string(), json!({SESSION_START_HOOK_EVENT: [{"matcher": "compact", "hooks": hooks}]}));
  settings
}

fn add_wait_guard_hook(settings: &mut Map<String, Value>) {
  let hooks = settings.entry("hooks".to_string()).or_insert_with(|| Value::Object(Map::new()));
  hooks.as_object_mut().expect("Claude hook settings are an object").insert(
    PRE_TOOL_USE_HOOK_EVENT.to_string(),
    json!([{
      "matcher": "Bash",
      "hooks": [{"type": "command", "command": "charliebot wait-guard"}]
    }]),
  );
}

/// The one settings object every headless Claude process receives for its run kind.
pub fn headless_claude_settings(run_kind: RunKind, session_id: Option<&str>) -> anyhow::Result<Map<String, Value>> {
  headless_claude_settings_for(run_kind, session_id, true)
}

/// Headless settings for compaction, which keeps its existing hook set.
pub fn headless_claude_settings_without_wait_guard(
  run_kind: RunKind,
  session_id: Option<&str>,
) -> anyhow::Result<Map<String, Value>> {
  headless_claude_settings_for(run_kind, session_id, false)
}

fn headless_claude_settings_for(
  run_kind: RunKind,
  session_id: Option<&str>,
  use_wait_guard: bool,
) -> anyhow::Result<Map<String, Value>> {
  let mut settings = Map::new();
  settings.insert(DISABLE_CONNECTOR_SETTINGS_KEY.to_string(), Value::Bool(true));
  settings.insert(SYNC_CLAUDE_AI_SKILLS_SETTINGS_KEY.to_string(), Value::Bool(false));
  match run_kind {
    RunKind::ManagerTurn => {
      let Some(session_id) = session_id else {
        anyhow::bail!("manager_turn Claude settings require a CharlieBot session id");
      };
      settings.extend(compaction_context_hook_settings(session_id));
    }
    RunKind::Work | RunKind::Review | RunKind::Iteration | RunKind::ScheduledStep => {
      settings.insert(DISABLE_BUNDLED_SKILLS_SETTINGS_KEY.to_string(), Value::Bool(true));
    }
  }
  if use_wait_guard {
    match run_kind {
      RunKind::ManagerTurn | RunKind::Work | RunKind::Review | RunKind::Iteration => add_wait_guard_hook(&mut settings),
      RunKind::ScheduledStep => {}
    }
  }
  Ok(settings)
}

/// Build the Claude headless environment from the server snapshot and run kind.
pub fn headless_claude_env(run_kind: RunKind, source: &Env) -> Env {
  let mut result = Env::from([
    ("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS".to_string(), "1".to_string()),
    ("BASH_DEFAULT_TIMEOUT_MS".to_string(), "900000".to_string()),
    ("BASH_MAX_TIMEOUT_MS".to_string(), "900000".to_string()),
    ("CLAUDE_CODE_DISABLE_ADVISOR_TOOL".to_string(), "1".to_string()),
    (AUTO_COMPACT_WINDOW_ENV.to_string(), declared_window_by_run_kind(run_kind).to_string()),
  ]);
  for name in HEADLESS_FORWARDED_ENV {
    if let Some(value) = source.get(name) {
      result.insert(name.to_string(), value.clone());
    }
  }
  result
}
