//! The quoted-string and escape scanning that both inline copies share.
//!
//! A JavaScript string and a CSS string end the same way: at their closing quote, or at the line's end unless a
//! backslash escapes it. Both inline strips must keep such a literal whole, so this module owns that scan once.

/// The index after the quoted string that starts at `start`: its closing quote, or the line's end when the line
/// ends first. A backslash escapes the character after it, and a backslash that ends a line continues the string on
/// the next one.
pub(crate) fn string_end(source: &str, start: usize) -> usize {
  let bytes = source.as_bytes();
  let quote = bytes[start];
  let mut i = start + 1;
  while i < bytes.len() {
    match bytes[i] {
      b'\\' => i = escape_end(source, i),
      b'\n' | b'\r' => return i,
      byte if byte == quote => return i + 1,
      _ => i += 1,
    }
  }
  bytes.len()
}

/// The index after the character that the escape at `start` consumes: the line terminator after its backslash, the
/// escaped character, or the backslash alone at the source's end.
pub(crate) fn escape_end(source: &str, start: usize) -> usize {
  let bytes = source.as_bytes();
  let i = start + 1;
  match bytes.get(i) {
    None => i,
    Some(b'\r') if bytes.get(i + 1) == Some(&b'\n') => i + 2,
    Some(_) => i + char_len(&source[i..]),
  }
}

/// The length in bytes of the first character of `text`.
pub(crate) fn char_len(text: &str) -> usize {
  text.chars().next().map_or(1, char::len_utf8)
}
