//! The `int(text)` reading: the text Python reads as an integer, and the sign and digits it carries.

/// The sign and digits of `int(text)`: surrounding whitespace drops, then an optional `+` or `-`, then ASCII
/// digits with single underscores between them. The digits come back without their underscores.
///
/// A caller converts the digits at its own width, which is also the width its overflow rule names: a flag's
/// `i64`, a range header's `i128`, a window's `u64`. A caller that compares the magnitude against a bound
/// folds the digits at a saturating width, so a value past that width still reports the bound it violates.
pub fn int_digits(text: &str) -> Option<(bool, String)> {
  let text = text.trim();
  let (negative, unsigned) = match text.strip_prefix('-') {
    Some(rest) => (true, rest),
    None => (false, text.strip_prefix('+').unwrap_or(text)),
  };
  let bytes = unsigned.as_bytes();
  let ends_on_digits = match (bytes.first(), bytes.last()) {
    (Some(first), Some(last)) => first.is_ascii_digit() && last.is_ascii_digit(),
    _ => false,
  };
  let digit_text = bytes.iter().all(|byte| byte.is_ascii_digit() || *byte == b'_') && !unsigned.contains("__");
  (ends_on_digits && digit_text).then(|| (negative, unsigned.replace('_', "")))
}

#[cfg(test)]
mod tests {
  use super::int_digits;

  #[test]
  fn int_digits_reads_whitespace_sign_and_underscores_between_digits() {
    assert_eq!(int_digits("12"), Some((false, "12".to_string())));
    assert_eq!(int_digits(" 12 "), Some((false, "12".to_string())));
    assert_eq!(int_digits("+5"), Some((false, "5".to_string())));
    assert_eq!(int_digits("-5"), Some((true, "5".to_string())));
    assert_eq!(int_digits("1_000"), Some((false, "1000".to_string())));
    assert_eq!(int_digits("-1_0_0"), Some((true, "100".to_string())));
    assert_eq!(int_digits("00012"), Some((false, "00012".to_string())));
    assert_eq!(int_digits("-0"), Some((true, "0".to_string())));
  }

  #[test]
  fn int_digits_refuses_a_text_that_is_not_an_int() {
    for text in ["", "-", "+", "  ", "1_", "_1", "1__0", "12x", "1.0", "+-5", "0x10", "\u{661}\u{662}", "+ 5"] {
      assert_eq!(int_digits(text), None, "{text:?}");
    }
  }
}
