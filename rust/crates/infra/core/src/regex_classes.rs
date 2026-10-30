//! Python regular-expression character classes used by Rust regex engines.

use fancy_regex::Regex;

const WHITESPACE: &str = r"[\x{0009}-\x{000d}\x{001c}-\x{0020}\x{0085}\x{00a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}]";
const NON_WHITESPACE: &str = r"[^\x{0009}-\x{000d}\x{001c}-\x{0020}\x{0085}\x{00a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}]";
const WORD_BOUNDARY: &str = r"(?:(?<![\p{L}\p{N}_])(?=[\p{L}\p{N}_])|(?<=[\p{L}\p{N}_])(?![\p{L}\p{N}_]))";

/// Compile the Python `\s`, `\S` and `\b` classes that differ from fancy-regex's Unicode defaults.
pub fn compile_regex(pattern: &str) -> Regex {
  let pattern = whitespace_classes(pattern).replace(r"\b", WORD_BOUNDARY);
  Regex::new(&pattern).expect("the Python-compatible expression is valid")
}

/// Compile Python whitespace classes for `regex`, which does not support lookaround.
pub fn compile_automata_regex(pattern: &str) -> regex::Regex {
  regex::Regex::new(&whitespace_classes(pattern)).expect("the Python-compatible expression is valid")
}

fn whitespace_classes(pattern: &str) -> String {
  pattern.replace(r"\s", WHITESPACE).replace(r"\S", NON_WHITESPACE)
}
