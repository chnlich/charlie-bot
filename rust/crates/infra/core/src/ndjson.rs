//! One line of a newline-delimited JSON stream, under the skip contract the backend stdout and the run raw log
//! share.
//!
//! A blank line is not an event, and a producer can write bytes that are not UTF-8. A reader that skips a line
//! reports it under its own event name, so the decode returns the failure and leaves the logging to the caller.

use serde_json::Value;

/// Decode one NDJSON line under the skip contract.
///
/// A blank line gives `None`. A line that is not JSON gives its parse error, for the caller to log under its own
/// event name. A line whose bytes are not UTF-8 is repaired with U+FFFD before the parse, so a line a producer wrote
/// in a broken encoding still reaches the event stream.
pub fn parse_line(raw: &[u8]) -> Option<Result<Value, serde_json::Error>> {
  if raw.is_empty() || raw.iter().all(u8::is_ascii_whitespace) {
    return None;
  }
  match serde_json::from_slice(raw) {
    Ok(value) => Some(Ok(value)),
    Err(error) => match std::str::from_utf8(raw) {
      Ok(_) => Some(Err(error)),
      Err(_) => Some(serde_json::from_str(&String::from_utf8_lossy(raw))),
    },
  }
}

/// Skip the spaces and tabs at the start of `bytes`.
///
/// A reader uses this between the punctuation of a line's JSON header, where the producer's spacing is free-form, so
/// a header comparison needs no parse of the whole line.
pub fn trim_horizontal_space(mut bytes: &[u8]) -> &[u8] {
  while matches!(bytes.first(), Some(b' ' | b'\t')) {
    bytes = &bytes[1..];
  }
  bytes
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn value(raw: &[u8]) -> Value {
    parse_line(raw).expect("the line is present").expect("the line parses")
  }

  #[test]
  fn only_leading_spaces_and_tabs_are_trimmed() {
    assert_eq!(trim_horizontal_space(b" \t{\"type\""), b"{\"type\"");
    assert_eq!(trim_horizontal_space(b"\r\n x"), b"\r\n x");
    assert_eq!(trim_horizontal_space(b"x \t"), b"x \t");
  }

  #[test]
  fn a_blank_line_is_not_an_event() {
    assert!(parse_line(b"").is_none());
    assert!(parse_line(b" \t\r\n").is_none());
  }

  #[test]
  fn a_parsed_line_returns_its_value() {
    assert_eq!(value(b"{\"type\": \"result\"}"), json!({"type": "result"}));
    assert_eq!(value(b"  {\"i\": 1}  \n"), json!({"i": 1}));
  }

  #[test]
  fn a_line_that_is_not_json_returns_its_error() {
    assert!(parse_line(b"{not json").is_some_and(|result| result.is_err()));
  }

  #[test]
  fn invalid_utf8_is_repaired_before_the_parse() {
    assert_eq!(value(b"{\"text\": \"ok\xff\"}"), json!({"text": "ok\u{fffd}"}));
    assert!(parse_line(b"\xff").is_some_and(|result| result.is_err()));
  }
}
