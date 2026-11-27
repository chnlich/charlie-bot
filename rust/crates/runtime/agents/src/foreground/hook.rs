//! The `PreToolUse` answer of the foreground cap. The Claude, Codex and Devin hook events carry the shell command in
//! `tool_input`, and the answer rewrites it into the call entry. The agy hook event carries it in `toolCall.args`, and
//! its answer rewrites it only when the launch names a session cgroup.

use serde_json::{Value, json};

use super::{HOOK_COMMAND, call_command};

/// The agy tool that runs a shell command.
const AGY_RUN_COMMAND: &str = "run_command";

/// The answer to one hook event, as JSON for the hook's stdout. `None` lets the event pass without an answer.
///
/// `session_cgroup_set` says whether the launch set `CHARLIEBOT_SESSION_CGROUP`. Only the agy answer reads it: the agy
/// hook is global, so it rewrites a call only in a CharlieBot launch. Any other event is rewritten on every launch.
pub fn answer(event: &Value, session_cgroup_set: bool) -> Result<Option<Value>, String> {
  if let Some(tool_call) = event.get("toolCall") {
    return agy_answer(tool_call, session_cgroup_set).map(Some);
  }
  Ok(rewrite_tool_input(event)?.map(|tool_input| {
    json!({
      "hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "allow",
        "updatedInput": tool_input
      }
    })
  }))
}

/// The agy answer: the call with its `CommandLine` rewritten when the launch set the session cgroup, else the call
/// unchanged.
fn agy_answer(tool_call: &Value, session_cgroup_set: bool) -> Result<Value, String> {
  let unchanged = json!({"decision": "allow"});
  if !session_cgroup_set || tool_call.get("name").and_then(Value::as_str) != Some(AGY_RUN_COMMAND) {
    return Ok(unchanged);
  }
  let command = tool_call
    .pointer("/args/CommandLine")
    .and_then(Value::as_str)
    .ok_or_else(|| format!("{AGY_RUN_COMMAND} carries no string CommandLine"))?;
  Ok(json!({"decision": "allow", "overwrite": {"CommandLine": call_command(command)}}))
}

/// The `tool_input` with its shell command rewritten, or `None` when the event carries no shell command. The command is
/// a string (`command` or `cmd`) or an argv list; a list stays a list and runs as `sh -c` over its shell-quoted words.
fn rewrite_tool_input(event: &Value) -> Result<Option<Value>, String> {
  let Some(tool_input) = event.get("tool_input").and_then(Value::as_object) else {
    return Ok(None);
  };
  let Some(key) = ["command", "cmd"].into_iter().find(|key| tool_input.contains_key(*key)) else {
    return Ok(None);
  };
  let replacement = match &tool_input[key] {
    Value::String(command) => Value::String(call_command(command)),
    Value::Array(words) => {
      let words = words
        .iter()
        .map(|word| word.as_str().ok_or_else(|| format!("tool_input.{key} holds a non-string word: {word}")))
        .collect::<Result<Vec<&str>, String>>()?;
      let joined = words
        .iter()
        .map(|word| shlex::try_quote(word).map(|quoted| quoted.into_owned()))
        .collect::<Result<Vec<String>, _>>()
        .map_err(|error| format!("tool_input.{key} has a word the shell cannot quote: {error}"))?
        .join(" ");
      json!(["sh", "-c", call_command(&joined)])
    }
    other => return Err(format!("tool_input.{key} must be a string or an argv list, got {other}")),
  };
  let mut updated = tool_input.clone();
  updated.insert(key.to_string(), replacement);
  Ok(Some(Value::Object(updated)))
}

/// The `PreToolUse` hook entry that runs the cap for the tools that `matcher` names, in the shape a Claude or Devin
/// settings file takes.
pub fn pre_tool_use_entry(matcher: &str) -> Value {
  json!({"matcher": matcher, "hooks": [{"type": "command", "command": HOOK_COMMAND}]})
}

#[cfg(test)]
mod tests {
  use super::*;

