//! Error hints and re-evaluable liveness reads used by live and recovered turns.

use std::sync::OnceLock;

use regex::Regex;
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

pub fn select_error_hint(error_messages: &[String], stderr_text: &str) -> Option<String> {
  if let Some(message) = error_messages.iter().rev().find(|message| !message.trim().is_empty()) {
    return Some(message.clone());
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
  use super::*;

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
