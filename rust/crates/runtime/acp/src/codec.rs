//! The ACP codec: one agent turn as JSON-RPC requests on one session, read back into CharlieBot events.
//!
//! The turn's first prompt is the run's initial prompt. Each live message is a further `session/prompt` on the same
//! session. The codec reports three neutral signals to the runtime:
//!  - `Delivered`: the first error-free response after a message's write. An error response to the message's own
//!    request drops the message from this turn, so it waits for the next one.
//!  - `TurnEnded`: every request the codec wrote has been answered. The codec refuses any input after this point, so a
//!    message can never be written into a turn that has already ended.
//!  - usage: the turn's usage rides the `result` event that the turn end emits. Each response's usage is one request's
//!    own count (`outputTokens` already includes `thoughtTokens`, and counts are not running totals across requests), so
//!    the turn sums each response once.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use charliebot_agents::event_builders::{make_error_event, make_result_event, make_text_event};
use charliebot_core::event_types;
use charliebot_registry::midturn_delivery::{EncodedInput, MidturnCodec, MidturnSignal, Observed};
use charliebot_store::run_records::RAW_LOG_NAME;
use serde_json::{Value, json};

/// The request id of `initialize`, the first line the handshake writes.
pub const INITIALIZE_ID: u64 = 1;
/// The request id of `session/load` or `session/new`.
pub const SESSION_ID: u64 = 2;
/// The request id of `session/set_model`, written only when the session selects a model.
pub const MODEL_ID: u64 = 3;
/// The first request id of a prompt: the run's initial prompt. Each later prompt counts up from it.
pub const FIRST_PROMPT_ID: u64 = 4;

/// The session the prompts go to. The handshake fills it before the first prompt is written.
pub type SharedSession = Arc<Mutex<Option<String>>>;

/// The turn's usage sums, one request's values added once.
#[derive(Default)]
struct Usage {
  input: i64,
  output: i64,
  cache_read: i64,
  cache_creation: i64,
}

/// The codec of one ACP session: the requests it wrote and the turn state their answers have moved.
pub struct AcpCodec {
  session: SharedSession,
  next_id: u64,
  /// True until the session response. The history that `session/load` replays comes before that response, and it is
  /// not this turn's output.
  replay_open: bool,
  /// Requests written and not yet answered.
  outstanding: BTreeSet<u64>,
  /// Input requests written and not yet delivered; the next error-free response delivers them.
  undelivered: Vec<u64>,
  /// The agent text, keyed by the latest prompt id written when each chunk arrived.
  texts: BTreeMap<u64, String>,
  /// The highest prompt id whose response was error-free: it names the turn's final answer.
  final_prompt: Option<u64>,
  /// Whether the initial prompt's response was error-free: the turn's status.
  initial_succeeded: Option<bool>,
  usage: Usage,
  ended: bool,
}

impl AcpCodec {
  /// A codec for the session the handshake fills into `session`.
  pub fn new(session: SharedSession) -> Self {
    Self {
      session,
      next_id: FIRST_PROMPT_ID,
      replay_open: true,
      outstanding: BTreeSet::new(),
      undelivered: Vec::new(),
      texts: BTreeMap::new(),
      final_prompt: None,
      initial_succeeded: None,
      usage: Usage::default(),
      ended: false,
    }
  }

