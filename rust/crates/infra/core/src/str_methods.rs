//! CPython's `str` methods on Unicode text, for the ports to match: `isalnum`, `isdigit`, the decimal value of a decimal
//! digit, `isspace`, and `split()` with its `" ".join` collapse.

use std::sync::LazyLock;

use regex::Regex;

const DIGIT_RANGES: &[(u32, u32)] = &[
  (0x0030, 0x0039),
  (0x00b2, 0x00b3),
  (0x00b9, 0x00b9),
  (0x0660, 0x0669),
  (0x06f0, 0x06f9),
  (0x07c0, 0x07c9),
  (0x0966, 0x096f),
  (0x09e6, 0x09ef),
  (0x0a66, 0x0a6f),
  (0x0ae6, 0x0aef),
  (0x0b66, 0x0b6f),
  (0x0be6, 0x0bef),
  (0x0c66, 0x0c6f),
  (0x0ce6, 0x0cef),
  (0x0d66, 0x0d6f),
  (0x0de6, 0x0def),
  (0x0e50, 0x0e59),
  (0x0ed0, 0x0ed9),
  (0x0f20, 0x0f29),
  (0x1040, 0x1049),
  (0x1090, 0x1099),
  (0x1369, 0x1371),
  (0x17e0, 0x17e9),
  (0x1810, 0x1819),
  (0x1946, 0x194f),
  (0x19d0, 0x19da),
  (0x1a80, 0x1a89),
  (0x1a90, 0x1a99),
  (0x1b50, 0x1b59),
  (0x1bb0, 0x1bb9),
  (0x1c40, 0x1c49),
  (0x1c50, 0x1c59),
  (0x2070, 0x2070),
  (0x2074, 0x2079),
  (0x2080, 0x2089),
  (0x2460, 0x2468),
  (0x2474, 0x247c),
  (0x2488, 0x2490),
  (0x24ea, 0x24ea),
  (0x24f5, 0x24fd),
  (0x24ff, 0x24ff),
  (0x2776, 0x277e),
  (0x2780, 0x2788),
  (0x278a, 0x2792),
  (0xa620, 0xa629),
  (0xa8d0, 0xa8d9),
  (0xa900, 0xa909),
  (0xa9d0, 0xa9d9),
  (0xa9f0, 0xa9f9),
  (0xaa50, 0xaa59),
  (0xabf0, 0xabf9),
  (0xff10, 0xff19),
  (0x104a0, 0x104a9),
  (0x10a40, 0x10a43),
  (0x10d30, 0x10d39),
  (0x10d40, 0x10d49),
  (0x10e60, 0x10e68),
  (0x11052, 0x1105a),
  (0x11066, 0x1106f),
  (0x110f0, 0x110f9),
  (0x11136, 0x1113f),
  (0x111d0, 0x111d9),
  (0x112f0, 0x112f9),
  (0x11450, 0x11459),
  (0x114d0, 0x114d9),
  (0x11650, 0x11659),
  (0x116c0, 0x116c9),
  (0x116d0, 0x116e3),
  (0x11730, 0x11739),
  (0x118e0, 0x118e9),
  (0x11950, 0x11959),
  (0x11bf0, 0x11bf9),
  (0x11c50, 0x11c59),
  (0x11d50, 0x11d59),
  (0x11da0, 0x11da9),
  (0x11f50, 0x11f59),
  (0x16130, 0x16139),
  (0x16a60, 0x16a69),
  (0x16ac0, 0x16ac9),
  (0x16b50, 0x16b59),
  (0x16d70, 0x16d79),
  (0x1ccf0, 0x1ccf9),
  (0x1d7ce, 0x1d7ff),
  (0x1e140, 0x1e149),
  (0x1e2f0, 0x1e2f9),
  (0x1e4f0, 0x1e4f9),
  (0x1e5f1, 0x1e5fa),
  (0x1e950, 0x1e959),
  (0x1f100, 0x1f10a),
  (0x1fbf0, 0x1fbf9),
];

