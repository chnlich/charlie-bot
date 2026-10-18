//! The `str` operations of CPython that the ssh config parser and the probe's detail depend on. Rust's own
//! methods differ from them: `char::is_whitespace` misses the file separators U+001C to U+001F, and `str::lines`
//! does not split on the vertical tab, form feed, or the other separators `str.splitlines` recognizes.

/// `str.isspace()` for one character.
pub fn is_space(c: char) -> bool {
  c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.strip()`.
pub fn strip(text: &str) -> &str {
  text.trim_matches(is_space)
}

/// `str.split()` with no separator: runs of whitespace divide the text, and no empty piece is returned.
pub fn split_whitespace(text: &str) -> Vec<&str> {
  text.split(is_space).filter(|piece| !piece.is_empty()).collect()
}

/// `str.splitlines()` without line ends.
pub fn splitlines(text: &str) -> Vec<&str> {
  let mut lines = Vec::new();
  let mut start = 0;
  let mut chars = text.char_indices().peekable();
  while let Some((index, c)) = chars.next() {
    if !matches!(
      c,
      '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    ) {
      continue;
    }
    lines.push(&text[start..index]);
    start = index + c.len_utf8();
    if c == '\r' && chars.peek().is_some_and(|&(_, next)| next == '\n') {
      chars.next();
      start += 1;
    }
  }
  if start < text.len() {
    lines.push(&text[start..]);
  }
  lines
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_separators_python_recognizes_are_whitespace_and_line_breaks() {
    assert_eq!(strip("\u{1c}login\u{1f} "), "login");
    assert_eq!(split_whitespace("a\u{1c}b = c"), ["a", "b", "=", "c"]);
    assert_eq!(splitlines("a\u{c}b\r\nc\rd\n"), ["a", "b", "c", "d"]);
    assert_eq!(splitlines("a\n\nb"), ["a", "", "b"]);
    assert!(splitlines("").is_empty());
  }
}
