//! Event decisions and transcript movement for Claude account relays.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types::{RATE_LIMIT_EVENT, RATE_LIMIT_INFO};
use charliebot_registry::backend::LaunchWatch as LaunchWatchTrait;
use charliebot_store::metadata::UtcDateTime;
use serde_json::{Value, json};
use time::Duration;

use super::{claude_accounts, claude_config, event_types};

pub const MAX_RELAYS_PER_TURN: usize = 3;
pub const RELAY_MIN_RESET_AHEAD: Duration = Duration::minutes(45);
pub const CONTINUATION_PROMPT: &str = "The previous process was interrupted right after a tool call completed. Continue from where it stopped; do not redo the steps that are already done.";
pub const AUTH_FAILURE_MARKER: &str = "Failed to authenticate";
pub const RELAY_REJECTED: &str = "rejected";
pub const RELAY_WARNING: &str = "warning";
pub const LOGIN_FAILED: &str = "login_failed";
pub const LOGIN_REASON_AUTH_FAILED: &str = "auth_failed";
pub const LOGIN_REASON_EMPTY_CREDENTIALS: &str = "empty_credentials";
pub const POOL_EXHAUSTED_PHRASE: &str = "Claude account pool has no available account";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
  Rejected,
  Warning,
  LoginFailed,
}

impl Decision {
  fn as_str(self) -> &'static str {
    match self {
      Self::Rejected => RELAY_REJECTED,
      Self::Warning => RELAY_WARNING,
      Self::LoginFailed => LOGIN_FAILED,
    }
  }
}

/// Per-process quota and authentication watch.
pub struct RelayWatch {
  label: String,
  model: Option<String>,
  cfg: Arc<CharlieBotConfig>,
  state: Arc<claude_accounts::ClaudeAccountState>,
  account_pool: Option<String>,
  now: Option<UtcDateTime>,
  reason: Option<Decision>,
  armed: bool,
  auth_failed: bool,
  decision: Arc<Mutex<Option<String>>>,
}

impl RelayWatch {
  pub fn new(
    label: String,
    model: Option<String>,
    cfg: Arc<CharlieBotConfig>,
    state: Arc<claude_accounts::ClaudeAccountState>,
    account_pool: Option<String>,
    decision: Arc<Mutex<Option<String>>>,
  ) -> Self {
    Self { label, model, cfg, state, account_pool, now: None, reason: None, armed: false, auth_failed: false, decision }
  }

  fn decision(&self, exit_code: i32, stderr: &str) -> Option<Decision> {
    if exit_code != 0 && (self.auth_failed || stderr.contains(AUTH_FAILURE_MARKER)) {
      Some(Decision::LoginFailed)
    } else {
      self.reason
    }
  }
}

impl LaunchWatchTrait for RelayWatch {
  fn observe(&mut self, event: &Value) -> bool {
    match event.get("type").and_then(Value::as_str) {
      Some(RATE_LIMIT_EVENT) => {
        let Some(info) = event.get(RATE_LIMIT_INFO).filter(|value| value.is_object()) else {
          return false;
        };
        let moment = claude_accounts::now_or(self.now);
        let reading = claude_accounts::observe_rate_limit(&self.state, &self.label, info, Some(moment));
        match info.get("status").and_then(Value::as_str) {
          Some("rejected") => {
            self.reason = Some(Decision::Rejected);
            self.armed = false;
          }
          Some("allowed_warning")
            if reading.is_some_and(|reading| reading.utilization >= claude_accounts::WARNING_UTILIZATION)
              && reset_far_enough(info, moment) =>
          {
            self.armed = true;
          }
          _ => {}
        }
        false
      }
      Some("assistant") if assistant_texts(event).iter().any(|text| text.contains(AUTH_FAILURE_MARKER)) => {
        self.auth_failed = true;
        false
      }
      _ if self.armed && self.reason.is_none() && is_tool_result_event(event) => {
        let moment = claude_accounts::now_or(self.now);
        let destination = claude_accounts::cool_destination(
          &self.state,
          &self.cfg,
          self.model.as_deref(),
          self.account_pool.as_deref(),
          Some(moment),
        )
        .expect("validated Claude account pools remain readable during a process");
        if destination.as_ref().is_none_or(|account| account.label == self.label) {
          return false;
        }
        self.reason = Some(Decision::Warning);
        true
      }
      _ => false,
    }
  }

