//! The `PreToolUse` hook I/O that the Bash guards share: one hook event on stdin, and one deny decision on stdout
//! when a guard refuses the event's Bash command. The guards are `wait-guard`, `scan-guard` and `kill-guard`, and
//! the backends register them under the `PreToolUse` event with matcher `Bash`.

use std::io::{Read, Write};

use charliebot_claude_code::claude_launch;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::event_types;
use serde_json::{Value, json};

/// Reads one hook event from `input`. An event for another tool passes without a call. For a Bash event, `refuse` gets
/// the event and its command, and returns the reason to refuse it, or `None` to let it run. A refusal writes the deny
/// decision that carries the reason.
pub fn run_bash_guard<R: Read, W: Write>(
  mut input: R,
  output: &mut W,
  refuse: impl FnOnce(&Value, &str) -> Result<Option<&'static str>, String>,
) -> Result<(), CliExit> {
  let mut input_text = String::new();
  input
    .read_to_string(&mut input_text)
    .map_err(|error| CliExit::error(format!("could not read hook input: {error}")))?;
  let event: Value =
    serde_json::from_str(&input_text).map_err(|error| CliExit::error(format!("invalid hook JSON: {error}")))?;
  if event.get(event_types::TOOL_NAME).and_then(Value::as_str) != Some("Bash") {
    return Ok(());
  }
  let command = event
    .pointer("/tool_input/command")
    .and_then(Value::as_str)
    .ok_or_else(|| CliExit::error("Bash hook input must include a string tool_input.command"))?;
  if let Some(reason) = refuse(&event, command).map_err(CliExit::error)? {
    serde_json::to_writer(&mut *output, &deny_decision(reason))
      .map_err(|error| CliExit::error(format!("could not write hook output: {error}")))?;
    output.write_all(b"\n").map_err(|error| CliExit::error(format!("could not write hook output: {error}")))?;
  }
  Ok(())
}

fn deny_decision(reason: &str) -> Value {
  json!({
    "hookSpecificOutput": {
      "hookEventName": claude_launch::PRE_TOOL_USE_HOOK_EVENT,
      "permissionDecision": "deny",
      "permissionDecisionReason": reason
    }
  })
}
