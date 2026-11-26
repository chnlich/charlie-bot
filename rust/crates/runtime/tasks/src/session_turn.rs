//! The current turn's record: the events since the previous `master_done`, the inputs the running run received, and
//! the bounded text a `settle` cold read reads.
//!
//! The turn boundary and the last input are the ones the turn-end checks read: [`split_turn`] stamps `received_at` on
//! each input the running run's record names, and `turn_hooks::read_log` builds `status-reread`'s `TurnInput` through
//! it. [`turn_text`] renders the same turn as one line per event with its time, applying the shared cut and bound.

use charliebot_core::event_types::MASTER_DONE;
use charliebot_core::text_repr::{self, JsonStyle};
use charliebot_registry::FeatureContext;
use charliebot_store::SharedChatEvent;
use charliebot_store::metadata::UtcDateTime;
use charliebot_store::reply_order;
use serde_json::{Value, json};

/// The content characters one tool call or tool result keeps before it is cut.
pub const TURN_TEXT_CUT: usize = 1_000;
/// The characters a cut event keeps from its start.
pub const TURN_TEXT_HEAD: usize = 700;
/// The characters a cut event keeps from its end.
pub const TURN_TEXT_TAIL: usize = 300;
/// The most content characters the turn text carries; the bound drops the oldest tool events first.
pub const TURN_TEXT_BOUND: usize = 32_000;

/// The time prefix of one line: the first 19 characters of the event's timestamp.
const TIMESTAMP_CHARS: usize = 19;

/// The JSON style of a status payload and a tool input, as Python's `json.dumps` writes it.
const PYTHON_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

/// The session's current turn: the events since the previous `master_done`, with `received_at` on each input the
/// running run received.
pub fn current_turn(features: &FeatureContext, session_id: &str) -> anyhow::Result<Vec<Value>> {
  let loaded = features.store.load_chat_events_shared_sync(session_id)?;
  split_turn(features, session_id, loaded.as_slice())
}

/// [`current_turn`] over a log the caller already loaded.
pub fn split_turn(
  features: &FeatureContext,
  session_id: &str,
  loaded: &[SharedChatEvent],
) -> anyhow::Result<Vec<Value>> {
  let receipts = reply_order::running_receipts(&features.store, session_id, loaded)?;
  let mut events: Vec<Value> = loaded.iter().map(|event| event.as_ref().to_value()).collect();
  let start = events.iter().rposition(|event| event["type"] == MASTER_DONE).map_or(0, |index| index + 1);
  let mut turn = events.split_off(start);
  for event in &mut turn {
    let Some(id) = event["id"].as_str() else { continue };
    if let Some(receipt) = receipts.iter().find(|receipt| receipt.input_event_id == id) {
      event["received_at"] = json!(receipt.at.to_str());
    }
  }
  Ok(turn)
}

/// The moment of the turn's last input: the last event `received_at` the running run stamped. A turn with no input
/// event has no moment, and its checks look back to the previous `master_done` instead.
pub fn last_input_at(turn: &[Value]) -> Option<UtcDateTime> {
  let index = crate::turn_hooks::last_received_index(turn)?;
  UtcDateTime::parse(turn[index]["received_at"].as_str()?).ok()
}

/// The kind of one rendered entry. A tool entry is droppable when the text exceeds the bound.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
  Tool,
  Core,
}

/// One rendered entry: its kind, the `[time] kind` prefix, its body, and the body's content characters.
struct Entry {
  kind: Kind,
  prefix: String,
  body: String,
  content: usize,
}

impl Entry {
  fn new(kind: Kind, prefix: String, body: String) -> Self {
    let content = body.chars().count();
    Self { kind, prefix, body, content }
  }

  /// The tool entry with its body cut to the head and tail when it is over the cut length.
  fn cut_tool(prefix: String, body: String) -> Self {
    let (body, content) = cut(&body);
    Self { kind: Kind::Tool, prefix, body, content }
  }

  fn line(&self) -> String {
    format!("{}{}", self.prefix, self.body)
  }
}

/// The body cut to its first 700 and last 300 characters with the cut size marked, and the remaining content
/// characters. A body within the cut length stays whole.
fn cut(body: &str) -> (String, usize) {
  let chars: Vec<char> = body.chars().collect();
  if chars.len() <= TURN_TEXT_CUT {
    return (body.to_string(), chars.len());
  }
  let head: String = chars[..TURN_TEXT_HEAD].iter().collect();
  let tail: String = chars[chars.len() - TURN_TEXT_TAIL..].iter().collect();
  let removed = chars.len() - TURN_TEXT_HEAD - TURN_TEXT_TAIL;
  (format!("{head}[… {removed} chars cut …]{tail}"), TURN_TEXT_HEAD + TURN_TEXT_TAIL)
}

/// The `[time] ` prefix of one event's lines.
fn time_prefix(event: &Value) -> String {
  let timestamp = event["timestamp"].as_str().unwrap_or_default();
  let at = timestamp.chars().take(TIMESTAMP_CHARS).collect::<String>();
  format!("[{at}] ")
}