  /// The `session/prompt` line of one prompt id.
  fn prompt_line(&self, id: u64, text: &str) -> anyhow::Result<Vec<u8>> {
    let session_id = self.session.lock().expect("ACP session mutex is not poisoned");
    let session_id = session_id.as_deref().context("the ACP handshake has not set a session before a prompt")?;
    let request = json!({
      "jsonrpc": "2.0",
      "id": id,
      "method": "session/prompt",
      "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": text}]},
    });
    Ok(line_bytes(&request))
  }

  /// Takes the next prompt id and records it as outstanding.
  fn next_prompt(&mut self) -> u64 {
    let id = self.next_id;
    self.next_id += 1;
    self.outstanding.insert(id);
    id
  }

  /// One output event of the agent: its translated events and neutral signals. `emit` is false while the codec
  /// rebuilds its state from output that was already projected.
  fn process(&mut self, raw: &Value, emit: bool) -> Observed {
    let mut events = Vec::new();
    let mut signals = Vec::new();
    if let Some(method) = raw.get("method").and_then(Value::as_str) {
      if method == "session/update" && !self.replay_open {
        let update_events = self.update(raw);
        if emit {
          events.extend(update_events);
        }
      }
      return Observed { events, signals };
    }
    let Some(id) = raw.get("id").and_then(Value::as_u64) else {
      return Observed { events, signals };
    };
    let error = raw.get("error");
    if id == SESSION_ID {
      self.replay_open = false;
      let session_id = raw.pointer("/result/sessionId").and_then(Value::as_str);
      if error.is_none()
        && let Some(session_id) = session_id
      {
        *self.session.lock().expect("ACP session mutex is not poisoned") = Some(session_id.to_string());
      }
      if emit {
        match (error, session_id) {
          (None, Some(session_id)) => {
            events.push(json!({"type": event_types::SESSION_ATTACHED, "session_id": session_id}));
          }
          (Some(error), _) => events.push(error_event(error)),
          (None, None) => events.push(make_error_event("the ACP session response carries no sessionId")),
        }
      }
      return Observed { events, signals };
    }
    if id < FIRST_PROMPT_ID {
      if let (true, Some(error)) = (emit, error) {
        events.push(error_event(error));
      }
      return Observed { events, signals };
    }
    if !self.outstanding.remove(&id) {
      return Observed { events, signals };
    }
    match error {
      Some(error) => {
        if emit {
          events.push(error_event(error));
        }
        // The message this request carried stays out of this turn: it waits for the next turn.
        self.undelivered.retain(|undelivered| *undelivered != id);
        if id == FIRST_PROMPT_ID {
          self.initial_succeeded = Some(false);
        }
      }
      None => {
        if !self.undelivered.is_empty() {
          let keys = std::mem::take(&mut self.undelivered).iter().map(u64::to_string).collect();
          signals.push(MidturnSignal::Delivered(keys));
        }
        self.add_usage(raw);
        self.final_prompt = Some(self.final_prompt.map_or(id, |last| last.max(id)));
        if id == FIRST_PROMPT_ID {
          self.initial_succeeded = Some(true);
        }
      }
    }
    if self.outstanding.is_empty() && !self.ended {
      self.ended = true;
      let succeeded = self.initial_succeeded == Some(true);
      signals.push(MidturnSignal::TurnEnded { succeeded });
      if emit {
        events.push(self.result_event());
      }
    }
    Observed { events, signals }
  }

  /// The events of one `session/update` notification, and the agent text it carries.
  fn update(&mut self, raw: &Value) -> Vec<Value> {
    let Some(update) = raw.pointer("/params/update") else {
      return Vec::new();
    };
    match update.get("sessionUpdate").and_then(Value::as_str) {
      Some("agent_message_chunk") => {
        let text = update.pointer("/content/text").and_then(Value::as_str).unwrap_or_default();
        if let Some(latest) = self.latest_prompt() {
          self.texts.entry(latest).or_default().push_str(text);
        }
        if text.is_empty() { Vec::new() } else { vec![make_text_event(text)] }
      }
      Some("agent_thought_chunk") => {
        let text = update.pointer("/content/text").and_then(Value::as_str).unwrap_or_default();
        if text.is_empty() { Vec::new() } else { vec![json!({"type": event_types::THINKING, "content": text})] }
      }
      Some("tool_call") => {
        let input = update.get("rawInput").or_else(|| update.get("args")).cloned().unwrap_or(Value::Null);
        vec![json!({
          "type": event_types::TOOL_USE,
          "id": update.get("toolCallId").cloned().unwrap_or(Value::Null),
          "name": update.get("name").cloned().unwrap_or(Value::Null),
          "input": input,
        })]
      }
      Some("tool_call_update") => {
        let status = update.get("status").and_then(Value::as_str);
        if !matches!(status, Some("completed" | "failed")) {
          return Vec::new();
        }
        let output = update.pointer("/rawOutput/output").and_then(Value::as_str).unwrap_or_default();
        vec![json!({
          event_types::TOOL_USE_ID: update.get("toolCallId").cloned().unwrap_or(Value::Null),
          "type": event_types::TOOL_RESULT,
          "content": output,
          (event_types::IS_ERROR): status == Some("failed"),
        })]
      }
      _ => Vec::new(),
    }
  }

  /// The prompt id of the most recent prompt written, or `None` before the first one.
  fn latest_prompt(&self) -> Option<u64> {
    (self.next_id > FIRST_PROMPT_ID).then(|| self.next_id - 1)
  }

  /// Adds one response's usage. Every response is one request, so each is added once. `outputTokens` already includes
  /// `thoughtTokens`.
  fn add_usage(&mut self, raw: &Value) {
    let Some(usage) = raw.pointer("/result/usage") else {
      return;
    };
    let count = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
    self.usage.input += count("inputTokens");
    self.usage.output += count("outputTokens");
    self.usage.cache_read += count("cachedReadTokens");
    self.usage.cache_creation += count("cachedWriteTokens");
  }

  /// The turn's result event: the final answer and the summed usage.
  fn result_event(&self) -> Value {
    let answer = self.final_prompt.and_then(|id| self.texts.get(&id)).cloned().unwrap_or_default();
    let mut event = make_result_event(
      self.usage.input,
      self.usage.output,
      self.usage.cache_read,
      self.usage.cache_creation,
      json!(0),
      None,
    );
    event["result"] = json!(answer);
    if self.initial_succeeded != Some(true) {
      event[event_types::IS_ERROR] = json!(true);
    }
    event
  }
}

