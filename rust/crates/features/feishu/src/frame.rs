//! The Feishu long-connection frame: the protobuf envelope the server and Feishu exchange over one WebSocket. It also
//! holds the ping, event and ack shapes, and the reassembly of event payloads that arrive as fragments.
//!
//! Feishu documents the long connection only through its SDKs. The layout here is the official Go SDK's `pbbp2.Frame`
//! (`ws/pbbp2.pb.go`) and its header names (`ws/const.go`). A frame has nine fields. Its `method` is 0 for control
//! frames and 1 for data frames, and a `type` header names a ping, pong or event.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use prost::Message as _;

/// The `method` of a control frame: a ping, or its pong.
pub const METHOD_CONTROL: i32 = 0;
/// The `method` of a data frame: an event.
pub const METHOD_DATA: i32 = 1;
pub const TYPE_PING: &str = "ping";
pub const TYPE_EVENT: &str = "event";
const HEADER_TYPE: &str = "type";
const HEADER_MESSAGE_ID: &str = "message_id";
const HEADER_SUM: &str = "sum";
const HEADER_SEQ: &str = "seq";
const HEADER_BIZ_RT: &str = "biz_rt";
/// The most fragments one event payload may split into; a larger count is refused.
const MAX_FRAGMENTS: u64 = 64;
/// How long an incomplete fragment group waits for its missing parts.
const REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(5);
/// The payload of an ack frame: the event was taken.
const ACK_PAYLOAD: &str = r#"{"code":200}"#;

/// One `key: value` header of a frame.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Header {
  #[prost(string, required, tag = "1")]
  pub key: String,
  #[prost(string, required, tag = "2")]
  pub value: String,
}

/// One frame of the long connection. Fields 1 to 4 are required, as the SDK's proto2 schema declares them.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Frame {
  #[prost(uint64, required, tag = "1")]
  pub seq_id: u64,
  #[prost(uint64, required, tag = "2")]
  pub log_id: u64,
  #[prost(int32, required, tag = "3")]
  pub service: i32,
  #[prost(int32, required, tag = "4")]
  pub method: i32,
  #[prost(message, repeated, tag = "5")]
  pub headers: Vec<Header>,
  #[prost(string, optional, tag = "6")]
  pub payload_encoding: Option<String>,
  #[prost(string, optional, tag = "7")]
  pub payload_type: Option<String>,
  #[prost(bytes = "vec", optional, tag = "8")]
  pub payload: Option<Vec<u8>>,
  #[prost(string, optional, tag = "9")]
  pub log_id_new: Option<String>,
}

fn header(key: &str, value: &str) -> Header {
  Header { key: key.to_string(), value: value.to_string() }
}

impl Frame {
  /// The ping a client sends at the server's interval (`NewPingFrame`). The service id comes from the endpoint URL.
  pub fn ping(service: i32) -> Self {
    Self {
      seq_id: 0,
      log_id: 0,
      service,
      method: METHOD_CONTROL,
      headers: vec![header(HEADER_TYPE, TYPE_PING)],
      payload_encoding: None,
      payload_type: None,
      payload: None,
      log_id_new: None,
    }
  }

  /// The value of the first header named `name`, or None.
  pub fn header(&self, name: &str) -> Option<&str> {
    self.headers.iter().find(|candidate| candidate.key == name).map(|candidate| candidate.value.as_str())
  }

  /// The `type` header: `ping`, `pong` or `event`.
  pub fn kind(&self) -> Option<&str> {
    self.header(HEADER_TYPE)
  }

  /// The numeric header `name`, or None when the frame has none. A value that is not an unsigned integer is an error.
  fn header_u64(&self, name: &str) -> anyhow::Result<Option<u64>> {
    self
      .header(name)
      .map(|value| value.parse::<u64>().with_context(|| format!("frame header {name} is not an unsigned integer")))
      .transpose()
  }