/// The turn's text: one line per event with its time. The inputs that woke the turn, the agent's text, its tool calls
/// and their results, its web replies, each stored status payload, and the status-confirmation and close-request
/// control events are kept; `turn_hooks`, thinking, `system`, telemetry events and cold-read records are left out. The
/// bound cuts a long tool call or result and drops the oldest tool entries until the text fits, then cuts the retained
/// entries the same way when they alone overflow it.
pub fn turn_text(turn: &[Value]) -> String {
  let lines = bound(turn_entries(turn));
  lines.join("\n")
}

/// The turn's entries, chronological, with each tool body already cut.
fn turn_entries(turn: &[Value]) -> Vec<Entry> {
  let mut entries = Vec::new();
  for event in turn {
    let at = time_prefix(event);
    let event_type = event["type"].as_str().unwrap_or_default();
    if event.get("received_at").is_some_and(Value::is_string) {
      entries.push(Entry::new(Kind::Core, format!("{at}{event_type}: "), input_body(event)));
      continue;
    }
    match event_type {
      "assistant" => {
        for block in blocks(event) {
          match block["type"].as_str() {
            Some("text") => {
              if let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) {
                entries.push(Entry::new(Kind::Core, format!("{at}agent: "), text.to_string()));
              }
            }
            Some("tool_use") => {
              let name = block["name"].as_str().unwrap_or_default();
              let input = text_repr::json_dumps(&block["input"], &PYTHON_JSON);
              entries.push(Entry::cut_tool(format!("{at}tool call {name}: "), input));
            }
            _ => {}
          }
        }
      }
      "user" => {
        for block in blocks(event) {
          if block["type"] == "tool_result" {
            entries.push(Entry::cut_tool(format!("{at}tool result: "), text_of(&block["content"])));
          }
        }
      }
      "web_reply" => entries.push(Entry::new(
        Kind::Core,
        format!("{at}web reply: "),
        event["content"].as_str().unwrap_or_default().to_string(),
      )),
      "web_status" => {
        let payload = text_repr::json_dumps(&event["web_status"], &PYTHON_JSON);
        let changed = text_repr::json_dumps(&event["web_status_changed"], &PYTHON_JSON);
        entries.push(Entry::new(Kind::Core, format!("{at}web_status "), format!("{payload} changed {changed}")));
      }
      "web_status_confirmed" | "task_close_requested" => {
        entries.push(Entry::new(Kind::Core, at, event_type.to_string()));
      }
      _ => {}
    }
  }
  entries
}

/// The text an input event carries: its content, or a child report's summary.
fn input_body(event: &Value) -> String {
  if let Some(content) = event["content"].as_str() {
    return content.to_string();
  }
  if let Some(summary) = event["summary"].as_str() {
    return summary.to_string();
  }
  String::new()
}

/// The content blocks of an assistant or user event.
fn blocks(event: &Value) -> &[Value] {
  match event["message"]["content"].as_array() {
    Some(blocks) => blocks,
    None => &[],
  }
}

/// The text of one tool result's content: a string, the joined texts of its blocks, or its JSON.
fn text_of(content: &Value) -> String {
  match content {
    Value::String(text) => text.clone(),
    Value::Array(items) => items.iter().filter_map(|item| item["text"].as_str()).collect::<Vec<_>>().join(""),
    other => text_repr::json_dumps(other, &PYTHON_JSON),
  }
}