impl MidturnCodec for AcpCodec {
  fn encode_prompt(&mut self, prompt: &str) -> anyhow::Result<EncodedInput> {
    let id = self.next_id;
    let line = self.prompt_line(id, prompt)?;
    self.next_prompt();
    Ok(EncodedInput { key: id.to_string(), line })
  }

  fn encode_input(&mut self, _input_id: &str, content: &str) -> anyhow::Result<Option<EncodedInput>> {
    if self.ended {
      return Ok(None);
    }
    let id = self.next_id;
    let line = self.prompt_line(id, content)?;
    self.next_prompt();
    self.undelivered.push(id);
    Ok(Some(EncodedInput { key: id.to_string(), line }))
  }

  fn observe(&mut self, raw: &Value) -> anyhow::Result<Observed> {
    Ok(self.process(raw, true))
  }

  fn turn_may_end(&self) -> bool {
    self.outstanding.is_empty()
  }

  fn resume(&mut self, logged_keys: &[String], transport_dir: &Path, cursor: u64) -> anyhow::Result<()> {
    let ids = logged_keys
      .iter()
      .map(|key| key.parse::<u64>().with_context(|| format!("the input log names request {key}, not a JSON-RPC id")))
      .collect::<anyhow::Result<Vec<_>>>()?;
    let Some(&initial) = ids.first() else {
      return Ok(());
    };
    if initial != FIRST_PROMPT_ID {
      bail!("the input log's first request is {initial}, not the initial prompt {FIRST_PROMPT_ID}");
    }
    self.next_id = ids.iter().max().map_or(FIRST_PROMPT_ID, |max| (max + 1).max(FIRST_PROMPT_ID));
    self.outstanding = ids.iter().copied().collect();
    self.undelivered = ids.iter().skip(1).copied().collect();
    if cursor > 0 {
      self.replay_open = false;
    }
    // Output before the checkpoint was projected by the process that wrote it. Its requests and usage still count.
    for raw in read_output_before(&transport_dir.join(RAW_LOG_NAME), cursor)? {
      self.process(&raw, false);
    }
    Ok(())
  }
}

/// The error event of one JSON-RPC error object.
fn error_event(error: &Value) -> Value {
  let message = error.get("message").and_then(Value::as_str).map_or_else(|| error.to_string(), str::to_string);
  make_error_event(&message)
}

/// One JSON-RPC line, newline terminated.
pub fn line_bytes(value: &Value) -> Vec<u8> {
  let mut line = value.to_string().into_bytes();
  line.push(b'\n');
  line
}

