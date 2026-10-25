//! The byte-level parts of the proxy's stream: the line framing of `src/infra/sse.py` and the frame `_sse_event` writes.

use charliebot_core::pyfmt;
use memchr::memchr2;
use serde_json::{Map, Value};

use super::pyvalue::COMPACT_ASCII_JSON;

/// Splits a byte stream into lines as `sse.iter_sse_lines` frames them: CRLF, LF and CR each end a line, and a CR at a
/// chunk's end waits for the next byte, so a CRLF split across chunks is still one terminator.
#[derive(Default)]
pub struct LineFramer {
  line: Vec<u8>,
  after_cr: bool,
}

impl LineFramer {
  /// The lines that `chunk` completes, without their terminators.
  pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
    if chunk.is_empty() {
      return Vec::new();
    }
    let mut body = chunk;
    if std::mem::take(&mut self.after_cr) && body[0] == b'\n' {
      body = &body[1..];
    }
    let mut lines = Vec::new();
    let mut start = 0;
    while let Some(offset) = memchr2(b'\r', b'\n', &body[start..]) {
      let end = start + offset;
      self.line.extend_from_slice(&body[start..end]);
      lines.push(std::mem::take(&mut self.line));
      start = end + 1;
      if body[end] == b'\r' && body.get(start) == Some(&b'\n') {
        start += 1;
      }
    }
    self.line.extend_from_slice(&body[start..]);
    self.after_cr = body.last() == Some(&b'\r');
    lines
  }

  /// The unterminated last line when the stream ends, when it holds any bytes.
  pub fn finish(self) -> Option<Vec<u8>> {
    (!self.line.is_empty()).then_some(self.line)
  }
}

/// `_sse_event(event, data)`: `event: <name>` and a compact JSON `data` line whose first member is `type`.
pub fn sse_frame(event: &str, data: &Value) -> Vec<u8> {
  let mut members = Map::new();
  members.insert("type".to_string(), Value::String(event.to_string()));
  if let Value::Object(data) = data {
    members.extend(data.clone());
  }
  format!("event: {event}\ndata: {}\n\n", pyfmt::json_dumps(&Value::Object(members), &COMPACT_ASCII_JSON)).into_bytes()
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  #[test]
  fn every_terminator_ends_a_line_across_chunk_boundaries() {
    let mut framer = LineFramer::default();
    let mut lines = framer.push(b"a\r");
    lines.extend(framer.push(b"\nb\rc\n\r"));
    lines.extend(framer.push(b"\nd"));
    assert_eq!(lines, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"".to_vec()]);
    assert_eq!(framer.finish(), Some(b"d".to_vec()));
  }

  #[test]
  fn an_empty_chunk_keeps_a_pending_cr_for_the_next_lf() {
    let mut framer = LineFramer::default();
    let mut lines = framer.push(b"x\r");
    lines.extend(framer.push(b""));
    lines.extend(framer.push(b"\r\n\n"));
    assert_eq!(lines, vec![b"x".to_vec(), b"".to_vec(), b"".to_vec()]);
    assert_eq!(framer.finish(), None);
  }

  #[test]
  fn a_frame_names_its_type_first_and_renders_ascii_escapes() {
    let frame = sse_frame("content_block_delta", &json!({"index": 0, "delta": {"text": "é\u{2028}"}}));
    assert_eq!(
      String::from_utf8(frame).unwrap(),
      "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"text\":\"\\u00e9\\u2028\"}}\n\n"
    );
  }
}
