//! Error hints and re-evaluable liveness reads used by live and recovered turns.

use std::sync::OnceLock;

use charliebot_core::event_types;
use regex::Regex;
use serde_json::Value;
use time::OffsetDateTime;

use crate::transport::identity;

const STDERR_HINT_MAX_CHARS: usize = 500;

fn ansi_escape() -> &'static Regex {
  static ANSI_ESCAPE: OnceLock<Regex> = OnceLock::new();
  ANSI_ESCAPE.get_or_init(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").unwrap())
}

fn control_character() -> &'static Regex {
  static CONTROL_CHARACTER: OnceLock<Regex> = OnceLock::new();
  CONTROL_CHARACTER.get_or_init(|| Regex::new(r"[\x00-\x08\x0b-\x1f\x7f-\x9f]").unwrap())
}

pub fn clean_control_characters(text: &str) -> String {
  control_character().replace_all(&ansi_escape().replace_all(text, ""), "").into_owned()
}

/// The failure message an event carries. An `error` event carries its `message`; a Claude Code `result` record carries
/// its `result` text when `is_error` is true, which is where a rejected request names its cause. An empty text carries
/// no failure message.
pub fn failure_message(event: &Value) -> Option<String> {
  let text = match event.get("type").and_then(Value::as_str)? {
    event_types::ERROR => event.get("message").and_then(Value::as_str),
    event_types::RESULT if event.get("is_error").and_then(Value::as_bool) == Some(true) => {
      event.get("result").and_then(Value::as_str)
    }
    _ => None,
  }?;
  (!text.trim().is_empty()).then(|| text.to_string())
}

/// The failure messages a run's events carry, in event order.
pub fn failure_messages<'a>(events: impl IntoIterator<Item = &'a Value>) -> Vec<String> {
  events.into_iter().filter_map(failure_message).collect()
}

/// The most recent non-empty structured failure message, which outranks a stderr tail.
pub fn last_failure_message(messages: &[String]) -> Option<String> {
  messages.iter().rev().find(|message| !message.trim().is_empty()).cloned()
}

/// The failure reason of a run: its structured failure messages outrank the stderr tail.
pub fn select_run_error<'a>(events: impl IntoIterator<Item = &'a Value>, stderr_text: &str) -> Option<String> {
  select_error_hint(&failure_messages(events), stderr_text)
}

pub fn select_error_hint(error_messages: &[String], stderr_text: &str) -> Option<String> {
  if let Some(message) = last_failure_message(error_messages) {
    return Some(message);
  }
  let cleaned = clean_control_characters(stderr_text);
  let cleaned = cleaned.trim();
  if cleaned.is_empty() { None } else { Some(cleaned.chars().take(STDERR_HINT_MAX_CHARS).collect()) }
}

pub fn run_alive_probe(
  pid: Option<u32>,
  pid_start: Option<String>,
  started_at: Option<OffsetDateTime>,
  host_boot_time: OffsetDateTime,
) -> impl Fn() -> bool + Send + Sync + 'static {
  move || identity::is_run_alive(pid, pid_start.as_deref(), started_at, host_boot_time)
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  /// The stderr line Claude Code writes on every run of a non-Claude model name; it names no failure.
  const MODEL_WARNING: &str =
    "[claude-code:unrecognized_model] {\"model\":\"synthetic-model\",\"query_source\":\"sdk\"}\n";

  #[test]
  fn a_claude_code_error_result_outranks_the_stderr_tail() {
    let events = [json!({
      "type": "result",
      "subtype": "success",
      "is_error": true,
      "api_error_status": 402,
      "result": "API Error: 402 the upstream rejected the request",
    })];
    assert_eq!(
      select_run_error(&events, MODEL_WARNING),
      Some("API Error: 402 the upstream rejected the request".to_string())
    );
  }

  #[test]
  fn a_successful_result_leaves_the_stderr_tail_as_the_failure_reason() {
    let events = [json!({"type": "result", "subtype": "success", "is_error": false, "result": "done"})];
    assert_eq!(select_run_error(&events, "synthetic stderr"), Some("synthetic stderr".to_string()));
  }

  #[test]
  fn an_errored_result_with_empty_text_carries_no_failure_message() {
    let events = [json!({"type": "result", "subtype": "success", "is_error": true, "result": "   "})];
    assert_eq!(select_run_error(&events, "synthetic stderr"), Some("synthetic stderr".to_string()));
  }

  #[test]
  fn the_latest_structured_failure_message_wins() {
    let events = [
      json!({"type": "error", "message": "the backend went away", "content": "the backend went away"}),
      json!({"type": "result", "is_error": true, "result": "API Error: 500 the upstream failed"}),
    ];
    assert_eq!(select_run_error(&events, MODEL_WARNING), Some("API Error: 500 the upstream failed".to_string()));
  }

  #[test]
  fn structured_error_messages_outrank_cleaned_stderr() {
    assert_eq!(
      select_error_hint(&["first".to_string(), "  last  ".to_string()], "\u{1b}[31mhelp\u{1b}[0m"),
      Some("  last  ".to_string())
    );
    assert_eq!(select_error_hint(&[], "\u{1b}[31mhelp\u{1b}[0m"), Some("help".to_string()));
    assert_eq!(select_error_hint(&[], "\u{1b}[31m\u{1b}[0m"), None);
  }
}