  fn wants_next(&self, exit_code: i32, stderr: &str) -> bool {
    let decision = self.decision(exit_code, stderr).map(Decision::as_str).map(str::to_string);
    *self.decision.lock().expect("Claude relay decision is not poisoned") = decision.clone();
    decision.is_some()
  }
}

fn is_tool_result_event(event: &Value) -> bool {
  event.get("type").and_then(Value::as_str) == Some("user")
    && event
      .pointer("/message/content")
      .and_then(Value::as_array)
      .is_some_and(|blocks| blocks.iter().any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result")))
}

fn assistant_texts(event: &Value) -> Vec<&str> {
  event
    .pointer("/message/content")
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|block| block.get("text").and_then(Value::as_str))
    .collect()
}

fn reset_far_enough(info: &Value, now: UtcDateTime) -> bool {
  let Some(seconds) = info.get("resetsAt").and_then(Value::as_f64) else {
    return true;
  };
  let reset = timestamp(seconds);
  reset.into_inner() - now.into_inner() > RELAY_MIN_RESET_AHEAD
}

fn timestamp(value: f64) -> UtcDateTime {
  let seconds = value.floor() as i64;
  let nanos = ((value - seconds as f64) * 1_000_000_000.0) as i64;
  let date = time::OffsetDateTime::from_unix_timestamp(seconds).expect("rate-limit reset timestamp is in range")
    + Duration::nanoseconds(nanos);
  let text = date.format(&time::format_description::well_known::Rfc3339).expect("timestamp formats as RFC3339");
  UtcDateTime::parse(&text).expect("RFC3339 timestamp parses")
}

fn pool_detail(
  state: &claude_accounts::ClaudeAccountState,
  cfg: &CharlieBotConfig,
  now: Option<UtcDateTime>,
  account_pool: Option<&str>,
) -> anyhow::Result<String> {
  let detail = if let Some(reset) = claude_accounts::earliest_reset(state, cfg, now, account_pool)? {
    format!("earliest reset {:02}:{:02} UTC", reset.into_inner().hour(), reset.into_inner().minute())
  } else {
    claude_accounts::unavailable_detail(state, cfg, now, account_pool)?
  };
  let detail = if detail.is_empty() { "no unavailability recorded" } else { &detail };
  Ok(account_pool.map_or_else(|| detail.to_string(), |pool| format!("pool '{pool}'; {detail}")))
}

pub fn pool_exhausted_message(
  state: &claude_accounts::ClaudeAccountState,
  cfg: &CharlieBotConfig,
  now: Option<UtcDateTime>,
  account_pool: Option<&str>,
) -> anyhow::Result<String> {
  Ok(format!("{POOL_EXHAUSTED_PHRASE} ({}); this run did not complete.", pool_detail(state, cfg, now, account_pool)?))
}

pub fn relay_limit_message(
  state: &claude_accounts::ClaudeAccountState,
  cfg: &CharlieBotConfig,
  account_pool: Option<&str>,
) -> anyhow::Result<String> {
  Ok(format!(
    "Claude account relay limit reached ({MAX_RELAYS_PER_TURN} relays in one run; {}); this run did not complete.",
    pool_detail(state, cfg, None, account_pool)?
  ))
}

pub fn login_required_event(account: &claude_config::ClaudeAccount, reason: &str) -> Value {
  json!({
    "type": event_types::CLAUDE_ACCOUNT_LOGIN_REQUIRED,
    "account": account.label,
    "config_dir": account.config_dir,
    "reason": reason
  })
}

pub struct RelayMove {
  pub account: Option<claude_config::ClaudeAccount>,
  pub error: Option<String>,
  pub refused_holder: Option<claude_config::ClaudeAccount>,
  pub pool_exhausted: bool,
}

pub fn relay_move(
  state: &ClaudeAccountStateRef,
  cfg: &CharlieBotConfig,
  model: Option<&str>,
  current: &claude_config::ClaudeAccount,
  session_id: Option<&str>,
  now: Option<UtcDateTime>,
  account_pool: Option<&str>,
) -> anyhow::Result<RelayMove> {
  let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
    return Ok(RelayMove {
      account: None,
      error: Some("Claude account relay impossible: the run produced no session id to resume.".to_string()),
      refused_holder: None,
      pool_exhausted: false,
    });
  };
  let exclude = HashSet::from([current.label.clone()]);
  let next = claude_accounts::select(state, cfg, model, &exclude, None, now, account_pool, 0.0)?;
  let Some(next) = next else {
    return Ok(RelayMove {
      account: None,
      error: Some(pool_exhausted_message(state, cfg, now, account_pool)?),
      refused_holder: None,
      pool_exhausted: true,
    });
  };
  match claude_accounts::move_transcript(session_id, Path::new(&current.config_dir), Path::new(&next.config_dir)) {
    Ok(_) => Ok(RelayMove { account: Some(next), error: None, refused_holder: None, pool_exhausted: false }),
    Err(error) => {
      let refused = claude_accounts::is_newer_transcript_refusal(&error);
      Ok(RelayMove {
        account: None,
        error: Some(format!("Claude account relay failed: {error}")),
        refused_holder: refused.then_some(next),
        pool_exhausted: false,
      })
    }
  }
}

