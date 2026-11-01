//! The byte-level parts of the proxy's stream: the line framing, and the frame one SSE event writes.

use axum::response::sse::Event;
use charliebot_core::text_repr;
use memchr::memchr2;
use serde_json::{Map, Value};

use super::value_ops::COMPACT_ASCII_JSON;

/// Splits a byte stream into lines. CRLF, LF and CR each end a line. A CR at a chunk's end waits for the next byte,
/// so a CRLF split across chunks is still one terminator.
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

/// The event named `event`, with a compact JSON data field whose first member is `type`: the map inserts `type`
/// before the event's own members, and the workspace's `preserve_order` feature keeps that insertion order.
pub fn sse_event(event: &str, data: &Value) -> Event {
  let mut members = Map::new();
  members.insert("type".to_string(), Value::String(event.to_string()));
  if let Value::Object(data) = data {
    members.extend(data.clone());
  }
  Event::default().event(event).data(text_repr::json_dumps(&Value::Object(members), &COMPACT_ASCII_JSON))
}

#[cfg(test)]
mod tests {
  use std::io;

  use axum::body::to_bytes;
  use axum::response::IntoResponse;
  use axum::response::sse::Sse;
  use futures_util::stream;
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

  #[tokio::test]
  async fn a_frame_names_its_type_first_and_renders_ascii_escapes() {
    let event = sse_event("content_block_delta", &json!({"index": 0, "delta": {"text": "é\u{2028}"}}));
    let body = Sse::new(stream::iter([Ok::<Event, io::Error>(event)])).into_response().into_body();
    let frame = to_bytes(body, usize::MAX).await.unwrap();
    assert_eq!(
      String::from_utf8(frame.to_vec()).unwrap(),
      "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"text\":\"\\u00e9\\u2028\"}}\n\n"
    );
  }
}