  /// The ack of this event frame: the same frame with its processing time in milliseconds (`biz_rt`) appended, and the
  /// code-200 payload that tells Feishu the event was taken.
  pub fn ack(&self, biz_rt_ms: u128) -> Self {
    let mut ack = self.clone();
    ack.headers.push(header(HEADER_BIZ_RT, &biz_rt_ms.to_string()));
    ack.payload = Some(ACK_PAYLOAD.as_bytes().to_vec());
    ack
  }

  /// The wire bytes of the frame.
  pub fn to_bytes(&self) -> Vec<u8> {
    self.encode_to_vec()
  }

  /// The frame that wire bytes carry.
  pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
    Ok(Self::decode(bytes)?)
  }
}

/// Rebuilds event payloads that Feishu splits across frames. A frame whose `sum` header is above 1 is fragment `seq` of
/// `sum` fragments that share its `message_id`.
#[derive(Debug, Default)]
pub struct Reassembler {
  pending: HashMap<String, Fragments>,
}

#[derive(Debug)]
struct Fragments {
  parts: Vec<Option<Vec<u8>>>,
  started: Instant,
}

impl Reassembler {
  /// The complete payload of one data frame. An unfragmented frame yields its own payload. A fragment yields the joined
  /// payload once every fragment of its message has arrived, and None before that. A group whose first fragment is
  /// older than the reassembly timeout is dropped when the next fragment arrives.
  pub fn accept(&mut self, frame: &Frame, now: Instant) -> anyhow::Result<Option<Vec<u8>>> {
    let payload = frame.payload.clone().unwrap_or_default();
    let sum = frame.header_u64(HEADER_SUM)?.unwrap_or(1);
    if sum <= 1 {
      return Ok(Some(payload));
    }
    ensure!(sum <= MAX_FRAGMENTS, "a frame declares {sum} fragments, more than {MAX_FRAGMENTS}");
    let seq = frame.header_u64(HEADER_SEQ)?.context("a fragment frame has no seq header")?;
    ensure!(seq < sum, "fragment {seq} is outside the {sum} fragments of its message");
    let message_id = frame.header(HEADER_MESSAGE_ID).context("a fragment frame has no message_id header")?.to_string();
    self.pending.retain(|_, group| now.duration_since(group.started) < REASSEMBLY_TIMEOUT);
    let group = self
      .pending
      .entry(message_id.clone())
      .or_insert_with(|| Fragments { parts: vec![None; sum as usize], started: now });
    ensure!(group.parts.len() as u64 == sum, "the fragments of one message disagree on their count");
    group.parts[seq as usize] = Some(payload);
    if group.parts.iter().any(Option::is_none) {
      return Ok(None);
    }
    let group = self.pending.remove(&message_id).expect("the group was filled by the fragment just stored");
    Ok(Some(group.parts.into_iter().flatten().flatten().collect()))
  }
}

#[cfg(test)]
mod tests {
  use std::time::{Duration, Instant};

  use super::*;

  const TYPE_PONG: &str = "pong";

  /// A data frame carrying `payload` as one fragment of `sum`.
  fn data(message_id: &str, sum: u64, seq: u64, payload: &[u8]) -> Frame {
    Frame {
      seq_id: 1,
      log_id: 2,
      service: 7,
      method: METHOD_DATA,
      headers: vec![
        header(HEADER_TYPE, TYPE_EVENT),
        header(HEADER_MESSAGE_ID, message_id),
        header(HEADER_SUM, &sum.to_string()),
        header(HEADER_SEQ, &seq.to_string()),
      ],
      payload_encoding: None,
      payload_type: None,
      payload: Some(payload.to_vec()),
      log_id_new: None,
    }
  }

  #[test]
  fn a_ping_encodes_to_the_bytes_the_sdk_writes() {
    // Field by field: seq_id 0, log_id 0, service 7, method 0, one header type=ping. Required fields are written even
    // when they are zero.
    let golden: &[u8] = &[
      0x08, 0x00, 0x10, 0x00, 0x18, 0x07, 0x20, 0x00, 0x2a, 0x0c, 0x0a, 0x04, b't', b'y', b'p', b'e', 0x12, 0x04, b'p',
      b'i', b'n', b'g',
    ];
    let ping = Frame::ping(7);
    assert_eq!(ping.to_bytes(), golden);
    let decoded = Frame::from_bytes(golden).expect("the golden ping decodes");
    assert_eq!(decoded, ping);
    assert_eq!(decoded.method, METHOD_CONTROL);
    assert_eq!(decoded.kind(), Some(TYPE_PING));
  }