  fn bash_event(tool_input: Value) -> Value {
    json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": tool_input})
  }

  #[test]
  fn a_string_command_is_rewritten_into_the_call_entry_and_keeps_its_other_inputs() {
    let event = bash_event(json!({"command": "echo BEFORE; sleep 25; echo AFTER", "timeout": 120000}));
    let answer = answer(&event, false).unwrap().unwrap();
    let updated = &answer["hookSpecificOutput"]["updatedInput"];
    assert_eq!(updated["timeout"], json!(120000));
    let rewritten = updated["command"].as_str().unwrap();
    assert_eq!(
      rewritten,
      call_command("echo BEFORE; sleep 25; echo AFTER"),
      "the Claude, Codex and Devin answers carry the call entry whether or not the launch set the cgroup"
    );
    assert_eq!(answer["hookSpecificOutput"]["hookEventName"], json!("PreToolUse"));
  }

  #[test]
  fn a_cmd_key_is_rewritten_the_same_way() {
    let event = bash_event(json!({"cmd": "sleep 1"}));
    let answer = answer(&event, true).unwrap().unwrap();
    assert_eq!(answer["hookSpecificOutput"]["updatedInput"]["cmd"], json!(call_command("sleep 1")));
  }

  #[test]
  fn an_argv_list_becomes_a_shell_command_under_sh_c() {
    let event = bash_event(json!({"command": ["/bin/bash", "-lc", "echo 'a b'"]}));
    let answer = answer(&event, false).unwrap().unwrap();
    let rewritten = answer["hookSpecificOutput"]["updatedInput"]["command"].as_array().unwrap();
    assert_eq!(rewritten[0], json!("sh"));
    assert_eq!(rewritten[1], json!("-c"));
    let expected = call_command(r#"/bin/bash -lc "echo 'a b'""#);
    assert_eq!(rewritten[2].as_str().unwrap(), expected);
  }

  #[test]
  fn an_event_without_a_shell_command_passes() {
    let event = json!({"hook_event_name": "PreToolUse", "tool_name": "Read", "tool_input": {"file_path": "/x"}});
    assert_eq!(answer(&event, true).unwrap(), None);
  }

  #[test]
  fn a_command_of_another_shape_is_an_error_that_blocks_the_call() {
    let event = bash_event(json!({"command": 7}));
    assert!(answer(&event, true).unwrap_err().contains("must be a string or an argv list"));
  }

  #[test]
  fn the_agy_answer_is_a_no_op_without_the_session_cgroup() {
    let event = json!({"toolCall": {"name": "run_command", "args": {"CommandLine": "sleep 20"}}});
    assert_eq!(answer(&event, false).unwrap(), Some(json!({"decision": "allow"})));
  }

  #[test]
  fn the_agy_answer_rewrites_the_command_line_with_the_session_cgroup() {
    let event = json!({"toolCall": {"name": "run_command", "args": {"CommandLine": "sleep 20"}}});
    let answer = answer(&event, true).unwrap().unwrap();
    assert_eq!(answer["decision"], json!("allow"));
    assert_eq!(answer["overwrite"]["CommandLine"], json!(call_command("sleep 20")));
  }

  #[test]
  fn the_agy_answer_passes_other_tools_unchanged() {
    let event = json!({"toolCall": {"name": "schedule", "args": {}}});
    assert_eq!(answer(&event, true).unwrap(), Some(json!({"decision": "allow"})));
  }

  #[test]
  fn an_agy_run_command_without_a_command_line_is_an_error() {
    let event = json!({"toolCall": {"name": "run_command", "args": {}}});
    assert!(answer(&event, true).is_err());
  }

  #[test]
  fn the_pre_tool_use_entry_names_the_cap_command_for_its_matcher() {
    let entry = pre_tool_use_entry("exec");
    assert_eq!(entry["matcher"], json!("exec"));
    assert_eq!(entry["hooks"][0]["command"], json!("charliebot foreground-cap"));
  }
}
