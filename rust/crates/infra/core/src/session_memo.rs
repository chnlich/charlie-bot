//! The session memo sentence. Every context reorganization puts it at the top of the new context: the tasks layer's
//! reset and switch notes, the Claude and Codex compaction hooks and the Charlie Code rebuilt context all carry it.
//! The carriers span a runtime crate, the app CLI and a backend, so the sentence belongs to the crate all of them
//! depend on. The memo file's name belongs to `charliebot_store::layout`.

/// The sentence a rebuilt context opens with. The agent rereads its session memo, `memo.md` in the working directory,
/// before it acts in the rebuilt conversation.
pub const SESSION_MEMO_NOTE: &str =
  "Read your session memo, memo.md in the working directory, in full before your next action.";