pub type ClaudeAccountStateRef = Arc<claude_accounts::ClaudeAccountState>;

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use charliebot_core::config::CharlieBotConfig;
  use charliebot_registry::Registry;
  use charliebot_store::metadata::UtcDateTime;

  use crate::login_dirs;

  use super::*;

  fn config(home: &std::path::Path) -> Arc<CharlieBotConfig> {
    std::fs::write(
      home.join("config.yaml"),
      "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\naccounts:\n  claude: []\n",
    )
    .unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", crate::register).unwrap();
    Arc::new(registry.load_config(home).unwrap())
  }

  #[test]
  fn watch_relays_a_rejection_after_process_exit() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(home.path());
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    let decision = Arc::new(Mutex::new(None));
    let mut watch = RelayWatch::new(
      "synthetic".to_string(),
      Some("claude-fable-5-1".to_string()),
      cfg,
      state,
      None,
      decision.clone(),
    );
    assert!(!watch.observe(&json!({
      "type":"rate_limit_event", "rate_limit_info":{"status":"rejected"}
    })));
    assert!(watch.wants_next(1, ""));
    assert_eq!(decision.lock().unwrap().as_deref(), Some(RELAY_REJECTED));
    assert!(watch.wants_next(1, "Failed to authenticate"));
    assert_eq!(decision.lock().unwrap().as_deref(), Some(LOGIN_FAILED));
    let _ = UtcDateTime::now();
  }

  fn pooled_config(home: &std::path::Path, accounts: &[(&str, &std::path::Path)]) -> CharlieBotConfig {
    let mut rows = String::new();
    let mut labels = Vec::new();
    for (label, directory) in accounts {
      rows.push_str(&format!("    - label: {label}\n      config_dir: '{}'\n", directory.display()));
      labels.push(*label);
    }
    std::fs::write(
      home.join("config.yaml"),
      format!(
        "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n      account_pool: alpha\naccounts:\n  claude:\n{rows}  claude_pools:\n    alpha: [{}]\n",
        labels.join(", ")
      ),
    )
    .unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", crate::register).unwrap();
    registry.load_config(home).unwrap()
  }

  #[test]
  fn relay_move_lands_under_the_line_before_the_tie_rule_applies() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let main = root.path().join("main");
    let ext_1 = root.path().join("ext-1");
    let ext_2 = root.path().join("ext-2");
    for directory in [&main, &ext_1, &ext_2] {
      std::fs::create_dir_all(directory).unwrap();
      std::fs::write(directory.join(login_dirs::CREDENTIALS_FILE), r#"{"claudeAiOauth":{"accessToken":"synthetic"}}"#)
        .unwrap();
    }
    std::fs::create_dir_all(main.join("projects/work")).unwrap();
    std::fs::write(main.join("projects/work/uuid-1.jsonl"), "synthetic transcript\n").unwrap();
    let cfg = pooled_config(&home, &[("main", &main), ("ext-1", &ext_1), ("ext-2", &ext_2)]);
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    let now = UtcDateTime::parse("2025-01-01T12:00:00Z").unwrap();
    let older = UtcDateTime::parse("2025-01-01T11:00:00Z").unwrap();
    let window = |utilization: f64| json!({"unifiedWindows":{"five_hour":{"utilization":utilization}}});
    claude_accounts::observe_rate_limit(&state, "ext-1", &window(0.89), Some(now));
    claude_accounts::observe_rate_limit(&state, "ext-2", &window(0.905), Some(older));
    let current = claude_accounts::account_by_label(&cfg, Some("main")).unwrap().unwrap();

    let movement =
      relay_move(&state, &cfg, Some("claude-fable-5-1"), &current, Some("uuid-1"), Some(now), None).unwrap();

    assert_eq!(movement.account.map(|account| account.label), Some("ext-1".to_string()));
  }
}
