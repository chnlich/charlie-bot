//! The chat-events archive file-name rule shared by the store and the disk sweep.

/// Whether a file name in a session's `data/archives/` directory is a chat-events archive, matching
/// Python's `src/runtime/chat_events.py:47` `ARCHIVE_FILE_GLOB` (`chat_events.*.jsonl`): a bare
/// `chat_events.jsonl` carries the live log's name, not an archive's.
pub fn is_chat_events_archive(file_name: &str) -> bool {
  file_name.strip_prefix("chat_events.").and_then(|rest| rest.strip_suffix(".jsonl")).is_some()
}

#[cfg(test)]
mod tests {
  use super::is_chat_events_archive;

  #[test]
  fn test_archive_names_match_the_glob() {
    assert!(is_chat_events_archive("chat_events.1.jsonl"));
    assert!(is_chat_events_archive("chat_events..jsonl"));
  }

  #[test]
  fn test_non_archive_names_do_not_match_the_glob() {
    assert!(!is_chat_events_archive("chat_events.jsonl"));
    assert!(!is_chat_events_archive("chat_events.1.json"));
    assert!(!is_chat_events_archive("x.chat_events.1.jsonl"));
  }
}
