//! Native-context reset notes. A manager and a worker each get their own note.

use charliebot_store::session_fork::{HISTORY_LOCATION_NOTE, child_task_list_note};

/// The instruction a worker's fresh native conversation follows before it responds.
const WORKER_INSTRUCTION: &str = concat!(
  "Get oriented from the last two turns of that log: run ",
  "`charliebot session dialog --turns 2` before you respond. Those turns ",
  "show the actions the session just took, so you continue the work without ",
  "repeating them. A larger `--turns` value reads earlier turns. Then write ",
  "two or three sentences on where things stand and the conventions in ",
  "force. When your rules open the reply with a status block, put these ",
  "sentences after the block. Then respond to what follows. A Slack, Discord, or ",
  "Feishu thread post carries only the answer that ",
  "`prompts/thread_reply_format.md` defines."
);

/// The reset reason when a backend change itself forces the fresh conversation.
pub fn backend_switch_reset_reason(native_backend: &str, option_id: &str) -> String {
  format!("this session switched from backend {native_backend} to {option_id}, which starts its own conversation")
}

/// The manager note that opens a fresh native conversation in `session_id`'s chat log. The session's Goals sit in its
/// prompt, so the note names none.
pub fn manager_context_reset_note(session_id: &str, reason: &str) -> String {
  manager_reset_note(session_id, reason, HISTORY_LOCATION_NOTE)
}

/// The manager note of a retry that continues another run's context at `context`: the absolute paths that hold it.
pub fn manager_switch_reset_note(session_id: &str, reason: &str, context: &str) -> String {
  manager_reset_note(session_id, reason, context)
}

/// The worker note of a retry that continues another run's context at `context`: the absolute paths that hold it.
pub fn worker_switch_reset_note(reason: &str, task_goal: &str, context: &str) -> String {
  format!("[Context reset: {reason}. The task is: {task_goal}. {context} {WORKER_INSTRUCTION}]")
}

/// The manager note: the reason, where the history stays, then the orientation order and a sort.
fn manager_reset_note(session_id: &str, reason: &str, history: &str) -> String {
  let child_tasks = child_task_list_note(session_id);
  format!(
    "[Context reset: {reason}.\n\
     {history}\n\
     Before you respond, get oriented from your session status, then your child tasks, then the last two turns.\n\
     {child_tasks}\n\
     `charliebot session dialog --turns 2` prints the last two turns.\n\
     A larger `--turns` value reads earlier turns.\n\
     Those turns show the actions the session just took, so you continue the work without repeating them.\n\
     Then sort the parts of your task, as Role: Manager directs after a context reset.\n\
     Then write two or three sentences on where things stand after the sort.\n\
     Then respond to what follows.\n\
     A Slack, Discord, or Feishu thread post carries only the answer that `prompts/thread_reply_format.md` defines.]"
  )
}

#[cfg(test)]
mod tests {
  use super::{
    backend_switch_reset_reason, manager_context_reset_note, manager_switch_reset_note, worker_switch_reset_note,
  };

  const SESSION_ID: &str = "synthetic-session";

  #[test]
  fn manager_reset_note_names_the_reason_and_the_history() {
    let note = manager_context_reset_note(SESSION_ID, "synthetic reason");
    assert!(note.starts_with(
      "[Context reset: synthetic reason.\nEarlier turns' history remains \
       readable"
    ));
    assert!(note.ends_with("`prompts/thread_reply_format.md` defines.]"));
  }

  /// The Goals reach a manager through its prompt, so the reset note carries no goal line of either kind.
  #[test]
  fn manager_reset_notes_carry_no_goal_line() {
    let reset = manager_context_reset_note(SESSION_ID, "synthetic reason");
    let switch = manager_switch_reset_note(SESSION_ID, "synthetic reason", "at /synthetic/log");
    for note in [reset, switch] {
      assert!(!note.to_lowercase().contains("goal"), "{note}");
    }
  }

  /// Orientation runs from the status, to the child tasks, to the last two turns, and then to the sort.
  #[test]
  fn manager_reset_note_orients_from_status_then_children_then_turns_then_sorts() {
    let note = manager_context_reset_note(SESSION_ID, "synthetic reason");
    let positions = ["session status", "session tree --root", "dialog --turns 2", "sort"]
      .map(|needle| note.find(needle).unwrap_or_else(|| panic!("{needle} is missing from {note}")));
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{positions:?}\n{note}");
  }

  #[test]
  fn manager_reset_note_keeps_each_fixed_line_within_120_characters() {
    let note = manager_context_reset_note(SESSION_ID, "synthetic reason");
    for line in note.lines() {
      assert!(line.chars().count() <= 120, "{} characters: {line}", line.chars().count());
    }
  }

  #[test]
  fn manager_switch_reset_note_names_the_context_paths_in_place_of_the_history_line() {
    let note = manager_switch_reset_note(SESSION_ID, "synthetic reason", "at /synthetic/log");
    assert!(note.contains("synthetic reason.\nat /synthetic/log\n"), "{note}");
    assert!(!note.contains("data/chat_events.jsonl"), "{note}");
  }

  /// The worker note stays word for word what every worker continuation carried before the manager note changed.
  #[test]
  fn worker_switch_reset_note_is_unchanged() {
    let note = worker_switch_reset_note("synthetic reason", "synthetic goal", "at /synthetic/log");
    assert_eq!(
      note,
      "[Context reset: synthetic reason. The task is: synthetic goal. at /synthetic/log Get oriented from the last two \
       turns of that log: run `charliebot session dialog --turns 2` before you respond. Those turns show the actions the \
       session just took, so you continue the work without repeating them. A larger `--turns` value reads earlier turns. \
       Then write two or three sentences on where things stand and the conventions in force. When your rules open the \
       reply with a status block, put these sentences after the block. Then respond to what follows. A Slack, Discord, or \
       Feishu thread post carries only the answer that `prompts/thread_reply_format.md` defines.]"
    );
  }

  #[test]
  fn backend_switch_reason_names_both_backends() {
    assert_eq!(
      backend_switch_reset_reason("synthetic-old", "synthetic-new"),
      "this session switched from backend synthetic-old to synthetic-new, which starts its own conversation"
    );
  }
}
