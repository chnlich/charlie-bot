//! The chat_threads package's turn contribution (`src/features/chat_threads/turn_contribution.py`): a thread session
//! gets the thread brief.

use charliebot_registry::turn::TurnContribution;
use charliebot_store::metadata::SessionMetadata;

use crate::thread_sessions;

/// The workflow rules file that a thread session's turn names.
const THREAD_WORKFLOW_RULES_FILE: &str = "thread_session.md";

/// Names `thread_session.md` as the workflow rules file of a thread session, and no file otherwise.
pub struct ChatThreadsTurn;

impl TurnContribution for ChatThreadsTurn {
  fn workflow_rules_file(&self, meta: &SessionMetadata) -> Option<&'static str> {
    thread_sessions::is_thread_session(meta).then_some(THREAD_WORKFLOW_RULES_FILE)
  }
}

#[cfg(test)]
mod tests {
  use charliebot_registry::turn::TurnContribution;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use serde_json::{Value, json};

  use super::ChatThreadsTurn;

  fn session(origins: &[(&str, Value)]) -> SessionMetadata {
    let mut meta = SessionMetadata::new("synthetic", "Synthetic", TaskProfile::Manager);
    for (field, value) in origins {
      meta.set_extra(*field, value.clone());
    }
    meta
  }

  #[test]
  fn a_slack_thread_session_names_the_thread_brief() {
    let meta = session(&[("slack_origin", json!({"channel_id": "C0SYNTHETIC"}))]);
    assert_eq!(ChatThreadsTurn.workflow_rules_file(&meta), Some("thread_session.md"));
  }

  #[test]
  fn a_discord_thread_session_names_the_thread_brief() {
    let meta = session(&[("discord_origin", json!({"thread_id": "900000000000000030"}))]);
    assert_eq!(ChatThreadsTurn.workflow_rules_file(&meta), Some("thread_session.md"));
  }

  #[test]
  fn a_null_origin_names_no_file() {
    let meta = session(&[("slack_origin", Value::Null), ("discord_origin", Value::Null)]);
    assert_eq!(ChatThreadsTurn.workflow_rules_file(&meta), None);
  }

  #[test]
  fn a_session_without_an_origin_names_no_file() {
    assert_eq!(ChatThreadsTurn.workflow_rules_file(&session(&[])), None);
  }
}