/// The entries under the bound: drop the oldest tool entries until the content fits, then cut the retained entries
/// over the cut length when they alone still overflow it. A dropped run is marked at the top.
fn bound(mut entries: Vec<Entry>) -> Vec<String> {
  let mut size: usize = entries.iter().map(|entry| entry.content).sum();
  let mut dropped = 0;
  let mut dropped_chars = 0;
  while size > TURN_TEXT_BOUND {
    let Some(index) = entries.iter().position(|entry| entry.kind == Kind::Tool) else { break };
    size -= entries[index].content;
    dropped_chars += entries[index].content;
    dropped += 1;
    entries.remove(index);
  }
  if size > TURN_TEXT_BOUND {
    for entry in entries.iter_mut().filter(|entry| entry.kind == Kind::Core) {
      if entry.body.chars().count() > TURN_TEXT_CUT {
        let (body, content) = cut(&entry.body);
        entry.body = body;
        entry.content = content;
      }
    }
  }
  let mut lines = Vec::new();
  if dropped > 0 {
    lines.push(format!("[{dropped} oldest tool events cut, {dropped_chars} content chars]"));
  }
  lines.extend(entries.iter().map(Entry::line));
  lines
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn turn(events: Vec<Value>) -> Vec<Value> {
    events
      .into_iter()
      .enumerate()
      .map(|(index, mut event)| {
        event["timestamp"] = json!(format!("2026-01-01T00:00:{index:02}Z"));
        event
      })
      .collect()
  }

  fn user(content: &str) -> Value {
    json!({"type": "user", "id": "u1", "content": content, "received_at": "2026-01-01T00:00:00Z"})
  }

  #[test]
  fn the_text_keeps_the_inputs_and_drops_the_hook_thinking_system_and_cold_read_lines() {
    let events = turn(vec![
      user("the question"),
      json!({"type": "thinking", "id": "t1", "thinking": "hmm"}),
      json!({"type": "system", "id": "s1", "content": "telemetry"}),
      json!({"type": "turn_hooks", "id": "h1", "results": []}),
      json!({"type": "cold_read_check", "id": "c1", "name": "reply"}),
      json!({"type": "web_reply_check", "id": "c2"}),
      json!({"type": "web_ask_check", "id": "c3"}),
      json!({"type": "assistant", "id": "a1", "message": {"content": [{"type": "text", "text": "the answer"}]}}),
    ]);

    assert_eq!(turn_text(&events), "[2026-01-01T00:00:00] user: the question\n[2026-01-01T00:00:07] agent: the answer");
  }

  #[test]
  fn a_tool_call_and_a_tool_result_render_with_their_time() {
    let events = turn(vec![
      user("go"),
      json!({"type": "assistant", "id": "a1", "message": {"content": [
        {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
      ]}}),
      json!({"type": "user", "id": "u2", "message": {"content": [
        {"type": "tool_result", "content": [{"type": "text", "text": "a\nb"}]},
      ]}}),
    ]);

    let text = turn_text(&events);
    assert!(text.contains("[2026-01-01T00:00:01] tool call Bash: {\"command\": \"ls\"}"), "{text}");
    assert!(text.contains("[2026-01-01T00:00:02] tool result: a\nb"), "{text}");
  }

  #[test]
  fn a_long_tool_body_is_cut_to_its_head_and_tail_with_the_size_marked() {
    let body = "a".repeat(1_500);
    let event = json!({"type": "assistant", "id": "a1", "timestamp": "2026-01-01T00:00:00Z", "message": {"content": [
      {"type": "tool_use", "name": "Read", "input": body},
    ]}});
    let rendered = text_repr::json_dumps(&json!(body), &PYTHON_JSON);
    let removed = rendered.chars().count() - TURN_TEXT_CUT;
    let text = turn_text(&[event]);
    assert!(text.contains(&format!("[… {removed} chars cut …]")), "{text}");
    assert!(text.contains(&"a".repeat(TURN_TEXT_HEAD - 1)), "the head keeps 699 of the 1,500 characters: {text}");
    assert!(text.contains(&"a".repeat(TURN_TEXT_TAIL)), "the tail keeps its 300 characters: {text}");
    assert!(!text.contains(&"a".repeat(TURN_TEXT_CUT)), "no run reaches the 1,000-character cut: {text}");
  }

  #[test]
  fn the_bound_drops_the_oldest_tool_entries_first() {
    let mut events = vec![user("go")];
    for index in 0..40 {
      events.push(json!({"type": "user", "id": format!("r{index}"), "message": {"content": [
        {"type": "tool_result", "content": [{"type": "text", "text": "x".repeat(900)}]},
      ]}}));
    }
    let events = turn(events);
    let text = turn_text(&events);
    assert!(text.starts_with("[5 oldest tool events cut, 4500 content chars]"), "{text}");
    assert!(text.contains("user: go"), "the input stays");
  }

  #[test]
  fn the_bound_cuts_the_retained_entries_only_when_they_alone_overflow_it() {
    let mut events = vec![user(&"q".repeat(33_000))];
    events.push(json!({"type": "user", "id": "r0", "message": {"content": [
      {"type": "tool_result", "content": [{"type": "text", "text": "x".repeat(900)}]},
    ]}}));
    let text = turn_text(&turn(events));
    assert!(text.starts_with('['), "the dropped tool run is marked first: {text}");
    assert!(text.contains("[… "), "the retained input is cut: {text}");
  }

  #[test]
  fn a_status_payload_renders_as_its_stored_json() {
    let events = turn(vec![json!({"type": "web_status", "id": "s1", "web_status": {"goal": "Ship", "now": null},
      "web_status_changed": ["goal"]})]);
    let text = turn_text(&events);
    assert!(text.contains(r#"web_status {"goal": "Ship", "now": null} changed ["goal"]"#), "{text}");
  }

  #[test]
  fn the_last_input_time_is_the_last_received_stamp() {
    let events = turn(vec![
      user("first"),
      json!({"type": "child_report", "id": "c1", "summary": "done", "received_at": "2026-01-01T00:00:05Z"}),
    ]);
    assert_eq!(last_input_at(&events).unwrap().to_str(), "2026-01-01T00:00:05Z");
    assert!(last_input_at(&[json!({"type": "assistant", "id": "a1"})]).is_none());
  }
}