const DECIMAL_ZEROES: &[u32] = &[
  0x0030, 0x0660, 0x06f0, 0x07c0, 0x0966, 0x09e6, 0x0a66, 0x0ae6, 0x0b66, 0x0be6, 0x0c66, 0x0ce6, 0x0d66, 0x0de6,
  0x0e50, 0x0ed0, 0x0f20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90, 0x1b50, 0x1bb0, 0x1c40,
  0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0, 0xff10, 0x104a0, 0x10d30, 0x10d40, 0x11066, 0x110f0,
  0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0, 0x11650, 0x116c0, 0x116d0, 0x116da, 0x11730, 0x118e0, 0x11950, 0x11bf0,
  0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16130, 0x16a60, 0x16ac0, 0x16b50, 0x16d70, 0x1ccf0, 0x1d7ce, 0x1d7d8, 0x1d7e2,
  0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0, 0x1e5f1, 0x1e950, 0x1fbf0,
];

// Python's alphanumeric set uses Unicode Letter and Number categories. Rust's char predicate also accepts
// Other_Alphabetic.
static ALPHANUMERIC: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}]$").expect("the Python alphanumeric character pattern is valid"));

/// Whether CPython's `str.isalnum()` accepts this character.
pub fn is_alphanumeric(character: char) -> bool {
  let mut buffer = [0; 4];
  ALPHANUMERIC.is_match(character.encode_utf8(&mut buffer))
}

pub fn is_digit(character: char) -> bool {
  let code_point = character as u32;
  DIGIT_RANGES.iter().any(|&(start, end)| (start..=end).contains(&code_point))
}

/// Python's `str.isdigit`: a non-empty string whose every character is a digit.
pub fn isdigit(text: &str) -> bool {
  !text.is_empty() && text.chars().all(is_digit)
}

pub fn decimal_value(character: char) -> Option<u8> {
  let code_point = character as u32;
  DECIMAL_ZEROES.iter().find_map(|&zero| (zero..zero + 10).contains(&code_point).then(|| (code_point - zero) as u8))
}

pub fn is_whitespace(character: char) -> bool {
  matches!(
    character,
    '\u{0009}'..='\u{000d}' | '\u{001c}'..='\u{0020}' | '\u{0085}' | '\u{00a0}' | '\u{1680}' |
      '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
  )
}

/// Python's `str.strip()` with no argument: trims the characters that `is_whitespace` accepts from both ends.
pub fn strip(text: &str) -> &str {
  text.trim_matches(is_whitespace)
}

pub fn split_whitespace(text: &str) -> Vec<&str> {
  let mut words = Vec::new();
  let mut start = None;
  for (offset, character) in text.char_indices() {
    if is_whitespace(character) {
      if let Some(begin) = start.take() {
        words.push(&text[begin..offset]);
      }
    } else if start.is_none() {
      start = Some(offset);
    }
  }
  if let Some(start) = start {
    words.push(&text[start..]);
  }
  words
}

pub fn collapse_whitespace(text: &str) -> String {
  split_whitespace(text).join(" ")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn digit_and_decimal_methods_keep_their_unicode_sets() {
    assert!(is_digit('²'));
    assert_eq!(decimal_value('²'), None);
    assert_eq!(decimal_value('٣'), Some(3));
    assert!(is_digit('٣'));
    assert!(!is_digit('⅓'));
  }

  #[test]
  fn split_uses_whitespace() {
    assert_eq!(split_whitespace("\u{001c}one\u{3000}two\u{001f}"), ["one", "two"]);
  }

  #[test]
  fn strip_trims_both_ends_and_keeps_zero_width_space() {
    assert_eq!(strip("\u{001c}\u{0085}\u{3000}pad\u{3000}\u{0085}\u{001c}"), "pad");
    assert_eq!(strip("\u{200b}pad\u{200b}"), "\u{200b}pad\u{200b}");
  }
}
