//! The verify-result trailer: the one authority for the `RESULT:` line a verify report ends with
//! (`src/runtime/verify_trailer.py`). The verify prompt shows it, and the verify reader matches it.

use std::sync::OnceLock;

use regex::Regex;

/// The trailer pattern (`VERIFY_RESULT_TRAILER_RE.pattern`).
pub const VERIFY_RESULT_TRAILER_PATTERN: &str = r"RESULT: (?:clean|[1-9][0-9]* mismatch(?:es)? \([0-9]+ approval\))";

/// The compiled trailer pattern (`VERIFY_RESULT_TRAILER_RE`).
pub fn verify_result_trailer_re() -> &'static Regex {
  static TRAILER: OnceLock<Regex> = OnceLock::new();
  TRAILER.get_or_init(|| Regex::new(VERIFY_RESULT_TRAILER_PATTERN).expect("the trailer pattern is a valid regex"))
}

/// The trailer as the verify prompt names it: the pattern in backticks (`VERIFY_RESULT_TRAILER_EXPECTED`).
pub fn verify_result_trailer_expected() -> String {
  format!("`{VERIFY_RESULT_TRAILER_PATTERN}`")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn trailer_accepts_clean_and_counted_results_only() {
    let trailer = verify_result_trailer_re();
    assert!(trailer.is_match("RESULT: clean"));
    assert!(trailer.is_match("RESULT: 2 mismatches (1 approval)"));
    assert!(!trailer.is_match("RESULT: 0 mismatches (0 approval)"));
  }
}
