//! The Claude Code `PreToolUse` hook for unbounded scans in Bash commands.
//!
//! A refused scan is one of three shell-level forms: one process per session directory, a search from `/` or the home
//! directory that neither limits depth nor prunes the session store, or a recursive search of the session store or a
//! parent of it whose file-name filter reaches run logs. Script code inside single quotes or heredoc bodies is not
//! shell, so one process that reads all the files in a glob passes.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::OnceLock;

use charliebot_claude_code::claude_launch;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::config::SESSIONS_DIR_NAME;
use charliebot_core::home::CHARLIEBOT_HOME_ENV;
use charliebot_core::metadata_slot::METADATA_FILE_NAME;
use charliebot_store::layout::DATA_DIR_NAME;
use charliebot_store::run_records::{RAW_LOG_NAME, RUN_EVENTS_NAME};
use fancy_regex::{Captures, Regex};
use serde_json::{Value, json};

/// The profile-home directory name under the user's home.
const CHARLIEBOT_DIR: &str = ".charliebot";

const PER_FILE_PROCESS_REASON: &str = "This command starts one process per session directory. Use `charliebot session tree --root <id> --include-archived --limit 500` for a session's children, or read all the files in one process.";
const ROOTED_SEARCH_REASON: &str = "This search runs from `/` or the home directory with no depth limit. Search the directories that can hold the answer, add `-maxdepth`, or prune the session store.";
const RUN_LOG_SEARCH_REASON: &str = "This recursive search reaches the session store's run logs. Filter with `-g 'chat_events*.jsonl'`, `-g metadata.json` or `-g '*.md'`, or search a more specific directory.";

/// The unbounded scan form a Bash command holds; each refusal names the form's bounded replacement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScanForm {
  PerFileProcess,
  RootedSearch,
  RunLogSearch,
}

impl ScanForm {
  fn reason(self) -> &'static str {
    match self {
      ScanForm::PerFileProcess => PER_FILE_PROCESS_REASON,
      ScanForm::RootedSearch => ROOTED_SEARCH_REASON,
      ScanForm::RunLogSearch => RUN_LOG_SEARCH_REASON,
    }
  }
}

/// Read one Claude Code hook event and emit a deny decision when its Bash command holds an unbounded scan.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  if !args.is_empty() {
    return Err(CliExit::usage("scan-guard takes no arguments"));
  }
  run(std::io::stdin().lock(), &mut std::io::stdout().lock())
}