  #[test]
  fn a_pong_decodes_as_a_control_frame() {
    let pong = Frame {
      seq_id: 0,
      log_id: 0,
      service: 7,
      method: METHOD_CONTROL,
      headers: vec![header(HEADER_TYPE, TYPE_PONG)],
      payload_encoding: None,
      payload_type: None,
      payload: None,
      log_id_new: None,
    };
    let decoded = Frame::from_bytes(&pong.to_bytes()).expect("a pong round-trips");
    assert_eq!(decoded.method, METHOD_CONTROL);
    assert_eq!(decoded.kind(), Some(TYPE_PONG));
  }

  #[test]
  fn an_event_round_trips_its_headers_and_payload() {
    let event = data("om_1", 1, 0, br#"{"schema":"2.0"}"#);
    let decoded = Frame::from_bytes(&event.to_bytes()).expect("an event round-trips");
    assert_eq!(decoded, event);
    assert_eq!(decoded.kind(), Some(TYPE_EVENT));
    assert_eq!(decoded.header(HEADER_MESSAGE_ID), Some("om_1"));
  }

  #[test]
  fn an_ack_echoes_the_event_and_adds_the_processing_time_with_code_200() {
    let event = data("om_1", 1, 0, b"{}");
    let ack = Frame::from_bytes(&event.ack(12).to_bytes()).expect("an ack round-trips");
    assert_eq!(ack.seq_id, event.seq_id);
    assert_eq!(ack.service, event.service);
    assert_eq!(ack.method, event.method);
    assert_eq!(ack.header(HEADER_MESSAGE_ID), Some("om_1"));
    assert_eq!(ack.header(HEADER_BIZ_RT), Some("12"));
    assert_eq!(ack.payload.as_deref(), Some(ACK_PAYLOAD.as_bytes()));
  }

  #[test]
  fn an_unfragmented_event_yields_its_own_payload() {
    let mut reassembler = Reassembler::default();
    let payload = reassembler.accept(&data("om_1", 1, 0, b"whole"), Instant::now()).expect("the frame is valid");
    assert_eq!(payload.as_deref(), Some(b"whole".as_slice()));
  }

  #[test]
  fn fragments_join_in_sequence_once_every_part_has_arrived() {
    let mut reassembler = Reassembler::default();
    let now = Instant::now();
    assert_eq!(reassembler.accept(&data("om_2", 3, 2, b"c"), now).unwrap(), None);
    assert_eq!(reassembler.accept(&data("om_2", 3, 0, b"a"), now).unwrap(), None);
    let joined = reassembler.accept(&data("om_2", 3, 1, b"b"), now).unwrap();
    assert_eq!(joined.as_deref(), Some(b"abc".as_slice()));
  }

  #[test]
  fn a_stale_fragment_group_is_dropped_before_its_parts_join() {
    let mut reassembler = Reassembler::default();
    let start = Instant::now();
    assert_eq!(reassembler.accept(&data("om_3", 2, 0, b"x"), start).unwrap(), None);
    let later = start + REASSEMBLY_TIMEOUT + Duration::from_millis(1);
    // The old group is gone, so its second part starts a new group and does not complete it.
    assert_eq!(reassembler.accept(&data("om_3", 2, 1, b"y"), later).unwrap(), None);
  }

  #[test]
  fn a_fragment_outside_its_count_is_refused() {
    let mut reassembler = Reassembler::default();
    assert!(reassembler.accept(&data("om_4", 2, 2, b"z"), Instant::now()).is_err());
    assert!(reassembler.accept(&data("om_4", MAX_FRAGMENTS + 1, 0, b"z"), Instant::now()).is_err());
  }
}
