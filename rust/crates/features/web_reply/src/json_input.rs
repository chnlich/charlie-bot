//! Keep Python JSON number types when the shared JSON reader cannot represent their values.

/// Replace each JSON number, `NaN` or infinity token with zero, preserving the input's JSON shape.
///
/// Callers use this only after `charliebot_compat::json_loads` has validated the entire document and reported a value
/// it cannot represent. The exact numeric value is unused by these routes; Pydantic only needs to see a number so it
/// can report the same field type error Python reports.
pub fn replace_unrepresentable_numbers(text: &str) -> String {
  let bytes = text.as_bytes();
  let mut result = String::with_capacity(text.len());
  let mut index = 0;
  let mut in_string = false;
  let mut escaped = false;

  while index < bytes.len() {
    let character = text[index..].chars().next().expect("the index is on a character boundary");
    if in_string {
      result.push(character);
      index += character.len_utf8();
      if escaped {
        escaped = false;
      } else if character == '\\' {
        escaped = true;
      } else if character == '"' {
        in_string = false;
      }
      continue;
    }
    if character == '"' {
      in_string = true;
      result.push(character);
      index += character.len_utf8();
      continue;
    }

    let rest = &text[index..];
    let special_len =
      ["-Infinity", "Infinity", "NaN"].iter().find_map(|token| rest.starts_with(token).then_some(token.len()));
    if let Some(token_len) = special_len {
      result.push('0');
      index += token_len;
      continue;
    }
    if character.is_ascii_digit() || (character == '-' && bytes.get(index + 1).is_some_and(u8::is_ascii_digit)) {
      let end = number_end(bytes, index);
      result.push('0');
      index = end;
      continue;
    }
    result.push(character);
    index += character.len_utf8();
  }
  result
}

/// The end of a JSON number token. The caller has already validated the document's JSON syntax.
fn number_end(bytes: &[u8], mut index: usize) -> usize {
  if bytes[index] == b'-' {
    index += 1;
  }
  while bytes.get(index).is_some_and(u8::is_ascii_digit) {
    index += 1;
  }
  if bytes.get(index) == Some(&b'.') {
    index += 1;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
      index += 1;
    }
  }
  if matches!(bytes.get(index), Some(b'e' | b'E')) {
    index += 1;
    if matches!(bytes.get(index), Some(b'+' | b'-')) {
      index += 1;
    }
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
      index += 1;
    }
  }
  index
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn replaces_only_numbers_outside_strings() {
    let text = r#"{"goal": "1e400 NaN", "now": 123456789012345678901234567890, "waiting": [1.5, -Infinity]}"#;
    assert_eq!(replace_unrepresentable_numbers(text), r#"{"goal": "1e400 NaN", "now": 0, "waiting": [0, 0]}"#,);
  }
}