/// The JSON objects of the complete lines of `path` before byte `cursor`. A missing file has none, and a line that is
/// not JSON is skipped.
pub fn read_output_before(path: &Path, cursor: u64) -> anyhow::Result<Vec<Value>> {
  let mut file = match std::fs::File::open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(error).with_context(|| format!("reading the agent output {}", path.display())),
  };
  let mut bytes = Vec::new();
  file
    .by_ref()
    .take(cursor)
    .read_to_end(&mut bytes)
    .with_context(|| format!("reading the agent output {}", path.display()))?;
  let complete = bytes.iter().rposition(|byte| *byte == b'\n').map_or(0, |end| end + 1);
  Ok(
    bytes[..complete]
      .split(|byte| *byte == b'\n')
      .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
      .collect(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  fn codec() -> AcpCodec {
    let session: SharedSession = Arc::new(Mutex::new(Some("session-1".to_string())));
    let mut codec = AcpCodec::new(session);
    codec.process(&json!({"jsonrpc": "2.0", "id": SESSION_ID, "result": {"sessionId": "session-1"}}), true);
    codec
  }

  fn response(id: u64, usage: Value) -> Value {
    json!({"jsonrpc": "2.0", "result": {"stopReason": "end_turn", "usage": usage}, "id": id})
  }

  fn chunk(text: &str) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "session-1",
      "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})
  }

  fn usage(input: i64, output: i64, cached: i64) -> Value {
    json!({"inputTokens": input, "outputTokens": output, "cachedReadTokens": cached, "thoughtTokens": 0,
      "cachedWriteTokens": 0, "totalTokens": input + output + cached})
  }

  #[test]
  fn a_message_is_delivered_by_the_next_answer_and_the_turn_ends_on_the_last() {
    let mut codec = codec();
    let initial = codec.encode_prompt("Run the steps").unwrap();
    assert_eq!(initial.key, "4");
    let message = codec.encode_input("m2", "Also BRAVO1").unwrap().expect("the turn takes the message");
    assert_eq!(message.key, "5");

    let observed = codec.observe(&chunk("final BRAVO1")).unwrap();
    assert_eq!(observed.events, vec![make_text_event("final BRAVO1")]);
    let answered_initial = codec.observe(&response(4, usage(100, 10, 5))).unwrap();
    assert_eq!(answered_initial.signals, vec![MidturnSignal::Delivered(vec!["5".to_string()])]);
    assert!(answered_initial.events.is_empty(), "the answer to the prompt that was superseded ends nothing");

    let last = codec.observe(&response(5, usage(200, 20, 7))).unwrap();
    assert_eq!(last.signals, vec![MidturnSignal::TurnEnded { succeeded: true }]);
    let result = &last.events[0];
    assert_eq!(result["type"], event_types::RESULT);
    assert_eq!(result["result"], "final BRAVO1");
    assert_eq!(result["usage"][event_types::USAGE_INPUT_TOKENS], 300, "each response counts once");
    assert_eq!(result["usage"][event_types::USAGE_OUTPUT_TOKENS], 30);
    assert_eq!(result["usage"][event_types::USAGE_CACHE_READ_INPUT_TOKENS], 12);
    assert!(codec.encode_input("late", "Too late").unwrap().is_none(), "no input after the turn end");
  }

  #[test]
  fn an_error_to_the_messages_own_request_leaves_the_message_for_the_next_turn() {
    let mut codec = codec();
    codec.encode_prompt("Run the steps").unwrap();
    codec.encode_input("m2", "Refused").unwrap().unwrap();
    let refused =
      codec.observe(&json!({"jsonrpc": "2.0", "id": 5, "error": {"code": -32000, "message": "nope"}})).unwrap();
    assert_eq!(refused.events, vec![make_error_event("nope")]);
    assert!(refused.signals.is_empty());

    let answered = codec.observe(&response(4, usage(1, 1, 0))).unwrap();
    assert_eq!(answered.signals, vec![MidturnSignal::TurnEnded { succeeded: true }], "no delivery after the error");
  }

  #[test]
  fn an_error_to_the_initial_prompt_ends_the_turn_as_a_failure() {
    let mut codec = codec();
    codec.encode_prompt("Run the steps").unwrap();
    let failed = codec.observe(&json!({"jsonrpc": "2.0", "id": 4, "error": {"message": "quota exhausted"}})).unwrap();
    assert_eq!(failed.signals, vec![MidturnSignal::TurnEnded { succeeded: false }]);
    assert_eq!(failed.events[0]["message"], "quota exhausted");
  }

  #[test]
  fn history_replayed_before_the_session_response_is_not_this_turn() {
    let session: SharedSession = Arc::new(Mutex::new(Some("session-1".to_string())));
    let mut codec = AcpCodec::new(session);
    let replayed = codec.observe(&chunk("old answer")).unwrap();
    assert!(replayed.events.is_empty());
    codec.process(&json!({"jsonrpc": "2.0", "id": SESSION_ID, "result": {"sessionId": "session-1"}}), true);
    codec.encode_prompt("Next").unwrap();
    let live = codec.observe(&chunk("new answer")).unwrap();
    assert_eq!(live.events, vec![make_text_event("new answer")]);
  }

  #[test]
  fn a_resumed_codec_rebuilds_its_requests_from_the_output_before_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let before = format!("{}\n", response(4, usage(10, 1, 0)));
    let after = format!("{}\n", response(5, usage(20, 2, 0)));
    std::fs::write(dir.path().join(RAW_LOG_NAME), format!("{before}{after}")).unwrap();

    let session: SharedSession = Arc::new(Mutex::new(Some("session-1".to_string())));
    let mut codec = AcpCodec::new(session);
    codec.resume(&["4".to_string(), "5".to_string()], dir.path(), before.len() as u64).unwrap();
    assert_eq!(codec.next_id, 6, "ids continue after the largest logged id");
    // The answer to the initial prompt came before the checkpoint, after message 5 was written: it delivered message 5,
    // and the process that wrote it projected that event.
    assert!(codec.undelivered.is_empty(), "the earlier answer delivered the message");
    let last = codec.observe(&response(5, usage(20, 2, 0))).unwrap();
    assert_eq!(last.signals, vec![MidturnSignal::TurnEnded { succeeded: true }]);
    assert_eq!(last.events[0]["usage"][event_types::USAGE_INPUT_TOKENS], 30, "the answer before the checkpoint counts");
  }

  #[test]
  fn output_tokens_are_not_double_counted_with_thought_tokens() {
    let mut codec = codec();
    codec.encode_prompt("Think").unwrap();
    let ended = codec
      .observe(&response(
        4,
        json!({"inputTokens": 100, "outputTokens": 196, "cachedReadTokens": 20, "thoughtTokens": 108,
          "cachedWriteTokens": 0, "totalTokens": 316}),
      ))
      .unwrap();
    assert_eq!(ended.events[0]["usage"][event_types::USAGE_OUTPUT_TOKENS], 196);
    assert_eq!(ended.events[0][event_types::IS_ERROR], Value::Null);
  }

  #[test]
  fn an_errored_initial_prompt_marks_the_result_as_error() {
    let mut codec = codec();
    codec.encode_prompt("Fail").unwrap();
    let failed = codec.observe(&json!({"jsonrpc": "2.0", "id": 4, "error": {"message": "bad"}})).unwrap();
    let result = failed.events.iter().find(|event| event["type"] == event_types::RESULT).unwrap();
    assert_eq!(result[event_types::IS_ERROR], json!(true));
  }

  #[test]
  fn resume_at_cursor_zero_discards_session_load_replay_until_session_response() {
    let dir = tempfile::tempdir().unwrap();
    let session: SharedSession = Arc::new(Mutex::new(Some("session-1".to_string())));
    let mut codec = AcpCodec::new(session);
    codec.resume(&["4".to_string()], dir.path(), 0).unwrap();
    assert!(codec.observe(&chunk("history replay")).unwrap().events.is_empty());
    codec.observe(&json!({"jsonrpc": "2.0", "id": SESSION_ID, "result": {"sessionId": "session-1"}})).unwrap();
    let live = codec.observe(&chunk("live answer")).unwrap();
    assert_eq!(live.events, vec![make_text_event("live answer")]);
  }
}
