//! The one-line form of a chat tool call, shared by `charliebot session dialog` and the handoff summary.

use charliebot_core::text_repr::{self, ONE_LINE_JSON};
use serde_json::Value;

/// One tool call's natural one-line form: Bash carries its command, every other tool a compact JSON rendering of
/// the input; only the first line survives.
pub fn tool_input_first_line(tool: &Value) -> String {
  let rendered = match tool.get("input") {
    Some(Value::Object(input)) => {
      let is_bash = matches!(tool.get("name").and_then(Value::as_str), Some("Bash") | Some("bash"));
      match (is_bash, input.get("command")) {
        (true, Some(Value::String(command))) => command.clone(),
        _ => text_repr::json_dumps(&Value::Object(input.clone()), &ONE_LINE_JSON),
      }
    }
    Some(Value::String(text)) => text.clone(),
    _ => String::new(),
  };
  rendered.split_once('\n').map_or(rendered.clone(), |(first, _)| first.to_string())
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::tool_input_first_line;

  #[test]
  fn a_bash_call_carries_its_first_command_line() {
    let tool = json!({"name": "Bash", "input": {"command": "cargo test\ncargo clippy"}});
    assert_eq!(tool_input_first_line(&tool), "cargo test");
  }

  #[test]
  fn another_tool_carries_a_compact_json_input() {
    let tool = json!({"name": "Read", "input": {"file_path": "/synthetic/a.rs", "limit": 2}});
    assert_eq!(tool_input_first_line(&tool), r#"{"file_path": "/synthetic/a.rs", "limit": 2}"#);
  }
}
