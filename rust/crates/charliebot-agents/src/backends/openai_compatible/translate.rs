//! The translations of `src/backends/openai_compatible/anthropic_proxy.py`: an Anthropic Messages request to an OpenAI
//! chat completions request, an OpenAI answer to an Anthropic message, and the OpenAI chat stream to Anthropic's
//! events. Each keeps Python's operation order, so a request raises the failure Python raises first.

use charliebot_core::pyfmt;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::pyvalue::{
  COMPACT_ASCII_JSON, PyResult, first_item, get, get_or_empty, iterate, iterate_member, loads_text, raised,
  require_dict, truthy, type_name, value_error,
};

/// One Anthropic stream event: its name, and the data object the SSE frame carries after `type`.
pub type Event = (&'static str, Value);

/// Whether `api_base` names the Google Gemini endpoint that uses Gemini stream rules.
pub fn gemini_stream_rules(api_base: &str) -> Result<bool, String> {
  let url = reqwest::Url::parse(api_base).map_err(|error| error.to_string())?;
  Ok(url.host_str() == Some("generativelanguage.googleapis.com"))
}

/// `anthropic_to_openai_chat_request(payload, upstream_model)`: the chat completions body for `payload`.
pub fn anthropic_to_openai_chat_request(
  payload: &Value,
  upstream_model: &str,
  _gemini_stream_rules: bool,
) -> PyResult<Value> {
  let messages = match get(payload, "messages")? {
    Some(Value::Array(messages)) => messages,
    _ => return value_error("messages must be a list"),
  };
  let mut converted_messages = Vec::new();
  if let Some(system) = get(payload, "system")?.filter(|system| truthy(system)) {
    converted_messages.push(json!({"role": "system", "content": blocks_to_text(system, "system")?}));
  }
  for raw in messages {
    let message = require_dict(raw, "message")?;
    match message.get("role").and_then(Value::as_str) {
      Some("user") => converted_messages.extend(convert_user_message(message)?),
      Some("assistant") => converted_messages.push(convert_assistant_message(message)?),
      _ => return value_error(format!("unsupported message role: {}", py_str(message.get("role"), "None"))),
    }
  }

  let stream = get(payload, "stream")?.is_some_and(truthy);
  let mut converted = Map::new();
  converted.insert("model".to_string(), json!(upstream_model));
  converted.insert("messages".to_string(), Value::Array(converted_messages));
  converted.insert("stream".to_string(), json!(stream));
  for (source, target) in
    [("max_tokens", "max_tokens"), ("temperature", "temperature"), ("top_p", "top_p"), ("stop_sequences", "stop")]
  {
    if let Some(value) = get(payload, source)?.filter(|value| !value.is_null()) {
      converted.insert(target.to_string(), value.clone());
    }
  }
  if let Some(tools) = get(payload, "tools")?.filter(|tools| !tools.is_null()) {
    converted.insert("tools".to_string(), Value::Array(convert_tools(tools)?));
  }
  if let Some(choice) = get(payload, "tool_choice")?.filter(|choice| !choice.is_null()) {
    let choice = require_dict(choice, "tool_choice")?;
    converted.insert("tool_choice".to_string(), convert_tool_choice(choice)?);
    if let Some(disable) = choice.get("disable_parallel_tool_use").filter(|disable| !disable.is_null()) {
      converted.insert("parallel_tool_calls".to_string(), json!(!truthy(disable)));
    }
  }
  if stream {
    converted.insert("stream_options".to_string(), json!({"include_usage": true}));
  }
  // The upstream's reasoning field is not an Anthropic thinking block and carries no signature, so thinking is refused.
  if get(payload, "thinking")?.is_some_and(|thinking| !thinking.is_null()) {
    return value_error("OpenAI-compatible proxy does not support Anthropic thinking blocks");
  }
  if get(payload, "top_k")?.is_some_and(|top_k| !top_k.is_null()) {
    return value_error("OpenAI-compatible proxy does not support Anthropic top_k");
  }
  Ok(Value::Object(converted))
}

/// `str(payload.get(key, missing))` for a member that may be absent.
fn py_str(value: Option<&Value>, missing: &str) -> String {
  value.map_or_else(|| missing.to_string(), pyfmt::python_str)
}

/// `_blocks_to_text(content, label)`: a string as it is, or the text of a list of text blocks.
fn blocks_to_text(content: &Value, label: &str) -> PyResult<String> {
  match content {
    Value::String(text) => Ok(text.clone()),
    Value::Array(blocks) => {
      let mut text = String::new();
      for raw in blocks {
        let block = require_dict(raw, &format!("{label} block"))?;
        if block.get("type").and_then(Value::as_str) != Some("text") {
          return value_error(format!("unsupported {label} block type: {}", py_str(block.get("type"), "None")));
        }
        text.push_str(&py_str(block.get("text"), ""));
      }
      Ok(text)
    }
    _ => value_error(format!("{label} must be a string or content block list")),
  }
}

fn flush_user_text(converted: &mut Vec<Value>, parts: &mut Vec<String>) {
  if !parts.is_empty() {
    converted.push(json!({"role": "user", "content": parts.concat()}));
    parts.clear();
  }
}

/// `_convert_user_message`: a string stays one user message; blocks split at each tool result.
fn convert_user_message(message: &Map<String, Value>) -> PyResult<Vec<Value>> {
  let blocks = match message.get("content") {
    None => return Ok(vec![json!({"role": "user", "content": ""})]),
    Some(Value::String(text)) => return Ok(vec![json!({"role": "user", "content": text})]),
    Some(Value::Array(blocks)) => blocks,
    Some(_) => return value_error("user message content must be a string or content block list"),
  };
  let mut converted = Vec::new();
  let mut text_parts = Vec::new();
  for raw in blocks {
    let block = require_dict(raw, "user content block")?;
    match block.get("type").and_then(Value::as_str) {
      Some("text") => text_parts.push(py_str(block.get("text"), "")),
      Some("tool_result") => {
        flush_user_text(&mut converted, &mut text_parts);
        let Some(tool_use_id) = block.get("tool_use_id").filter(|id| truthy(id)) else {
          return value_error("tool_result block missing tool_use_id");
        };
        let content = match block.get("content") {
          None => String::new(),
          Some(content) => blocks_to_text(content, "tool_result content")?,
        };
        converted.push(json!({"role": "tool", "tool_call_id": tool_use_id, "content": content}));
      }
      _ => return value_error(format!("unsupported user content block type: {}", py_str(block.get("type"), "None"))),
    }
  }
  flush_user_text(&mut converted, &mut text_parts);
  Ok(converted)
}

/// `_convert_assistant_message`: the text joins into `content` (null when empty), and each tool use becomes a call.
fn convert_assistant_message(message: &Map<String, Value>) -> PyResult<Value> {
  let blocks = match message.get("content") {
    None => return Ok(json!({"role": "assistant", "content": ""})),
    Some(Value::String(text)) => return Ok(json!({"role": "assistant", "content": text})),
    Some(Value::Array(blocks)) => blocks,
    Some(_) => return value_error("assistant message content must be a string or content block list"),
  };
  let mut text = String::new();
  let mut tool_calls = Vec::new();
  for raw in blocks {
    let block = require_dict(raw, "assistant content block")?;
    match block.get("type").and_then(Value::as_str) {
      Some("text") => text.push_str(&py_str(block.get("text"), "")),
      Some("tool_use") => {
        let id = block.get("id").filter(|id| truthy(id));
        let name = block.get("name").filter(|name| truthy(name));
        let (Some(id), Some(name)) = (id, name) else {
          return value_error("tool_use block missing id or name");
        };
        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
        tool_calls.push(json!({
          "id": id,
          "type": "function",
          "function": {"name": name, "arguments": pyfmt::json_dumps(&input, &COMPACT_ASCII_JSON)},
        }));
      }
      _ => {
        return value_error(format!("unsupported assistant content block type: {}", py_str(block.get("type"), "None")));
      }
    }
  }
  let mut converted = Map::new();
  converted.insert("role".to_string(), json!("assistant"));
  converted.insert("content".to_string(), if text.is_empty() { Value::Null } else { Value::String(text) });
  if !tool_calls.is_empty() {
    converted.insert("tool_calls".to_string(), Value::Array(tool_calls));
  }
  Ok(Value::Object(converted))
}

/// `_convert_tools`: each custom tool becomes a function. A built-in tool type is refused.
fn convert_tools(tools: &Value) -> PyResult<Vec<Value>> {
  let mut converted = Vec::new();
  for raw in iterate(tools)? {
    let tool = require_dict(&raw, "tool")?;
    if let Some(kind) = tool.get("type").filter(|kind| truthy(kind)) {
      return value_error(format!("unsupported Anthropic built-in tool type: {}", pyfmt::python_str(kind)));
    }
    let name = tool.get("name").filter(|name| truthy(name));
    let input_schema = tool.get("input_schema").filter(|schema| !schema.is_null());
    let (Some(name), Some(input_schema)) = (name, input_schema) else {
      return value_error("tool missing name or input_schema");
    };
    converted.push(json!({
      "type": "function",
      "function": {
        "name": name,
        "description": tool.get("description").cloned().unwrap_or_else(|| json!("")),
        "parameters": input_schema,
      },
    }));
  }
  Ok(converted)
}

/// `_convert_tool_choice`: `auto`, `any` (required), `tool` (a named function) and `none`.
fn convert_tool_choice(choice: &Map<String, Value>) -> PyResult<Value> {
  match choice.get("type").and_then(Value::as_str) {
    Some("auto") => Ok(json!("auto")),
    Some("any") => Ok(json!("required")),
    Some("tool") => match choice.get("name").filter(|name| truthy(name)) {
      Some(name) => Ok(json!({"type": "function", "function": {"name": name}})),
      None => value_error("tool_choice type 'tool' missing name"),
    },
    Some("none") => Ok(json!("none")),
    _ => value_error(format!("unsupported tool_choice type: {}", py_str(choice.get("type"), "None"))),
  }
}

/// `_stop_reason(finish_reason)`: the Anthropic reason for an OpenAI one. Any other value passes through as it is.
fn stop_reason(finish_reason: Option<&Value>) -> Value {
  match finish_reason {
    None | Some(Value::Null) => json!("end_turn"),
    Some(Value::String(reason)) if reason == "tool_calls" => json!("tool_use"),
    Some(Value::String(reason)) if reason == "length" => json!("max_tokens"),
    Some(Value::String(reason)) if reason == "stop" => json!("end_turn"),
    Some(other) => other.clone(),
  }
}

/// `_usage_from_openai(usage)`: the prompt and completion counts, and zero cache counts.
pub fn usage_from_openai(usage: Option<&Value>) -> PyResult<Value> {
  let usage = usage.filter(|usage| truthy(usage)).cloned().unwrap_or_else(|| json!({}));
  Ok(json!({
    "input_tokens": get(&usage, "prompt_tokens")?.cloned().unwrap_or_else(|| json!(0)),
    "output_tokens": get(&usage, "completion_tokens")?.cloned().unwrap_or_else(|| json!(0)),
    "cache_creation_input_tokens": 0,
    "cache_read_input_tokens": 0,
  }))
}

/// `f"msg_{uuid.uuid4().hex}"`: the id Python gives a message the upstream did not name.
pub fn new_message_id() -> String {
  format!("msg_{}", Uuid::new_v4().simple())
}

/// `openai_chat_response_to_anthropic(response, requested_model)`: the Anthropic message for a non-streaming answer.
pub fn openai_chat_response_to_anthropic(response: &Value, requested_model: &str) -> PyResult<Value> {
  let choices = match get(response, "choices")?.filter(|choices| truthy(choices)) {
    Some(choices) => choices,
    None => return value_error("OpenAI response missing choices"),
  };
  let choice = first_item(choices)?;
  let message = get_or_empty(&choice, "message")?;
  let mut content_blocks = Vec::new();
  if let Some(content) = get(&message, "content")?.filter(|content| truthy(content)) {
    content_blocks.push(json!({"type": "text", "text": content}));
  }
  for tool_call in iterate_member(get(&message, "tool_calls")?)? {
    let function = get_or_empty(&tool_call, "function")?;
    let tool_id = get(&tool_call, "id")?.filter(|id| truthy(id));
    let name = get(&function, "name")?.filter(|name| truthy(name));
    let (Some(tool_id), Some(name)) = (tool_id, name) else {
      return value_error("OpenAI tool_call missing id or function.name");
    };
    let arguments = get(&function, "arguments")?.filter(|arguments| truthy(arguments)).cloned();
    let input = match arguments.unwrap_or_else(|| json!("{}")) {
      Value::String(text) => loads_text(&text)?,
      other => {
        return raised(
          "TypeError",
          format!("the JSON object must be str, bytes or bytearray, not {}", type_name(&other)),
        );
      }
    };
    content_blocks.push(json!({"type": "tool_use", "id": tool_id, "name": name, "input": input}));
  }

  let id = get(response, "id")?.filter(|id| truthy(id)).cloned().unwrap_or_else(|| json!(new_message_id()));
  let model = get(response, "model")?.filter(|model| truthy(model)).cloned().unwrap_or_else(|| json!(requested_model));
  let reason = stop_reason(get(&choice, "finish_reason")?);
  let usage = usage_from_openai(get(response, "usage")?)?;
  Ok(json!({
    "id": id,
    "type": "message",
    "role": "assistant",
    "model": model,
    "content": content_blocks,
    "stop_reason": reason,
    "stop_sequence": null,
    "usage": usage,
  }))
}

/// `OpenAIChatStreamToAnthropic`: the stateful translation of the OpenAI chat chunks to Anthropic's stream events.
pub struct OpenAiChatStream {
  message_id: String,
  model: String,
  next_index: u64,
  text_index: Option<u64>,
  /// The open tool blocks in opening order, each with the call index the upstream gave it.
  tool_indexes: Vec<(Value, u64)>,
  usage: Value,
  finish_reason: Option<Value>,
}

impl OpenAiChatStream {
  pub fn new(model: &str, _gemini_stream_rules: bool) -> Self {
    Self {
      message_id: new_message_id(),
      model: model.to_string(),
      next_index: 0,
      text_index: None,
      tool_indexes: Vec::new(),
      usage: json!({}),
      finish_reason: None,
    }
  }

  pub fn start_events(&self) -> Vec<Event> {
    vec![(
      "message_start",
      json!({
        "message": {
          "id": self.message_id,
          "type": "message",
          "role": "assistant",
          "content": [],
          "model": self.model,
          "stop_reason": null,
          "stop_sequence": null,
          "usage": {
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
          },
        },
      }),
    )]
  }

  /// `events_for_chunk(chunk)`: the events one upstream chunk completes.
  pub fn events_for_chunk(&mut self, chunk: &Value) -> PyResult<Vec<Event>> {
    let mut events = Vec::new();
    if let Some(usage) = get(chunk, "usage")?.filter(|usage| truthy(usage)) {
      self.usage = usage.clone();
    }
    for choice in iterate_member(get(chunk, "choices")?)? {
      let delta = get_or_empty(&choice, "delta")?;
      if let Some(content) = get(&delta, "content")?.filter(|content| truthy(content)).cloned() {
        let index = self.open_text_block(&mut events);
        events.push(("content_block_delta", json!({"index": index, "delta": {"type": "text_delta", "text": content}})));
      }
      for tool_call in iterate_member(get(&delta, "tool_calls")?)? {
        self.tool_delta(&tool_call, &mut events)?;
      }
      if let Some(reason) = get(&choice, "finish_reason")?.filter(|reason| truthy(reason)) {
        self.finish_reason = Some(reason.clone());
      }
    }
    Ok(events)
  }

  /// `finish_events()`: closes the open blocks, then the message's delta and its stop.
  pub fn finish_events(&mut self) -> PyResult<Vec<Event>> {
    let mut events = Vec::new();
    self.close_text_block(&mut events);
    self.close_tool_blocks(&mut events);
    let usage = usage_from_openai(Some(&self.usage))?;
    events.push((
      "message_delta",
      json!({"delta": {"stop_reason": stop_reason(self.finish_reason.as_ref()), "stop_sequence": null}, "usage": usage}),
    ));
    events.push(("message_stop", json!({})));
    Ok(events)
  }

  fn take_index(&mut self) -> u64 {
    let index = self.next_index;
    self.next_index += 1;
    index
  }

  /// `_ensure_text_block`: the open text block's index, opening one (and closing the tool blocks) when none is open.
  fn open_text_block(&mut self, events: &mut Vec<Event>) -> u64 {
    if let Some(index) = self.text_index {
      return index;
    }
    self.close_tool_blocks(events);
    let index = self.take_index();
    self.text_index = Some(index);
    events.push(("content_block_start", json!({"index": index, "content_block": {"type": "text", "text": ""}})));
    index
  }

  fn close_text_block(&mut self, events: &mut Vec<Event>) {
    if let Some(index) = self.text_index.take() {
      events.push(("content_block_stop", json!({"index": index})));
    }
  }

  fn close_tool_blocks(&mut self, events: &mut Vec<Event>) {
    for (_, index) in self.tool_indexes.drain(..) {
      events.push(("content_block_stop", json!({"index": index})));
    }
  }

  /// `_handle_tool_delta`: a call's first delta opens its block; later deltas carry its arguments.
  fn tool_delta(&mut self, tool_call: &Value, events: &mut Vec<Event>) -> PyResult<()> {
    let call_index = match get(tool_call, "index")? {
      None | Some(Value::Null) => return value_error("streaming tool_call missing index"),
      Some(index) => index.clone(),
    };
    let function = get_or_empty(tool_call, "function")?;
    if matches!(call_index, Value::Array(_) | Value::Object(_)) {
      return raised("TypeError", format!("unhashable type: '{}'", type_name(&call_index)));
    }
    if !self.tool_indexes.iter().any(|(known, _)| known == &call_index) {
      let tool_id = get(tool_call, "id")?.filter(|id| truthy(id)).cloned();
      let name = get(&function, "name")?.filter(|name| truthy(name)).cloned();
      let (Some(tool_id), Some(name)) = (tool_id, name) else {
        return value_error("first streaming tool_call delta missing id or function.name");
      };
      self.close_text_block(events);
      let block_index = self.take_index();
      self.tool_indexes.push((call_index.clone(), block_index));
      events.push((
        "content_block_start",
        json!({
          "index": block_index,
          "content_block": {"type": "tool_use", "id": tool_id, "name": name, "input": {}},
        }),
      ));
    }
    if let Some(arguments) = get(&function, "arguments")?.filter(|arguments| !arguments.is_null()) {
      let index = self
        .tool_indexes
        .iter()
        .find(|(known, _)| known == &call_index)
        .map(|(_, index)| *index)
        .expect("the call's block is open once its first delta has run");
      events.push((
        "content_block_delta",
        json!({"index": index, "delta": {"type": "input_json_delta", "partial_json": arguments}}),
      ));
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;
  use crate::backends::openai_compatible::pyvalue::Fault;

  fn request(payload: Value) -> PyResult<Value> {
    anthropic_to_openai_chat_request(&payload, "upstream-model", false)
  }

  #[test]
  fn a_non_object_payload_raises_the_attribute_error_python_raises() {
    assert_eq!(
      request(json!([1])).unwrap_err(),
      Fault::Raised { kind: "AttributeError", message: "'list' object has no attribute 'get'".to_string() }
    );
  }

  #[test]
  fn an_unhashable_call_index_is_a_type_error() {
    let mut stream = OpenAiChatStream::new("model", false);
    let chunk = json!({"choices": [{"delta": {"tool_calls": [{"index": [0], "id": "c", "function": {"name": "n"}}]}}]});
    assert_eq!(
      stream.events_for_chunk(&chunk).unwrap_err(),
      Fault::Raised { kind: "TypeError", message: "unhashable type: 'list'".to_string() }
    );
  }

  #[test]
  fn a_missing_message_role_names_none() {
    assert_eq!(
      request(json!({"messages": [{"content": "x"}]})).unwrap_err(),
      Fault::Value("unsupported message role: None".to_string())
    );
  }

  #[test]
  fn a_request_maps_text_tools_and_tool_results_to_openai() {
    let payload = json!({
      "model": "synthetic-provider/synthetic-vendor/Synthetic-Pro",
      "system": [{"type": "text", "text": "Use tools carefully."}],
      "messages": [
        {"role": "user", "content": "List files."},
        {"role": "assistant", "content": [
          {"type": "text", "text": "I will inspect the directory."},
          {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"cmd": "ls"}},
        ]},
        {"role": "user", "content": [
          {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "README.md"}]},
          {"type": "text", "text": "Summarize."},
        ]},
      ],
      "max_tokens": 1024,
      "temperature": 0.1,
      "stop_sequences": ["</stop>"],
      "tools": [{
        "name": "Bash",
        "description": "Run a shell command",
        "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]},
      }],
      "tool_choice": {"type": "any", "disable_parallel_tool_use": true},
      "stream": true,
    });

    let converted = anthropic_to_openai_chat_request(&payload, "deepseek-ai/DeepSeek-V4-Pro", false).unwrap();

    assert_eq!(converted["model"], "deepseek-ai/DeepSeek-V4-Pro");
    assert_eq!(converted["stream"], true);
    assert_eq!(converted["stream_options"], json!({"include_usage": true}));
    assert_eq!(converted["max_tokens"], 1024);
    assert_eq!(converted["temperature"], 0.1);
    assert_eq!(converted["stop"], json!(["</stop>"]));
    assert_eq!(converted["tool_choice"], "required");
    assert_eq!(converted["parallel_tool_calls"], false);
    assert_eq!(
      converted["messages"],
      json!([
        {"role": "system", "content": "Use tools carefully."},
        {"role": "user", "content": "List files."},
        {
          "role": "assistant",
          "content": "I will inspect the directory.",
          "tool_calls": [{
            "id": "toolu_1",
            "type": "function",
            "function": {"name": "Bash", "arguments": "{\"cmd\":\"ls\"}"},
          }],
        },
        {"role": "tool", "tool_call_id": "toolu_1", "content": "README.md"},
        {"role": "user", "content": "Summarize."},
      ])
    );
    assert_eq!(
      converted["tools"],
      json!([{
        "type": "function",
        "function": {
          "name": "Bash",
          "description": "Run a shell command",
          "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]},
        },
      }])
    );
  }

  #[test]
  fn the_stream_translator_emits_text_and_tool_events_with_the_stop_reason() {
    let mut stream = OpenAiChatStream::new("deepseek-v4-pro", false);
    let mut events = stream.start_events();
    events.extend(stream.events_for_chunk(&json!({"choices": [{"delta": {"content": "Checking"}}]})).unwrap());
    events.extend(
      stream
        .events_for_chunk(&json!({"choices": [{"delta": {"tool_calls": [{
          "index": 0,
          "id": "call_1",
          "type": "function",
          "function": {"name": "Bash", "arguments": "{\"cmd\""},
        }]}}]}))
        .unwrap(),
    );
    events.extend(
      stream
        .events_for_chunk(&json!({
          "choices": [{
            "delta": {"tool_calls": [{"index": 0, "function": {"arguments": ":\"pwd\"}"}}]},
            "finish_reason": "tool_calls",
          }],
          "usage": {"prompt_tokens": 8, "completion_tokens": 3},
        }))
        .unwrap(),
    );
    events.extend(stream.finish_events().unwrap());

    let names: Vec<&str> = events.iter().map(|(name, _)| *name).collect();
    assert_eq!(
      names,
      [
        "message_start",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "content_block_start",
        "content_block_delta",
        "content_block_delta",
        "content_block_stop",
        "message_delta",
        "message_stop",
      ]
    );
    assert_eq!(events[1].1["content_block"], json!({"type": "text", "text": ""}));
    assert_eq!(events[2].1["delta"], json!({"type": "text_delta", "text": "Checking"}));
    assert_eq!(events[4].1["content_block"], json!({"type": "tool_use", "id": "call_1", "name": "Bash", "input": {}}));
    assert_eq!(events[5].1["delta"], json!({"type": "input_json_delta", "partial_json": "{\"cmd\""}));
    assert_eq!(events[6].1["delta"], json!({"type": "input_json_delta", "partial_json": ":\"pwd\"}"}));
    assert_eq!(events[8].1["delta"]["stop_reason"], "tool_use");
    assert_eq!(events[8].1["usage"]["input_tokens"], 8);
    assert_eq!(events[8].1["usage"]["output_tokens"], 3);
  }

  #[test]
  fn test_stream_tool_call_without_index_is_rejected() {
    let mut stream = OpenAiChatStream::new("openai-model", false);
    let result = stream.events_for_chunk(&json!({
      "choices": [{"delta": {"tool_calls": [{"id": "call-1", "function": {"name": "Bash", "arguments": "{}"}}]}}],
    }));
    assert_eq!(result.unwrap_err(), Fault::Value("streaming tool_call missing index".to_string()));
  }

  #[test]
  fn test_stream_tool_call_without_index_uses_zero_for_idless_continuations() {
    let mut stream = OpenAiChatStream::new("gemini-model", true);
    let mut events = Vec::new();
    let first = stream.events_for_chunk(&json!({
      "choices": [{"delta": {"tool_calls": [{"id": "call-1", "function": {"name": "Bash", "arguments": "{"}}]}}],
    }));
    assert!(first.is_ok(), "Gemini's index-less delta should be accepted: {first:?}");
    events.extend(first.unwrap());
    let continuation = stream.events_for_chunk(&json!({
      "choices": [{"delta": {"tool_calls": [{"function": {"arguments": "\"cmd\":\"pwd\"}"}}]}}],
    }));
    assert!(continuation.is_ok(), "an id-less delta should continue index 0: {continuation:?}");
    events.extend(continuation.unwrap());
    events.extend(stream.finish_events().unwrap());

    let tool_starts: Vec<Value> = events
      .iter()
      .filter(|(name, data)| *name == "content_block_start" && data["content_block"]["type"] == "tool_use")
      .map(|(_, data)| data["content_block"].clone())
      .collect();
    let arguments: Vec<Value> = events
      .iter()
      .filter(|(name, data)| *name == "content_block_delta" && data["delta"]["type"] == "input_json_delta")
      .map(|(_, data)| data["delta"]["partial_json"].clone())
      .collect();
    assert_eq!(tool_starts, vec![json!({"type": "tool_use", "id": "call-1", "name": "Bash", "input": {}})]);
    assert_eq!(arguments, vec![json!("{"), json!("\"cmd\":\"pwd\"}")]);
  }

  #[test]
  fn test_claude_code_request_shape_uses_inline_system_and_google_schema() {
    let payload = json!({
      "model": "gemini-model",
      "system": [{"type": "text", "text": "Use the available tools."}],
      "messages": [
        {"role": "user", "content": [
          {"type": "text", "text": "Attribution reminder."},
          {"type": "text", "text": "Run pwd with the Bash tool."},
        ]},
        {"role": "system", "content": [{"type": "text", "text": "# Environment\nPrimary working directory: /tmp/run"}]},
      ],
      "thinking": {"type": "adaptive"},
      "output_config": {"effort": "medium"},
      "tools": [{
        "name": "Bash",
        "description": "Run a shell command",
        "input_schema": {
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "cmd": {"type": "string", "const": "pwd"},
            "count": {"type": "number", "exclusiveMinimum": 0},
            "choice": {"type": "string", "allOf": [{"type": "string"}]},
          },
          "required": ["cmd"],
        },
      }],
      "stream": true,
    });
    let gemini_rules = gemini_stream_rules("https://generativelanguage.googleapis.com/v1beta/openai/").unwrap();
    assert!(gemini_rules);
    assert!(!gemini_stream_rules("https://openai.example/v1").unwrap());
    let converted = anthropic_to_openai_chat_request(&payload, "gemini-model", gemini_rules);
    assert!(converted.is_ok(), "the Claude Code request should translate: {converted:?}");
    let converted = converted.unwrap();

    assert_eq!(
      converted["messages"][0],
      json!({
        "role": "system",
        "content": "Use the available tools.\n\n# Environment\nPrimary working directory: /tmp/run",
      })
    );
    assert_eq!(
      converted["messages"][1],
      json!({
        "role": "user",
        "content": "Attribution reminder.Run pwd with the Bash tool.",
      })
    );
    assert!(converted.get("thinking").is_none());
    assert!(converted.get("output_config").is_none());
    assert_eq!(
      converted["tools"][0]["function"]["parameters"],
      json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "cmd": {"type": "string"},
          "count": {"type": "number"},
          "choice": {"type": "string"},
        },
        "required": ["cmd"],
      })
    );
  }

  #[test]
  fn test_stream_tool_signatures_round_trip_for_calls_sharing_an_index() {
    let mut stream = OpenAiChatStream::new("gemini-model", true);
    let mut events = stream.start_events();
    events.extend(
      stream
        .events_for_chunk(&json!({
          "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call-a",
            "type": "function",
            "function": {"name": "Bash", "arguments": "{\"cmd\":"},
          }]}}],
        }))
        .unwrap(),
    );
    events.extend(
      stream
        .events_for_chunk(&json!({
          "choices": [{"delta": {"tool_calls": [
            {
              "index": 0,
              "extra_content": {"google": {"thought_signature": "SIG-A"}},
              "function": {"arguments": "\"pwd\"}"},
            },
            {
              "index": 0,
              "id": "call-b",
              "type": "function",
              "extra_content": {"google": {"thought_signature": "SIG-B"}},
              "function": {"name": "Read", "arguments": "{\"file_path\":\"image.png\"}"},
            },
          ]}}],
        }))
        .unwrap(),
    );
    events.extend(stream.finish_events().unwrap());

    let mut blocks: Vec<(u64, Value, String)> = Vec::new();
    for (name, data) in events {
      if name == "content_block_start" && data["content_block"]["type"] == "tool_use" {
        blocks.push((data["index"].as_u64().unwrap(), data["content_block"].clone(), String::new()));
      } else if name == "content_block_delta" && data["delta"]["type"] == "input_json_delta" {
        let block_index = data["index"].as_u64().unwrap();
        let (_, _, arguments) = blocks.iter_mut().find(|(index, _, _)| *index == block_index).unwrap();
        arguments.push_str(data["delta"]["partial_json"].as_str().unwrap());
      }
    }
    let ids: Vec<Value> = blocks.iter().map(|(_, block, _)| block["id"].clone()).collect();
    assert_eq!(ids, vec![json!("call-a"), json!("call-b")]);
    let tool_blocks: Vec<Value> = blocks
      .into_iter()
      .map(|(_, mut block, arguments)| {
        block["input"] = serde_json::from_str(&arguments).unwrap();
        block
      })
      .collect();
    let converted = anthropic_to_openai_chat_request(
      &json!({"messages": [{"role": "assistant", "content": tool_blocks}]}),
      "gemini-model",
      true,
    )
    .unwrap();
    let calls = &converted["messages"][0]["tool_calls"];
    assert_eq!(calls[0]["function"]["name"], "Bash");
    assert_eq!(calls[0]["function"]["arguments"], "{\"cmd\":\"pwd\"}");
    assert_eq!(calls[0]["extra_content"]["google"]["thought_signature"], "SIG-A");
    assert_eq!(calls[1]["function"]["name"], "Read");
    assert_eq!(calls[1]["function"]["arguments"], "{\"file_path\":\"image.png\"}");
    assert_eq!(calls[1]["extra_content"]["google"]["thought_signature"], "SIG-B");
  }

  #[test]
  fn test_non_stream_tool_signatures_round_trip_to_next_request() {
    let response = json!({
      "choices": [{"finish_reason": "tool_calls", "message": {"tool_calls": [{
        "id": "call-1",
        "type": "function",
        "function": {"name": "Bash", "arguments": "{\"cmd\":\"pwd\"}"},
        "extra_content": {"google": {"thought_signature": "SIG-1"}},
      }]}}],
    });
    let anthropic_response = openai_chat_response_to_anthropic(&response, "gemini-model").unwrap();
    let converted_request = request(json!({
      "messages": [{"role": "assistant", "content": anthropic_response["content"]}],
    }))
    .unwrap();
    assert_eq!(
      converted_request["messages"][0]["tool_calls"][0]["extra_content"],
      json!({"google": {"thought_signature": "SIG-1"}}),
    );
  }

  #[test]
  fn test_user_and_tool_result_images_convert_to_openai_image_url_parts() {
    let image_block = json!({
      "type": "image",
      "source": {"type": "base64", "media_type": "image/png", "data": "AQID"},
    });
    let image_part = json!({
      "type": "image_url",
      "image_url": {"url": "data:image/png;base64,AQID"},
    });
    let user_request = request(json!({
      "messages": [{"role": "user", "content": [
        {"type": "text", "text": "What is in this image?"},
        image_block.clone(),
      ]}],
    }));
    assert!(user_request.is_ok(), "user image should translate: {user_request:?}");
    assert_eq!(
      user_request.unwrap()["messages"][0]["content"],
      json!([
        {"type": "text", "text": "What is in this image?"},
        image_part.clone(),
      ])
    );

    let tool_result_request = request(json!({
      "messages": [{"role": "user", "content": [{
        "type": "tool_result",
        "tool_use_id": "call-1",
        "content": [{"type": "text", "text": "Image loaded."}, image_block],
      }]}],
    }));
    assert!(tool_result_request.is_ok(), "tool result image should translate: {tool_result_request:?}");
    let tool_result_request = tool_result_request.unwrap();
    assert_eq!(
      tool_result_request["messages"][0],
      json!({
        "role": "tool",
        "tool_call_id": "call-1",
        "content": "Image loaded.",
      })
    );
    assert_eq!(tool_result_request["messages"][1], json!({"role": "user", "content": [image_part]}));
  }

  #[test]
  fn test_gemini_stream_index_less_delta_with_signature_yields_one_tool_block() {
    let mut stream = OpenAiChatStream::new("gemini-model", true);
    let result = stream.events_for_chunk(&json!({
      "choices": [{"delta": {"tool_calls": [{
        "extra_content": {"google": {"thought_signature": "EqMDCqAD..."}},
        "function": {
          "arguments": "{\"description\":\"Print working directory\",\"command\":\"pwd\"}",
          "name": "Bash",
        },
        "id": "call_371733",
        "type": "function",
      }]}}],
    }));
    assert!(result.is_ok(), "Gemini's index-less signed call should translate: {result:?}");
    let mut events = result.unwrap();
    events.extend(stream.finish_events().unwrap());
    let tool_starts: Vec<Value> = events
      .iter()
      .filter(|(name, data)| *name == "content_block_start" && data["content_block"]["type"] == "tool_use")
      .map(|(_, data)| data["content_block"].clone())
      .collect();
    let arguments: Vec<Value> = events
      .iter()
      .filter(|(name, data)| *name == "content_block_delta" && data["delta"]["type"] == "input_json_delta")
      .map(|(_, data)| data["delta"]["partial_json"].clone())
      .collect();
    assert_eq!(
      tool_starts,
      vec![json!({
        "type": "tool_use",
        "id": "call_371733",
        "name": "Bash",
        "input": {},
        "extra_content": {"google": {"thought_signature": "EqMDCqAD..."}},
      })]
    );
    assert_eq!(arguments, vec![json!("{\"description\":\"Print working directory\",\"command\":\"pwd\"}")]);
  }

  #[test]
  fn test_gemini_stream_two_index_less_calls_split_by_id() {
    let mut stream = OpenAiChatStream::new("gemini-model", true);
    let chunks = [
      json!({"choices": [{"delta": {"tool_calls": [{
        "id": "call-a",
        "function": {"name": "Bash", "arguments": "{\"cmd\":"},
      }]}}]}),
      json!({"choices": [{"delta": {"tool_calls": [{"function": {"arguments": "\"pwd\"}"}}]}}]}),
      json!({"choices": [{"delta": {"tool_calls": [{
        "id": "call-b",
        "function": {"name": "Read", "arguments": "{\"file_path\":\"image.png\"}"},
      }]}}]}),
    ];
    let mut events = Vec::new();
    for chunk in chunks {
      let result = stream.events_for_chunk(&chunk);
      assert!(result.is_ok(), "Gemini's index-less tool delta should translate: {result:?}");
      events.extend(result.unwrap());
    }
    events.extend(stream.finish_events().unwrap());

    let tool_starts: Vec<Value> = events
      .iter()
      .filter(|(name, data)| *name == "content_block_start" && data["content_block"]["type"] == "tool_use")
      .map(|(_, data)| data["content_block"].clone())
      .collect();
    let arguments: Vec<Value> = events
      .iter()
      .filter(|(name, data)| *name == "content_block_delta" && data["delta"]["type"] == "input_json_delta")
      .map(|(_, data)| data["delta"]["partial_json"].clone())
      .collect();
    assert_eq!(
      tool_starts,
      vec![
        json!({"type": "tool_use", "id": "call-a", "name": "Bash", "input": {}}),
        json!({"type": "tool_use", "id": "call-b", "name": "Read", "input": {}}),
      ]
    );
    assert_eq!(arguments, vec![json!("{\"cmd\":"), json!("\"pwd\"}"), json!("{\"file_path\":\"image.png\"}")]);
  }
}