fn run<R: Read, W: Write>(mut input: R, output: &mut W) -> Result<(), CliExit> {
  let mut input_text = String::new();
  input
    .read_to_string(&mut input_text)
    .map_err(|error| CliExit::error(format!("could not read hook input: {error}")))?;
  let event: Value =
    serde_json::from_str(&input_text).map_err(|error| CliExit::error(format!("invalid hook JSON: {error}")))?;
  if event.get("tool_name").and_then(Value::as_str) != Some("Bash") {
    return Ok(());
  }
  let command = event
    .pointer("/tool_input/command")
    .and_then(Value::as_str)
    .ok_or_else(|| CliExit::error("Bash hook input must include a string tool_input.command"))?;
  let forms = scan_forms(command).map_err(CliExit::error)?;
  if let Some(form) = forms.first() {
    serde_json::to_writer(&mut *output, &deny_decision(form.reason()))
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

/// The unbounded scan forms one command holds, in refusal order: a per-file process, then a rooted search, then a
/// run-log search.
fn scan_forms(command: &str) -> Result<Vec<ScanForm>, String> {
  let shell = shell_text(command);
  let mut forms = Vec::new();
  if per_file_process_pattern()?.is_match(&shell).map_err(parse_error)? {
    forms.push(ScanForm::PerFileProcess);
  }
  for captures in rooted_search_pattern()?.captures_iter(&shell) {
    let captures = captures.map_err(parse_error)?;
    let around = groups_text(&captures);
    if !around.contains("-maxdepth") && !around.contains(SESSIONS_DIR_NAME) {
      forms.push(ScanForm::RootedSearch);
      break;
    }
  }
  for captures in run_log_search_pattern()?.captures_iter(command) {
    let captures = captures.map_err(parse_error)?;
    let globs = glob_names(&groups_text(&captures))?;
    if globs.is_empty() || globs.iter().any(|glob| run_log_globs().contains(&glob.as_str())) {
      forms.push(ScanForm::RunLogSearch);
      break;
    }
  }
  Ok(forms)
}

/// The command with heredoc bodies and single-quoted strings removed: script code there is not shell.
fn shell_text(command: &str) -> String {
  let without_heredocs = heredoc_pattern().replace_all(command, "<<HEREDOC");
  single_quote_pattern().replace_all(&without_heredocs, "''").into_owned()
}

fn groups_text(captures: &Captures<'_>) -> String {
  [1, 2].into_iter().filter_map(|index| captures.get(index)).map(|group| group.as_str()).collect()
}

fn glob_names(text: &str) -> Result<Vec<String>, String> {
  glob_pattern()
    .captures_iter(text)
    .map(|captures| {
      Ok(captures.map_err(parse_error)?.get(1).expect("glob pattern has a name group").as_str().to_string())
    })
    .collect()
}

fn run_log_globs() -> [&'static str; 9] {
  ["*", "**", "*.jsonl", "*.ndjson", "*.json", RUN_EVENTS_NAME, RAW_LOG_NAME, "agent.raw*", "*.log"]
}

fn parse_error(error: fancy_regex::Error) -> String {
  format!("scan command parsing failed: {error}")
}

fn home_token() -> Result<String, String> {
  match std::env::var("HOME") {
    Ok(home) => Ok(regex::escape(&home)),
    Err(error) => Err(format!("scan command parsing failed: the patterns need HOME: {error}")),
  }
}

/// The session store: `~/.charliebot/sessions`, the absolute home's, or `$CHARLIEBOT_HOME`'s.
fn store_token() -> Result<String, String> {
  Ok(format!(
    concat!(
      r"(?:",
      r"(?:~|\$HOME|{home})/{charliebot_dir}/{sessions}",
      r"|\$\{{?{charliebot_home}\}}?/{sessions}",
      r")"
    ),
    home = home_token()?,
    charliebot_dir = CHARLIEBOT_DIR,
    charliebot_home = regex::escape(CHARLIEBOT_HOME_ENV),
    sessions = regex::escape(SESSIONS_DIR_NAME),
  ))
}

/// The session store or a parent of it: `~/.charliebot` and `$CHARLIEBOT_HOME`, each with or without `sessions`.
fn store_or_parent_token() -> Result<String, String> {
  Ok(format!(
    concat!(
      r"(?:",
      r"(?:~|\$HOME|{home})/{charliebot_dir}(?:/{sessions})?",
      r"|\$\{{?{charliebot_home}\}}?(?:/{sessions})?",
      r")"
    ),
    home = home_token()?,
    charliebot_dir = CHARLIEBOT_DIR,
    charliebot_home = regex::escape(CHARLIEBOT_HOME_ENV),
    sessions = regex::escape(SESSIONS_DIR_NAME),
  ))
}

/// An enumeration over every session directory: a store glob, `*/metadata.json` or `*/data/...` after a `cd` into the
/// store, or `$(ls|find ... <store>)`.
fn enumeration_token(store: &str) -> String {
  format!(
    concat!(r"(?:", r"{store}/\*", r"|(?<![\w/])\*/(?:{metadata}|{data}/)", r"|\$\((?:ls|find)\s[^)]*{store}", r")"),
    store = store,
    metadata = regex::escape(METADATA_FILE_NAME),
    data = regex::escape(DATA_DIR_NAME),
  )
}

/// (a) One process per session directory at the shell level: a `for` loop over an enumeration, an enumeration piped to
/// `while read`, `xargs -n1` after a store path, or `find <store> -exec`.
fn per_file_process_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| {
      let store = store_token()?;
      let enumeration = enumeration_token(&store);
      Regex::new(&format!(
        concat!(
          r"\bfor\s+\w+\s+in\s+[^;\n]*?(?:{enumeration})[^;\n]*;\s*do\b",
          r"|(?:{enumeration})[^;\n]*\|\s*while\s+read\b",
          r"|{store}[^|;\n]*\|\s*xargs\s+(?:-\w+\s+)*-n\s*1\b",
          r"|\bfind\s+{store}\S*\s[^|;\n]*-exec\b[^;\n]*;"
        ),
        enumeration = enumeration,
        store = store
      ))
      .map_err(|error| format!("scan command parsing failed: {error}"))
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// (b) A search from `/` or the home directory itself, with its arguments on both sides.
fn rooted_search_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| {
      Regex::new(&format!(
        concat!(
          r"\b(?:find|rg|grep\s+-[a-zA-Z]*r[a-zA-Z]*|fd)\b([^|;&\n]*?)",
          r"\s(?:/|~|~/|\$HOME|{home}/?)(?=\s|$|\))([^|;&\n]*)"
        ),
        home = home_token()?
      ))
      .map_err(|error| format!("scan command parsing failed: {error}"))
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// (c) A recursive search of the store or a parent of it, with its arguments on both sides.
fn run_log_search_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| {
      Regex::new(&format!(
        concat!(
          r"\b(?:rg|grep\s+-[a-zA-Z]*r[a-zA-Z]*)\b([^|;&\n]*?)",
          r#"\s\"?(?:{store_or_parent})/?\"?(?=\s|$|\))([^|;&\n]*)"#
        ),
        store_or_parent = store_or_parent_token()?
      ))
      .map_err(|error| format!("scan command parsing failed: {error}"))
    })
    .as_ref()
    .map_err(Clone::clone)
}

fn heredoc_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r#"(?s)<<-?\s*['"]?(\w+)['"]?.*?\n(.*?)\n\1\b"#).expect("heredoc pattern is valid"))
}

fn single_quote_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r"'[^']*'").expect("single-quote pattern is valid"))
}

