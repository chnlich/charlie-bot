//! The `int(text)` reading: the text Python reads as an integer, and the sign and digits it carries.

use std::ops::Neg;
use std::str::FromStr;

/// The sign and digits of `int(text)`: surrounding whitespace drops, then an optional `+` or `-`, then ASCII
/// digits with single underscores between them. The digits come back without their underscores.
///
/// A caller converts the digits at its own width, which is also the width its overflow rule names: a flag's
/// `i64`, a range header's `i128`, a window's `u64`. A caller that returns the number itself folds the digits
/// here with `int_value`. A caller that compares the magnitude against a bound folds the digits at a saturating
/// width instead, so a value past that width still reports the bound it violates.
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

/// `int(text)` folded at the width `T` names: the digits read at that width, then the sign applies.
///
/// A magnitude past `T`'s range reads as None, so the caller's width is also its overflow rule. `T` is a signed
/// integer, because an unsigned width has nowhere for the sign to go.
pub fn int_value<T: FromStr + Neg<Output = T>>(text: &str) -> Option<T> {
  let (negative, digits) = int_digits(text)?;
  let magnitude: T = digits.parse().ok()?;
  Some(if negative { -magnitude } else { magnitude })
}

#[cfg(test)]
mod tests {
  use super::{int_digits, int_value};

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

  #[test]
  fn int_value_folds_the_digits_at_the_width_the_caller_names() {
    assert_eq!(int_value::<i64>("8080"), Some(8080));
    assert_eq!(int_value::<i64>(" +8080 "), Some(8080));
    assert_eq!(int_value::<i64>("-1"), Some(-1));
    assert_eq!(int_value::<i64>("80_80"), Some(8080));
    assert_eq!(int_value::<i64>("80__80"), None);
    assert_eq!(int_value::<i128>("170141183460469231731687303715884105727"), Some(i128::MAX));
    assert_eq!(int_value::<i128>("170141183460469231731687303715884105728"), None);
    assert_eq!(int_value::<i64>("9223372036854775808"), None);
    assert_eq!(int_value::<i64>(""), None);
  }
}
