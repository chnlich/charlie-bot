//! Claude CLI command settings and headless environment names.

use charliebot_registry::backend::Env;

pub const AUTO_COMPACT_WINDOW_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
pub const AUTOCOMPACT_PCT_OVERRIDE_ENV: &str = "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE";
pub const MAX_CONTEXT_TOKENS_ENV: &str = "CLAUDE_CODE_MAX_CONTEXT_TOKENS";
pub const SIMPLE_SYSTEM_PROMPT_ENV: &str = "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT";
pub const AUTO_COMPACT_DEFAULT: &str = "433000";
pub const COMPACT_OUTPUT_RESERVE: u64 = 20_000;
pub const COMPACT_CONTEXT_RESERVE: u64 = 13_000;
pub const DISABLE_CONNECTOR_SETTINGS_KEY: &str = "disableClaudeAiConnectors";
pub const FAST_MODE_SETTINGS_KEY: &str = "fastMode";

pub const HEADLESS_FORWARDED_ENV: [&str; 4] =
  [AUTOCOMPACT_PCT_OVERRIDE_ENV, MAX_CONTEXT_TOKENS_ENV, AUTO_COMPACT_WINDOW_ENV, SIMPLE_SYSTEM_PROMPT_ENV];

/// Build the Claude headless environment from the server environment snapshot.
pub fn headless_claude_env(source: &Env) -> Env {
  let mut result = Env::from([
    ("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS".to_string(), "1".to_string()),
    (AUTO_COMPACT_WINDOW_ENV.to_string(), AUTO_COMPACT_DEFAULT.to_string()),
  ]);
  for name in HEADLESS_FORWARDED_ENV {
    if let Some(value) = source.get(name) {
      result.insert(name.to_string(), value.clone());
    }
  }
  result
}