/// A file-name filter with its value: an exclusion (`-g '!...'`) matches none, so it names no glob.
fn glob_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN
    .get_or_init(|| Regex::new(r#"(?:-g|--glob|--include)[ =]['"]?(?!!)([^'" ]+)"#).expect("glob pattern is valid"))
}

#[cfg(test)]
mod tests {
  use std::io::Cursor;

  use super::*;

  fn run_input(input: &str) -> (Result<(), CliExit>, Vec<u8>) {
    let mut output = Vec::new();
    let result = run(Cursor::new(input.as_bytes()), &mut output);
    (result, output)
  }

  fn hook_input(command: &str) -> String {
    serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}}).to_string()
  }

  fn home() -> String {
    std::env::var("HOME").expect("the test cases name the home through HOME")
  }

  #[test]
  fn scan_form_cases_match_the_replay_table() {
    let denied = [
      ScanForm::PerFileProcess,
      ScanForm::RootedSearch,
      ScanForm::RunLogSearch,
      ScanForm::PerFileProcess,
      ScanForm::PerFileProcess,
      ScanForm::PerFileProcess,
      ScanForm::RunLogSearch,
      ScanForm::RunLogSearch,
      ScanForm::PerFileProcess,
      ScanForm::RunLogSearch,
      ScanForm::PerFileProcess,
    ];
    let commands: [&str; 11] = [
      // The replay's first real command: a metadata loop after a `cd` into the store.
      r#"cd ~/.charliebot/sessions && for f in */metadata.json; do python3 -I -c "import json; print(json.load(open('$f')))"; done"#,
      "find / -path /proc -prune -o -type d -name dataviz -print 2>/dev/null | head -3",
      r#"rg -o 'ur-[A-Za-z0-9._-]+' "$CHARLIEBOT_HOME/sessions" ~/.charlie-code/sessions 2>/dev/null | sort -u"#,
      // A run-record loop whose python body is a heredoc, not shell.
      "cd ~/.charliebot/sessions && for m in */data/runs/*/metadata.json; do python3 - \"$m\" <<'EOF2'\nimport json\nd = json.load(open(\"$m\"))\nprint(d)\nEOF2\ndone",
      r#"for d in $(ls ~/.charliebot/sessions); do python3 -I -c 'print(1)' $d/metadata.json; done"#,
      "find ~/.charliebot/sessions -name metadata.json | xargs -n1 python3 -I x.py",
      r"rg -l X ~/.charliebot/sessions -g '*.jsonl'",
      r"rg -l X ~/.charliebot/sessions -g 'agent.raw.ndjson'",
      r"grep -l 520ad103 ~/.charliebot/sessions/*/metadata.json | while read f; do stat $f; done",
      // The absolute home form, spelled through the test home.
      &format!("grep -rl '39828874' {}/.charliebot/ 2>/dev/null", home()),
      // The same loop shape with a different body still refuses.
      r#"cd ~/.charliebot/sessions && for f in */metadata.json; do python3 -I -c "import json,sys; print(json.load(open('$f')).get('title'))"; done"#,
    ];
    for (command, expected) in commands.iter().zip(denied) {
      let forms = scan_forms(command).unwrap();
      assert_eq!(forms.first(), Some(&expected), "{command}");
    }
  }

  #[test]
  fn bounded_lookup_cases_hold_no_scan_form() {
    let commands: [&str; 10] = [
      "charliebot session tree --root 28f1df1f --include-archived --limit 500",
      r"rg -l -F X ~/.charliebot/sessions -g 'chat_events*.jsonl'",
      r"rg -l -F X ~/.charliebot/sessions -g '*.md'",
      "find / -maxdepth 4 -name X",
      &format!(r"find / \( -path /proc -o -path {}/.charliebot/sessions \) -prune -o -name X -print", home()),
      r"ls -d ~/.charliebot/sessions/92b2*",
      "rg -n X ~/workspace/charlie-bot/rust",
      r"for p in 92b28639 c8eb085f; do ls -d ~/.charliebot/sessions/${p}*/; done",
      &format!(
        r#"python3 -I -c 'import glob,json; [print(f) for f in glob.glob("{}/.charliebot/sessions/*/metadata.json") if "x" in open(f).read()]'"#,
        home()
      ),
      r"grep -l X ~/.charliebot/sessions/*/metadata.json",
    ];
    for command in commands {
      assert!(scan_forms(command).unwrap().is_empty(), "{command}");
    }
  }

  #[test]
  fn non_bash_hook_event_prints_nothing() {
    let (result, output) = run_input(r#"{"tool_name":"Read","tool_input":{}}"#);
    result.unwrap();
    assert!(output.is_empty());
  }

  #[test]
  fn invalid_json_returns_a_stderr_cli_error_without_a_deny_decision() {
    let (result, output) = run_input("not json");
    let error = result.unwrap_err();
    assert_eq!(error.code, 1);
    assert!(error.body["error"].as_str().unwrap().starts_with("invalid hook JSON:"));
    assert!(output.is_empty());
  }

  #[test]
  fn bash_event_without_a_string_command_returns_a_stderr_cli_error() {
    for input in [r#"{"tool_name":"Bash"}"#, r#"{"tool_name":"Bash","tool_input":{"command":61}}"#] {
      let (result, output) = run_input(input);
      let error = result.unwrap_err();
      assert_eq!(error.code, 1);
      assert!(error.body["error"].as_str().unwrap().contains("tool_input.command"));
      assert!(output.is_empty());
    }
  }

  #[test]
  fn refused_command_prints_the_exact_deny_json() {
    let (result, output) = run_input(&hook_input(r"rg -l X ~/.charliebot/sessions -g '*.jsonl'"));
    result.unwrap();
    let actual: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
      actual,
      json!({
        "hookSpecificOutput": {
          "hookEventName": "PreToolUse",
          "permissionDecision": "deny",
          "permissionDecisionReason": RUN_LOG_SEARCH_REASON
        }
      })
    );
  }

  #[test]
  fn allowed_command_prints_nothing() {
    let (result, output) =
      run_input(&hook_input("charliebot session tree --root 28f1df1f --include-archived --limit 500"));
    result.unwrap();
    assert!(output.is_empty());
  }
}
