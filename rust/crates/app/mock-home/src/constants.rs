//! The synthetic home's constants: its data version, the production session count, the default seed and the search
//! words.

/// The data version. It names the mock home's directory, `~/improve/mock-home/<version>/`, and changes with any change
/// to the bytes that `generate` writes. Version 8 is the task-tree shape, which the improve rounds generate: its boot
/// marker lets a start skip every session that holds no open task, run or worktree. Its serve shape also writes the
/// fixture row that a ux round captures: one sidebar row that carries the six row indicators, with a long display name
/// and a long backend name (`task_tree::ServeFixture`).
pub const DATA_VERSION: &str = "8";
/// The session count of the production home, and the default of `generate`: the live store's session directories when
/// it was measured (14,451 at 18:04Z on 2026-11-19).
pub const PRODUCTION_SESSIONS: usize = 14_451;
/// The seed of `generate` when no seed is given.
pub const DEFAULT_SEED: u64 = 1;

/// The fixed words that the search measurement sends to the sidebar search route on this home. Each role names the
/// answer it gives on the production-size home at `DEFAULT_SEED`, counted over the session names and chat logs: a
/// session matches a word when the word occurs in either. The route answers at most 200 rows.
pub struct SearchWords {
  /// A word most sessions hold: 13,625 sessions match, so the answer fills its 200 rows.
  pub common: &'static str,
  /// A word whose 8,074 matching sessions fill the answer's 200 rows.
  pub hits_200: &'static str,
  /// A word one session holds: the first eight characters of that session's id, which its checkpoint line writes.
  pub rare: &'static str,
}

/// The search measurement's words, as the counts above state them.
pub const SEARCH_WORDS: SearchWords = SearchWords { common: "the", hits_200: "sidebar", rare: "0000c694" };
