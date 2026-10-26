//! Native-context reset notes (`src/runtime/session_anchors.py`).

use charliebot_store::session_fork::HISTORY_LOCATION_NOTE;

/// The instruction a fresh native conversation follows before it responds (`src/runtime/session_anchors.py:32`).
const CONTEXT_RESET_INSTRUCTION: &str = concat!(
  "Get oriented from the last two turns of that log: run ",
  "`charliebot session dialog --turns 2` before you respond. Those turns ",
  "show the actions the session just took, so you continue the work without ",
  "repeating them. A larger `--turns` value reads earlier turns. Then write ",
  "two or three sentences on where things stand and the conventions in ",
  "force. When your rules open the reply with a status block, put these ",
  "sentences after the block. Then respond to what follows. A Slack or ",
  "Discord thread post carries only the answer that ",
  "`prompts/thread_reply_format.md` defines."
);

/// The reset reason when a backend change itself forces the fresh conversation (`src/runtime/session_anchors.py:36`).
pub fn backend_switch_reset_reason(native_backend: &str, option_id: &str) -> String {
  format!("this session switched from backend {native_backend} to {option_id}, which starts its own conversation")
}

/// The bracketed note that opens a fresh native conversation's prompt (`src/runtime/session_anchors.py:46`).
pub fn context_reset_note(reason: &str, task_goal: &str) -> String {
  format!("[Context reset: {reason}. The task is: {task_goal}. {HISTORY_LOCATION_NOTE} {CONTEXT_RESET_INSTRUCTION}]")
}

#[cfg(test)]
mod tests {
  use super::{backend_switch_reset_reason, context_reset_note};

  #[test]
  fn reset_note_names_the_reason_the_task_and_the_history() {
    let note = context_reset_note("synthetic reason", "synthetic goal");
    assert!(note.starts_with(
      "[Context reset: synthetic reason. The task is: synthetic goal. Earlier turns' history remains readable"
    ));
    assert!(note.ends_with("`prompts/thread_reply_format.md` defines.]"));
  }

  /// `tests/test_task_execution.py::test_manager_native_continuation_gates_on_instruction_hash`: the reset note
  /// points the fresh context at the last two turns with `charliebot session dialog --turns 2`.
  #[test]
  fn manager_native_continuation_reset_note_reads_the_last_two_turns() {
    assert!(context_reset_note("synthetic reason", "synthetic goal").contains("charliebot session dialog --turns 2"));
  }

  #[test]
  fn backend_switch_reason_names_both_backends() {
    assert_eq!(
      backend_switch_reset_reason("synthetic-old", "synthetic-new"),
      "this session switched from backend synthetic-old to synthetic-new, which starts its own conversation"
    );
  }
}
