//! Native-context reset notes. A manager and a worker each get their own note. The task record is
//! the system prompt's last block, so the notes carry no goal and no reading steps.

/// The reset reason when a backend change itself forces the fresh conversation.
pub fn backend_switch_reset_reason(native_backend: &str, option_id: &str) -> String {
  format!("this session switched from backend {native_backend} to {option_id}, which starts its own conversation")
}

/// The manager note that opens a fresh native conversation in `session_id`'s chat log: the reason,
/// the sentence that reanchors the earlier turns under the current rules, and where the state lives.
pub fn manager_context_reset_note(session_id: &str, reason: &str) -> String {
  manager_reset_note(session_id, reason, None)
}

/// The manager note of a retry that continues another run's context at `context`: the same note with
/// the absolute paths that hold that context.
pub fn manager_switch_reset_note(session_id: &str, reason: &str, context: &str) -> String {
  manager_reset_note(session_id, reason, Some(context))
}

/// The worker note of a retry that continues another run's context at `context`: the absolute paths
/// that hold it, then the instruction the fresh native conversation follows before it responds.
pub fn worker_switch_reset_note(reason: &str, context: &str) -> String {
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
  format!("[Context reset: {reason}. {context} {WORKER_INSTRUCTION}]")
}

/// The manager note: the reason, the reanchor sentence, the retry's context paths when a retry
/// carries them, then where the state lives.
fn manager_reset_note(session_id: &str, reason: &str, context: Option<&str>) -> String {
  let context = context.map(|line| format!("\n{line}")).unwrap_or_default();
  format!(
    "[Context reset: {reason}.\n\
     Your earlier turns ran under the previous rules. Read them as the state of the work, and continue under the \
     current rules, the split of your task included.{context}\n\
     State lives in `charliebot web status show`, `charliebot session tree --root {session_id}` and `charliebot \
     session dialog --turns <n>.]"
  )
}

#[cfg(test)]
mod tests {
  use super::{
    backend_switch_reset_reason, manager_context_reset_note, manager_switch_reset_note, worker_switch_reset_note,
  };

  #[test]
  fn backend_switch_reason_names_both_backends() {
    assert_eq!(
      backend_switch_reset_reason("synthetic-old", "synthetic-new"),
      "this session switched from backend synthetic-old to synthetic-new, which starts its own conversation"
    );
  }

  const SESSION_ID: &str = "synthetic-session";

  /// The note is word for word the reset text plan 4.1 pins (decision G): the reason, the
  /// reanchor sentence, and where the state lives. The task record's block in the system prompt
  /// carries the goal, so the note names none.
  #[test]
  fn the_manager_reset_note_is_the_pinned_text() {
    let note = manager_context_reset_note(SESSION_ID, "synthetic reason");
    assert_eq!(
      note,
      "[Context reset: synthetic reason.\n\
       Your earlier turns ran under the previous rules. Read them as the state of the work, and continue under the \
       current rules, the split of your task included.\n\
       State lives in `charliebot web status show`, `charliebot session tree --root synthetic-session` and \
       `charliebot session dialog --turns <n>.]"
    );
  }

  #[test]
  fn the_manager_switch_note_keeps_the_context_paths() {
    let note = manager_switch_reset_note(SESSION_ID, "synthetic reason", "at /synthetic/log");
    assert!(
      note.contains("the split of your task included.\nat /synthetic/log\nState lives in `charliebot web status show`")
    );
    assert!(!note.contains("data/chat_events.jsonl"));
  }

  /// The worker note stays word for word what every worker continuation carried, minus the task
  /// line the task block replaces.
  #[test]
  fn worker_switch_reset_note_carries_the_context_and_the_instruction() {
    let note = worker_switch_reset_note("synthetic reason", "at /synthetic/log");
    assert!(note.starts_with("[Context reset: synthetic reason. at /synthetic/log Get oriented from the last two"));
    assert!(!note.contains("The task is:"), "{note}");
    assert!(note.ends_with("`prompts/thread_reply_format.md` defines.]"));
  }
}
