//! The read-budget hook for manager turns: it holds each turn's direct reads within the
//! 16,000-character small-material budget of Direct Work (`prompts/master.md`).
//!
//! `charliebot read-budget --session SID --run RUN` is the Claude Code hook that the manager turn's
//! settings register under PostToolUse (matcher *), PostToolUseFailure (matcher *) and
//! SessionStart (matcher compact). Each event adds one tool result's text to the run's counted
//! total in `sessions/<sid>/data/runs/<run>/read_budget.json`, under an exclusive `flock`, and the
//! total restarts at zero when compaction fires SessionStart. A result that does not fit the
//! remaining budget is withheld when it is a successful Bash or Read output: `updatedToolOutput`
//! replaces the text the model reads with the withhold notice, and the result counts nothing. A
//! failure or another tool's output passes whole, is counted, and carries the over-budget notice as
//! `additionalContext`. Exempt material — rule and skill files, child conclusions and session
//! status, approval objects, the user's decisions — is never counted and never withheld.
//!
//! Persisted output: when a Bash output exceeds the inline limit, the model reads only the
//! `<persisted-output>` wrapper with a first-2KB preview, and the full text is saved at the
//! response's `persistedOutputPath` (`tool-results/<id>.txt` beside the transcript). The hook
//! counts the wrapper and the preview for the persisted call, and records the path with the source
//! call's exemption class. That recorded path is the link from a read-back to its source: when a
//! later Read names a recorded `persistedOutputPath`, the read-back keeps the exemption of the item
//! the source call read (a Bash text extract of an approval page stays exempt), and a read-back of
//! a counted source is its own counted read, withheld when it does not fit. A Bash command that
//! reads the saved file counts through the normal Bash rules. A missing or malformed field, a
//! missing run directory and a state-file failure exit 2 with the error on stderr, which Claude
//! Code shows to the model.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

use charliebot_claude_code::claude_launch;
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::config;
use charliebot_core::event_types::{self, TOOL_INPUT_FILE_PATH};
use charliebot_core::home;
use charliebot_core::subagent::SUBAGENT_TOOL_NAMES;
use charliebot_store::layout;
use clap::{CommandFactory, FromArgMatches, Parser};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The small-material budget of one turn, in characters (Direct Work, `prompts/master.md`).
pub const BUDGET_CHARS: usize = 16_000;

/// The state file under the run directory, and the lock that serializes its read-modify-write.
const STATE_FILE_NAME: &str = "read_budget.json";
const LOCK_FILE_NAME: &str = "read_budget.lock";

/// The tools whose successful output Claude Code replaces through `updatedToolOutput` (tested on
/// Claude Code 2.1.319). Any other tool's over-budget output passes whole and carries the notice.
const WITHHOLDABLE_TOOLS: [&str; 2] = ["Bash", "Read"];

/// The tools whose `tool_response` echoes written content the model never reads, so they count 0.
const WRITING_TOOLS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// Claude Code's persisted-output preview holds at most this many characters of the output ("first
/// 2KB"), cut back to the last line end when that line end lies past the preview's halfway mark.
const PERSISTED_PREVIEW_CHARS: usize = 2000;

/// The withhold notice that replaces a withheld output's text; `{size}` is the result's character
/// count and `{left}` the budget the turn has left.
const WITHHOLD_NOTICE: &str = "Output withheld: {size} characters exceed the {left} characters left in this turn's 16,000-character \
  small-material budget (Direct Work, prompts/master.md). The call already ran; only its output is withheld. \
  To read this material, hand the read to a subagent (the `Agent` tool) or to a repo-less worker with \
  `--task-type script-run`. To read part of it yourself, rerun the read capped within {left} characters. \
  Read a child's conclusion from the report.md in its run directory and a plan from its page under \
  artifacts/: the budget does not count those paths.";

/// The over-budget notice that rides with an unwithholdable over-budget output; `{total}` is the
/// run's counted total after this result.
const OVER_BUDGET_NOTICE: &str = "This turn's material totals {total} characters, over the 16,000-character small-material budget of Direct \
  Work (prompts/master.md). Each later model call re-reads this material. When you act as a lead, hand each \
  further read of material to a subagent (the `Agent` tool) or to a repo-less worker with \
  `--task-type script-run`.";

/// The paths of rule and skill material, whose reads Direct Work keeps outside the budget.
const RULE_PATH: &str = r"(/skills/|(^|[\s:/])prompts/|SKILL\.md|LESSONS\.md|CLAUDE\.md|AGENTS\.md|\.claude/rules/)";

/// The approval objects under a session's artifacts directory.
const APPROVAL_PATH: &str = r"/artifacts/(plan|understanding)_[^/\s]*\.html";

/// A child's conclusion: a run directory's report or findings file, or the query a report reader runs.
const CHILD_REPORT: &str = r#"/data/runs/[^/\s]+/(report|findings)\.md|select\(\.type ?== ?\\?"result\\?"\)"#;

/// CharlieBot's own coordination surface: session status and tree, child and wake records,
/// delegation and trigger receipts, plans, digest, decisions (taste, memory), page checks.
/// `charliebot session dialog` reads another session's record, and the integration subcommands read
/// outside material, so both count. The session arm excludes `dialog` by its captured subcommand
/// word, because the fast regular-expression engine holds no lookahead.
const COORDINATION_CMD: &str =
  r"\bcharliebot\s+(delegate|worker|plan|digest|taste|memory|web|artifact|publish|improve)\b";
const SESSION_CMD: &str = r"\bcharliebot\s+session\s+([\w-]+)";
const SESSION_DIALOG: &str = "dialog";

/// The shell readers whose presence in a Bash command keeps a rule-path read counted: their output
/// is a selection of the material, not the material itself.
const SHELL_READERS: &str = r"\bgrep\b|\brg\b|\bfind\b|\bdiff\b|git (log|diff)";

#[derive(Parser)]
#[command(
  name = "read-budget",
  about = "Hold a manager turn's direct reads within the 16,000-character small-material budget"
)]
struct ReadBudgetCli {
  /// CharlieBot session id
  #[arg(long)]
  session: String,
  /// CharlieBot run id of this manager turn
  #[arg(long)]
  run: String,
}

/// The run's budget state. `counted` is this turn segment's counted total; `notices` counts the
/// over-budget notices of the whole run; `withheld` records each withheld output; `persisted`
/// records each persisted Bash output's saved path with the source call's exemption class, which
/// links a later read-back of that path to its source.
#[derive(Serialize, Deserialize, Default, PartialEq)]
struct BudgetState {
  counted: usize,
  notices: usize,
  withheld: Vec<WithheldOutput>,
  persisted: Vec<PersistedOutput>,
}

#[derive(Serialize, Deserialize, PartialEq)]
struct WithheldOutput {
  tool_use_id: String,
  tool_name: String,
  chars: usize,
}

#[derive(Serialize, Deserialize, PartialEq)]
struct PersistedOutput {
  path: String,
  /// The Direct Work item the source call read, `None` when its output counts toward the budget.
  exempt: Option<String>,
}

/// Parse the arguments and run one hook event. Parser errors print and exit on their own: help
/// exits 0, a usage error exits 2.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(ReadBudgetCli::command());
  let argv = std::iter::once(OsString::from("charliebot read-budget")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = ReadBudgetCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  let home = home::charliebot_home_dir()
    .map_err(|error| CliExit::usage(format!("could not resolve the CharlieBot home: {error}")))?;
  let run_dir = config::sessions_dir_in(&home)
    .join(&cli.session)
    .join(layout::DATA_DIR_NAME)
    .join(layout::RUNS_DIR_NAME)
    .join(&cli.run);
  let mut output = std::io::stdout().lock();
  run(&mut std::io::stdin().lock(), &run_dir, &mut output)
}

fn run<R: Read, W: Write>(input: &mut R, run_dir: &Path, output: &mut W) -> Result<(), CliExit> {
  let mut input_text = String::new();
  input
    .read_to_string(&mut input_text)
    .map_err(|error| CliExit::usage(format!("could not read hook input: {error}")))?;
  let event: Value =
    serde_json::from_str(&input_text).map_err(|error| CliExit::usage(format!("invalid hook JSON: {error}")))?;
  if !run_dir.is_dir() {
    return Err(CliExit::usage(format!("run directory {} does not exist", run_dir.display())));
  }
  let decision = match event_name(&event)? {
    claude_launch::SESSION_START_HOOK_EVENT => session_start(&event, run_dir)?,
    claude_launch::POST_TOOL_USE_HOOK_EVENT | claude_launch::POST_TOOL_USE_FAILURE_HOOK_EVENT => {
      tool_result(&event, run_dir)?
    }
    // The settings register only the three events above; anything else reads nothing.
    _ => None,
  };
  if let Some(decision) = decision {
    serde_json::to_writer(&mut *output, &decision)
      .map_err(|error| CliExit::usage(format!("could not write hook output: {error}")))?;
    output.write_all(b"\n").map_err(|error| CliExit::usage(format!("could not write hook output: {error}")))?;
  }
  Ok(())
}

/// The hook event's name, which every event carries.
fn event_name(event: &Value) -> Result<&str, CliExit> {
  event
    .get("hook_event_name")
    .and_then(Value::as_str)
    .ok_or_else(|| CliExit::usage("hook input must include a string hook_event_name"))
}

/// SessionStart (compact): compaction dropped the old material from the context, so the counted
/// total restarts at zero. The withheld list, the notices and the persisted paths stay: they are
/// run records, and a persisted path's exemption does not depend on the turn.
fn session_start(event: &Value, run_dir: &Path) -> Result<Option<Value>, CliExit> {
  let source = event
    .get("source")
    .and_then(Value::as_str)
    .ok_or_else(|| CliExit::usage("SessionStart hook input must include a string source"))?;
  if source != "compact" {
    return Ok(None);
  }
  update_state(run_dir, |state| state.counted = 0)?;
  Ok(None)
}

/// One tool result: count it, and decide whether the model reads it whole, a notice, or nothing.
fn tool_result(event: &Value, run_dir: &Path) -> Result<Option<Value>, CliExit> {
  let hook_event = event_name(event)?;
  let failure = hook_event == claude_launch::POST_TOOL_USE_FAILURE_HOOK_EVENT;
  let tool_name = event
    .get(event_types::TOOL_NAME)
    .and_then(Value::as_str)
    .ok_or_else(|| CliExit::usage(format!("{hook_event} hook input must include a string tool_name")))?;
  let tool_input = event
    .get("tool_input")
    .and_then(Value::as_object)
    .ok_or_else(|| CliExit::usage(format!("{hook_event} hook input must include an object tool_input")))?;
  let tool_use_id = event
    .get(event_types::TOOL_USE_ID)
    .and_then(Value::as_str)
    .ok_or_else(|| CliExit::usage(format!("{hook_event} hook input must include a string tool_use_id")))?;
  let response = if failure {
    let error = event
      .get("error")
      .and_then(Value::as_str)
      .ok_or_else(|| CliExit::usage("PostToolUseFailure hook input must include a string error"))?;
    &string_response(error)
  } else {
    event
      .get("tool_response")
      .and_then(Value::as_object)
      .ok_or_else(|| CliExit::usage("PostToolUse hook input must include an object tool_response"))?
  };

  let mut decision = None;
  update_state(run_dir, |state| {
    let persisted_path = persisted_output_path(tool_name, response);
    if let Some(path) = &persisted_path {
      state.persisted.push(PersistedOutput { path: path.clone(), exempt: exempt_class(tool_name, tool_input) });
    }
    let exempt =
      read_back_exemption(tool_name, tool_input, state).unwrap_or_else(|| exempt_class(tool_name, tool_input));
    if exempt.is_some() {
      return;
    }
    let size = if failure {
      response.get("error").and_then(Value::as_str).unwrap_or("").chars().count()
    } else {
      result_chars(tool_name, response)
    };
    // A result the model reads nothing of changes neither the total nor the notices.
    if size == 0 {
      return;
    }
    if state.counted + size > BUDGET_CHARS {
      if !failure && WITHHOLDABLE_TOOLS.contains(&tool_name) {
        state.withheld.push(WithheldOutput {
          tool_use_id: tool_use_id.to_string(),
          tool_name: tool_name.to_string(),
          chars: size,
        });
        let notice = withhold_notice(size, BUDGET_CHARS.saturating_sub(state.counted));
        decision = Some(withhold_decision(tool_name, response, &notice));
        return;
      }
      state.notices += 1;
      let notice = over_budget_notice(state.counted + size);
      decision = Some(notice_decision(hook_event, &notice));
    }
    state.counted += size;
  })?;
  Ok(decision)
}

/// A failure's response carries only the error text the model reads.
fn string_response(error: &str) -> Map<String, Value> {
  let mut map = Map::new();
  map.insert("error".to_string(), json!(error));
  map
}

/// The saved path of a persisted Bash output, `None` when the output was not persisted.
fn persisted_output_path(tool_name: &str, response: &Map<String, Value>) -> Option<String> {
  if tool_name != "Bash" {
    return None;
  }
  response.get("persistedOutputPath").and_then(Value::as_str).map(str::to_string)
}

/// The exemption a Read keeps from the item a persisted Bash call read: the state records each
/// persisted output's saved path with the source call's exemption class, and a Read that names a
/// recorded path inherits it. `Some(exempt)` when the path matches a record, `None` when the
/// normal classes decide.
fn read_back_exemption(
  tool_name: &str,
  tool_input: &Map<String, Value>,
  state: &BudgetState,
) -> Option<Option<String>> {
  if tool_name != "Read" {
    return None;
  }
  let path = tool_input.get(TOOL_INPUT_FILE_PATH).and_then(Value::as_str)?;
  state.persisted.iter().find(|entry| entry.path == path).map(|entry| entry.exempt.clone())
}

/// The withhold decision: the response's own shape with its text replaced by the notice. A Bash
/// response's stdout carries the notice and its stderr is emptied; a Read response's file content
/// carries the notice. Every other field stays as the tool returned it.
fn withhold_decision(tool_name: &str, response: &Map<String, Value>, notice: &str) -> Value {
  let mut map = response.clone();
  if tool_name == "Bash" {
    map.insert("stdout".to_string(), json!(notice));
    map.insert("stderr".to_string(), json!(""));
  } else if let Some(file) = map.get_mut("file").and_then(Value::as_object_mut) {
    file.insert("content".to_string(), json!(notice));
  }
  json!({
    "hookSpecificOutput": {
      "hookEventName": claude_launch::POST_TOOL_USE_HOOK_EVENT,
      "updatedToolOutput": Value::Object(map)
    }
  })
}

/// The over-budget notice decision for a failure or a non-withholdable tool's output.
fn notice_decision(hook_event: &str, notice: &str) -> Value {
  json!({
    "hookSpecificOutput": {
      "hookEventName": hook_event,
      "additionalContext": notice
    }
  })
}

fn withhold_notice(size: usize, left: usize) -> String {
  WITHHOLD_NOTICE.replace("{size}", &size.to_string()).replace("{left}", &left.to_string())
}

fn over_budget_notice(total: usize) -> String {
  OVER_BUDGET_NOTICE.replace("{total}", &total.to_string())
}

/// The Direct Work item a tool call reads, or `None` when its output counts toward the budget.
fn exempt_class(tool_name: &str, tool_input: &Map<String, Value>) -> Option<String> {
  let class = match tool_name {
    name if SUBAGENT_TOOL_NAMES.contains(&name) => Some("child_conclusion"),
    "Skill" => Some("rule_skill"),
    "Read" => tool_input.get(TOOL_INPUT_FILE_PATH).and_then(Value::as_str).and_then(path_class),
    "Bash" => tool_input.get("command").and_then(Value::as_str).and_then(bash_class),
    _ => None,
  };
  class.map(str::to_string)
}

fn path_class(path: &str) -> Option<&'static str> {
  child_report_pattern()
    .is_match(path)
    .then_some("child_conclusion")
    .or_else(|| rule_path_pattern().is_match(path).then_some("rule_skill"))
    .or_else(|| approval_path_pattern().is_match(path).then_some("approval_object"))
}

fn bash_class(command: &str) -> Option<&'static str> {
  status_decision(command)
    .then_some("status_decision")
    .or_else(|| child_report_pattern().is_match(command).then_some("child_conclusion"))
    .or_else(|| {
      rule_path_pattern()
        .is_match(command)
        .then_some(!shell_reader_pattern().is_match(command))
        .filter(|readable| *readable)
        .map(|_| "rule_skill")
    })
    .or_else(|| approval_path_pattern().is_match(command).then_some("approval_object"))
}

/// Whether the command is one of CharlieBot's coordination commands: any of the coordination verbs,
/// or a `charliebot session` subcommand that is not `dialog`.
fn status_decision(command: &str) -> bool {
  if coordination_cmd_pattern().is_match(command) {
    return true;
  }
  session_cmd_pattern()
    .captures_iter(command)
    .any(|captures| captures.get(1).is_some_and(|word| word.as_str() != SESSION_DIALOG))
}

/// The text of one successful tool result that the hook adds to the counted total. Bash reads its
/// `stdout` and `stderr`; a persisted output counts the `<persisted-output>` wrapper and the
/// first-2KB preview the model reads instead of the whole `stdout`. A Read counts its
/// `file.content` for a text result and nothing for an image or PDF. The writing tools echo
/// content the model never reads. Every other tool counts all string values of its response,
/// except base64 data. A failure counts its error text in the caller.
fn result_chars(tool_name: &str, response: &Map<String, Value>) -> usize {
  if WRITING_TOOLS.contains(&tool_name) {
    return 0;
  }
  if tool_name == "Bash" {
    let stderr = response.get("stderr").and_then(Value::as_str).unwrap_or("");
    let stderr = stderr.chars().count();
    let stdout = response.get("stdout").and_then(Value::as_str).unwrap_or("");
    return match persisted_output_path(tool_name, response) {
      Some(path) => {
        persisted_wrapper_chars(&path, response.get("persistedOutputSize").and_then(Value::as_u64))
          + preview_chars(stdout)
          + stderr
      }
      None => stdout.chars().count() + stderr,
    };
  }
  if tool_name == "Read" {
    return match response.get("type").and_then(Value::as_str).unwrap_or("") {
      "text" => {
        response.get("file").and_then(|file| file.get("content")).and_then(Value::as_str).unwrap_or("").chars().count()
      }
      "image" | "pdf" => 0,
      _ => map_leaves(response, None),
    };
  }
  map_leaves(response, None)
}

/// The characters of the `<persisted-output>` wrapper Claude Code renders around the preview: the
/// framing, the "Output too large" sentence — which carries the byte size and the saved path — and
/// the ellipsis line that follows a cut preview.
fn persisted_wrapper_chars(path: &str, output_bytes: Option<u64>) -> usize {
  let size_text = output_bytes.map_or(String::new(), persisted_size_text);
  "<persisted-output>\n".len()
    + format!("Output too large ({size_text}). Full output saved to: {path}\n\n").chars().count()
    + "Preview (first 2KB):\n".len()
    + "\n...\n".len()
    + "</persisted-output>".len()
}

/// The byte-size text of Claude Code's "Output too large" sentence: kilobytes at base 1024, with
/// one decimal past 10 KB and none below it.
fn persisted_size_text(bytes: u64) -> String {
  let kb = bytes as f64 / 1024.0;
  if kb >= 1024.0 {
    format!("{:.1}MB", kb / 1024.0)
  } else if kb >= 10.0 {
    format!("{kb:.1}KB")
  } else {
    format!("{:.0}KB", kb)
  }
}

/// The preview Claude Code shows of a persisted output: the output's first 2,000 characters, cut
/// back to the last line end when that line end lies past the preview's halfway mark.
fn preview_chars(stdout: &str) -> usize {
  let total = stdout.chars().count();
  if total <= PERSISTED_PREVIEW_CHARS {
    return total;
  }
  let prefix: Vec<char> = stdout.chars().take(PERSISTED_PREVIEW_CHARS).collect();
  match prefix.iter().rposition(|&c| c == '\n') {
    Some(at) if at > PERSISTED_PREVIEW_CHARS / 2 => at,
    _ => PERSISTED_PREVIEW_CHARS,
  }
}

/// The string values of a response object, except base64 data. A map's own key does not reach its
/// values: each value's exclusion key is the name it sits under.
fn map_leaves(map: &Map<String, Value>, _key: Option<&str>) -> usize {
  map.iter().map(|(name, item)| leaves(item, Some(name.as_str()))).sum()
}

/// The string values of one JSON value, except base64 data; arrays and objects recurse.
fn leaves(value: &Value, key: Option<&str>) -> usize {
  match value {
    Value::String(text) => {
      if matches!(key, Some("base64") | Some("data")) {
        0
      } else {
        text.chars().count()
      }
    }
    Value::Array(items) => items.iter().map(|item| leaves(item, key)).sum(),
    Value::Object(map) => map_leaves(map, key),
    _ => 0,
  }
}

/// Read the state, apply one update under the exclusive lock, and write it back when it changed.
fn update_state(run_dir: &Path, update: impl FnOnce(&mut BudgetState)) -> Result<BudgetState, CliExit> {
  let _lock = StateLock::acquire(run_dir)?;
  let path = run_dir.join(STATE_FILE_NAME);
  let mut state = read_state(&path)?;
  update(&mut state);
  let bytes = serde_json::to_vec_pretty(&state)
    .map_err(|error| CliExit::usage(format!("could not write {}: {error}", path.display())))?;
  if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
    std::fs::write(&path, bytes)
      .map_err(|error| CliExit::usage(format!("could not write {}: {error}", path.display())))?;
  }
  Ok(state)
}

fn read_state(path: &Path) -> Result<BudgetState, CliExit> {
  match std::fs::read(path) {
    Ok(bytes) => serde_json::from_slice(&bytes)
      .map_err(|error| CliExit::usage(format!("could not read {}: {error}", path.display()))),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BudgetState::default()),
    Err(error) => Err(CliExit::usage(format!("could not read {}: {error}", path.display()))),
  }
}

/// The exclusive state lock: an advisory `flock` on a file beside the state file, held to the end
/// of the read-modify-write. Parallel tool calls' hooks run concurrently, and each one's update is
/// then atomic.
struct StateLock {
  _file: File,
}

impl StateLock {
  fn acquire(run_dir: &Path) -> Result<Self, CliExit> {
    let path = run_dir.join(LOCK_FILE_NAME);
    let file = OpenOptions::new()
      .create(true)
      .read(true)
      .write(true)
      .truncate(false)
      .open(&path)
      .map_err(|error| CliExit::usage(format!("could not open {}: {error}", path.display())))?;
    // SAFETY: `file` owns a valid descriptor for the duration of the lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
      return Err(CliExit::usage(format!("could not lock {}: {}", path.display(), std::io::Error::last_os_error())));
    }
    Ok(Self { _file: file })
  }
}

macro_rules! hook_pattern {
  ($name:ident, $text:expr) => {
    fn $name() -> &'static Regex {
      static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
      PATTERN.get_or_init(|| Regex::new($text).expect("the exemption pattern is valid"))
    }
  };
}

hook_pattern!(rule_path_pattern, RULE_PATH);
hook_pattern!(approval_path_pattern, APPROVAL_PATH);
hook_pattern!(child_report_pattern, CHILD_REPORT);
hook_pattern!(coordination_cmd_pattern, COORDINATION_CMD);
hook_pattern!(session_cmd_pattern, SESSION_CMD);
hook_pattern!(shell_reader_pattern, SHELL_READERS);

#[cfg(test)]
mod tests {
  use std::io::Cursor;
  use std::path::PathBuf;
  use std::sync::Barrier;

  use serde_json::json;

  use super::*;

  /// A synthetic CharlieBot home with one existing run directory. The temp dir lives as long as
  /// the struct: dropping it deletes the run directory.
  struct RunHome {
    _home: tempfile::TempDir,
    run_dir: PathBuf,
  }

  impl RunHome {
    fn new() -> RunHome {
      let home = tempfile::tempdir().unwrap();
      let run_dir =
        home.path().join("sessions").join("synthetic-session").join("data").join("runs").join("synthetic-run");
      std::fs::create_dir_all(&run_dir).unwrap();
      RunHome { _home: home, run_dir }
    }

    fn state(&self) -> BudgetState {
      serde_json::from_slice(&std::fs::read(self.run_dir.join(STATE_FILE_NAME)).unwrap()).unwrap()
    }

    fn seed_counted(&self, counted: usize) {
      // Keep the rest of the state: the persisted records outlive a seeded total.
      let mut state = match std::fs::read(self.run_dir.join(STATE_FILE_NAME)) {
        Ok(bytes) => serde_json::from_slice::<BudgetState>(&bytes).unwrap(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BudgetState::default(),
        Err(error) => panic!("{error}"),
      };
      state.counted = counted;
      std::fs::write(self.run_dir.join(STATE_FILE_NAME), serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    }
  }

  fn run_event(home: &RunHome, event: &Value) -> (Result<(), CliExit>, Vec<u8>) {
    let mut output = Vec::new();
    let result = run(&mut Cursor::new(serde_json::to_string(event).unwrap().into_bytes()), &home.run_dir, &mut output);
    (result, output)
  }

  fn bash_event(command: impl Into<String>, stdout: impl Into<String>) -> Value {
    json!({
      "hook_event_name": "PostToolUse",
      "tool_name": "Bash",
      "tool_input": {"command": command.into()},
      "tool_use_id": "toolu_01",
      "tool_response": {"stdout": stdout.into(), "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}
    })
  }

  fn read_event(file_path: &str, content: &str) -> Value {
    json!({
      "hook_event_name": "PostToolUse",
      "tool_name": "Read",
      "tool_input": {"file_path": file_path},
      "tool_use_id": "toolu_02",
      "tool_response": {"type": "text", "file": {"filePath": file_path, "content": content}}
    })
  }

  fn decision_of(output: &[u8]) -> Value {
    serde_json::from_slice(output).unwrap()
  }

  /// The withheld notice, sized so the tests read the withheld size and the remaining budget back.
  fn withhold_text(size: usize, left: usize) -> String {
    withhold_notice(size, left)
  }

  #[test]
  fn an_over_budget_bash_output_is_replaced_by_the_withhold_notice_and_counts_nothing() {
    let home = RunHome::new();
    home.seed_counted(10_000);
    let stdout = "x".repeat(6_001);
    let (result, output) = run_event(&home, &bash_event("big", &stdout));
    result.unwrap();
    let decision = decision_of(&output);
    let replaced = &decision["hookSpecificOutput"]["updatedToolOutput"];
    assert_eq!(replaced["stdout"].as_str().unwrap(), withhold_text(6_001, 6_000));
    assert!(replaced["stdout"].as_str().unwrap().starts_with("Output withheld: 6001 characters exceed the 6000"));
    assert_eq!(replaced["stderr"].as_str().unwrap(), "");
    assert_eq!(replaced["interrupted"], json!(false));
    assert_eq!(decision["hookSpecificOutput"]["hookEventName"], json!("PostToolUse"));
    let state = home.state();
    assert_eq!(state.counted, 10_000);
    assert_eq!(state.withheld.len(), 1);
    assert_eq!(state.withheld[0].tool_use_id, "toolu_01");
    assert_eq!(state.withheld[0].chars, 6_001);
    assert_eq!(state.notices, 0);
  }

  #[test]
  fn an_over_budget_bash_output_when_counted_already_exceeds_budget_shows_zero_left() {
    let home = RunHome::new();
    home.seed_counted(20_000);
    let stdout = "x".repeat(500);
    let (result, output) = run_event(&home, &bash_event("after-prior-over-budget", &stdout));
    result.unwrap();
    let decision = decision_of(&output);
    let replaced = &decision["hookSpecificOutput"]["updatedToolOutput"];
    assert_eq!(replaced["stdout"].as_str().unwrap(), withhold_text(500, 0));
    assert!(
      replaced["stdout"].as_str().unwrap().starts_with("Output withheld: 500 characters exceed the 0 characters left")
    );
    assert_eq!(home.state().counted, 20_000);
  }

  #[test]
  fn a_fitting_bash_output_counts_and_prints_nothing() {
    let home = RunHome::new();
    home.seed_counted(10_000);
    let stdout = "x".repeat(6_000);
    let (result, output) = run_event(&home, &bash_event("fitting", &stdout));
    result.unwrap();
    assert!(output.is_empty());
    assert_eq!(home.state().counted, 16_000);
  }

  #[test]
  fn an_exempt_read_of_a_skill_file_passes_uncounted() {
    let home = RunHome::new();
    let (result, output) = run_event(&home, &read_event("/repo/skills/charliebot/SKILL.md", &"y".repeat(20_000)));
    result.unwrap();
    assert!(output.is_empty());
    assert_eq!(home.state().counted, 0);
    assert!(home.state().withheld.is_empty());
  }

  #[test]
  fn an_over_budget_read_is_replaced_in_file_content() {
    let home = RunHome::new();
    let path = "/repo/notes/material.md";
    let (result, output) = run_event(&home, &read_event(path, &"y".repeat(20_000)));
    result.unwrap();
    let decision = decision_of(&output);
    let replaced = &decision["hookSpecificOutput"]["updatedToolOutput"];
    assert_eq!(replaced["file"]["content"].as_str().unwrap(), withhold_text(20_000, 16_000));
    assert_eq!(replaced["file"]["filePath"].as_str().unwrap(), path);
    assert_eq!(home.state().counted, 0);
    assert_eq!(home.state().withheld[0].tool_name, "Read");
  }

  #[test]
  fn an_over_budget_failure_counts_and_appends_the_over_budget_notice() {
    let home = RunHome::new();
    let error = "e".repeat(20_000);
    let event = json!({
      "hook_event_name": "PostToolUseFailure",
      "tool_name": "Bash",
      "tool_input": {"command": "ls /nonexistent_zz_dir"},
      "tool_use_id": "toolu_03",
      "error": error
    });
    let (result, output) = run_event(&home, &event);
    result.unwrap();
    let decision = decision_of(&output);
    assert_eq!(decision["hookSpecificOutput"]["hookEventName"], json!("PostToolUseFailure"));
    let notice = decision["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(notice.starts_with("This turn's material totals 20000 characters, over the 16,000-character"));
    let state = home.state();
    assert_eq!(state.counted, 20_000);
    assert_eq!(state.notices, 1);
  }

  #[test]
  fn an_over_budget_non_withholdable_tool_output_counts_and_appends_the_notice() {
    let home = RunHome::new();
    let event = json!({
      "hook_event_name": "PostToolUse",
      "tool_name": "WebFetch",
      "tool_input": {"url": "https://example.invalid"},
      "tool_use_id": "toolu_04",
      "tool_response": {"result": "w".repeat(20_000)}
    });
    let (result, output) = run_event(&home, &event);
    result.unwrap();
    let decision = decision_of(&output);
    assert!(
      decision["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .starts_with("This turn's material totals 20000 characters")
    );
    assert!(decision["hookSpecificOutput"].get("updatedToolOutput").is_none());
    assert_eq!(home.state().counted, 20_000);
  }

  #[test]
  fn eight_concurrent_small_outputs_all_count() {
    let home = RunHome::new();
    let barrier = std::sync::Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for index in 0..8 {
      let run_dir = home.run_dir.clone();
      let stdout = "z".repeat(1_000);
      let barrier = std::sync::Arc::clone(&barrier);
      handles.push(std::thread::spawn(move || {
        let _ = barrier.wait();
        let event = json!({
          "hook_event_name": "PostToolUse",
          "tool_name": "Bash",
          "tool_input": {"command": format!("echo {index}")},
          "tool_use_id": format!("toolu_{index}"),
          "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}
        });
        let mut output = Vec::new();
        run(&mut Cursor::new(serde_json::to_string(&event).unwrap().into_bytes()), &run_dir, &mut output).unwrap();
        output
      }));
    }
    for handle in handles {
      assert!(handle.join().unwrap().is_empty());
    }
    assert_eq!(home.state().counted, 8_000);
  }

  #[test]
  fn a_compact_session_start_resets_the_counted_total() {
    let home = RunHome::new();
    home.seed_counted(15_000);
    let event = json!({"hook_event_name": "SessionStart", "source": "compact", "session_id": "synthetic"});
    let (result, output) = run_event(&home, &event);
    result.unwrap();
    assert!(output.is_empty());
    assert_eq!(home.state().counted, 0);
    // The budget then starts over: a 14,000-character read fits again.
    let (result, output) = run_event(&home, &read_event("/repo/notes/more.md", &"y".repeat(14_000)));
    result.unwrap();
    assert!(output.is_empty());
    assert_eq!(home.state().counted, 14_000);
  }

  #[test]
  fn a_session_start_without_compact_changes_nothing() {
    let home = RunHome::new();
    home.seed_counted(15_000);
    let event = json!({"hook_event_name": "SessionStart", "source": "resume", "session_id": "synthetic"});
    let (result, output) = run_event(&home, &event);
    result.unwrap();
    assert!(output.is_empty());
    assert_eq!(home.state().counted, 15_000);
  }

  #[test]
  fn invalid_input_missing_run_dir_and_missing_fields_exit_2() {
    let home = RunHome::new();
    let missing = RunHome::new();
    let run_dir = missing.run_dir.clone();
    std::fs::remove_dir(&missing.run_dir).unwrap();
    let no_run_id =
      json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": {}, "tool_response": {}});
    let cases: Vec<(Value, PathBuf, &str)> = vec![
      (json!({"no_event": true}), home.run_dir.clone(), "hook_event_name"),
      (json!({"hook_event_name": "PostToolUse"}), home.run_dir.clone(), "tool_name"),
      (no_run_id, home.run_dir.clone(), "tool_use_id"),
      (json!({"hook_event_name": "SessionStart"}), home.run_dir.clone(), "source"),
      (bash_event("ls", "out"), run_dir, "run directory"),
    ];
    for (event, run_dir, needle) in cases {
      let mut output = Vec::new();
      let error = run(&mut Cursor::new(event.to_string().into_bytes()), &run_dir, &mut output).unwrap_err();
      assert_eq!(error.code, 2, "{needle}");
      assert!(error.body["error"].as_str().unwrap().contains(needle), "{error:?}");
      assert!(output.is_empty());
    }
    let mut output = Vec::new();
    let error = run(&mut Cursor::new(b"not json".to_vec()), &home.run_dir, &mut output).unwrap_err();
    assert_eq!(error.code, 2);
    assert!(error.body["error"].as_str().unwrap().starts_with("invalid hook JSON:"));
  }

  /// The observed shape of a persisted Bash response: the full stdout is truncated in the response
  /// while `persistedOutputPath` and `persistedOutputSize` name the saved full text.
  fn persisted_bash_event(command: &str, stdout: &str, saved_path: &str, output_bytes: u64) -> Value {
    let mut event = bash_event(command, stdout);
    let response = event.get_mut("tool_response").unwrap().as_object_mut().unwrap();
    response.insert("persistedOutputPath".to_string(), json!(saved_path));
    response.insert("persistedOutputSize".to_string(), json!(output_bytes));
    event
  }

  #[test]
  fn a_persisted_bash_output_counts_its_wrapper_and_preview_not_the_whole_stdout() {
    let home = RunHome::new();
    let saved = "/claude/projects/cwd/session/tool-results/b5ddrnipe.txt";
    let stdout = "x".repeat(20_000);
    let (result, output) = run_event(&home, &persisted_bash_event("big-report", &stdout, saved, 157_913));
    result.unwrap();
    assert!(output.is_empty(), "the preview fits the fresh budget");
    // The wrapper (framing, size sentence with the saved path, ellipsis line) plus the 2,000-char
    // preview, and not the 20,000-character stdout.
    let expected = persisted_wrapper_chars(saved, Some(157_913)) + 2_000;
    assert_eq!(home.state().counted, expected);
    assert_eq!(home.state().persisted.len(), 1);
    assert_eq!(home.state().persisted[0].path, saved);
    assert_eq!(home.state().persisted[0].exempt, None);
  }

  #[test]
  fn a_persisted_preview_cut_backs_to_the_last_line_end_past_halfway() {
    // A preview whose 2,000-character window holds a line end past the halfway mark cuts there,
    // before the line end itself.
    let line = "a".repeat(1_500);
    let stdout = format!("{line}\n{}", "b".repeat(20_000));
    assert_eq!(preview_chars(&stdout), 1_500);
    // Without such a line end the window holds the full 2,000 characters.
    assert_eq!(preview_chars(&"b".repeat(20_000)), 2_000);
    // A short output is its own preview.
    assert_eq!(preview_chars("short"), 5);
    // Multibyte UTF-8 characters: line end past halfway in character count (1,500 chars > 1,000).
    let prefix = "中".repeat(1_500);
    let stdout = format!("{prefix}\n{}", "中".repeat(2_000));
    assert_eq!(preview_chars(&stdout), 1_500);
    // Multibyte UTF-8 characters: line end before halfway in character count (500 chars <= 1,000)
    // keeps the full 2,000 characters even though byte offset (1,500) exceeds 1,000.
    let prefix = "中".repeat(500);
    let stdout = format!("{prefix}\n{}", "中".repeat(2_000));
    assert_eq!(preview_chars(&stdout), 2_000);
  }

  #[test]
  fn a_read_of_a_recorded_persisted_path_is_its_own_counted_read() {
    let home = RunHome::new();
    let saved = "/claude/projects/cwd/session/tool-results/bnf8vgdna.txt";
    let stdout = "x".repeat(20_000);
    let (result, output) = run_event(&home, &persisted_bash_event("big-grep", &stdout, saved, 36_791));
    result.unwrap();
    assert!(output.is_empty());
    let counted_after_persist = home.state().counted;
    // The read-back arrives over budget, so it is withheld like any counted read.
    home.seed_counted(15_500);
    let (result, output) = run_event(&home, &read_event(saved, &"y".repeat(1_000)));
    result.unwrap();
    let decision = decision_of(&output);
    assert_eq!(
      decision["hookSpecificOutput"]["updatedToolOutput"]["file"]["content"].as_str().unwrap(),
      withhold_text(1_000, 500)
    );
    assert_eq!(home.state().counted, 15_500);
    // The record of the source call stays.
    assert_eq!(home.state().persisted[0].path, saved);
    assert_eq!(counted_after_persist, persisted_wrapper_chars(saved, Some(36_791)) + 2_000);
  }

  #[test]
  fn a_read_of_a_recorded_persisted_path_keeps_its_source_exemption() {
    let home = RunHome::new();
    let plan = "/charliebot/sessions/some-session/artifacts/plan_manager-direct-read-check_v3.html";
    let saved = "/claude/projects/cwd/session/tool-results/b805t93n6.txt";
    let command = format!("python3 -c 'extract text' {plan}");
    let stdout = "x".repeat(20_000);
    let (result, output) = run_event(&home, &persisted_bash_event(&command, &stdout, saved, 50_000));
    result.unwrap();
    assert!(output.is_empty(), "an exempt source counts nothing");
    assert_eq!(home.state().counted, 0);
    assert_eq!(home.state().persisted[0].exempt.as_deref(), Some("approval_object"));
    // The read-back of the exempt source stays readable at any size.
    let (result, output) = run_event(&home, &read_event(saved, &"y".repeat(20_000)));
    result.unwrap();
    assert!(output.is_empty());
    assert_eq!(home.state().counted, 0);
    assert!(home.state().withheld.is_empty());
  }

  #[test]
  fn a_non_persisted_bash_output_counts_stdout_and_stderr() {
    let home = RunHome::new();
    let mut event = bash_event("echo hi", "s".repeat(100));
    event["tool_response"]["stderr"] = json!("w".repeat(50));
    let (result, output) = run_event(&home, &event);
    result.unwrap();
    assert!(output.is_empty());
    assert_eq!(home.state().counted, 150);
  }

  #[test]
  fn exempt_classes_match_the_direct_work_items() {
    let plan = "/charliebot/sessions/s/artifacts/understanding_topic_v2.html";
    let cases: Vec<(&str, Value, Option<&str>)> = vec![
      ("Agent", json!({}), Some("child_conclusion")),
      ("Skill", json!({"skill": "charliebot"}), Some("rule_skill")),
      ("Read", json!({"file_path": "/repo/skills/x/SKILL.md"}), Some("rule_skill")),
      ("Read", json!({"file_path": "/repo/prompts/master.md"}), Some("rule_skill")),
      ("Read", json!({"file_path": "/repo/CLAUDE.md"}), Some("rule_skill")),
      ("Read", json!({"file_path": plan}), Some("approval_object")),
      ("Read", json!({"file_path": "/repo/src/main.rs"}), None),
      ("Read", json!({"file_path": "/home/sessions/s/data/runs/r/report.md"}), Some("child_conclusion")),
      ("Bash", json!({"command": "charliebot session tree --root s"}), Some("status_decision")),
      ("Bash", json!({"command": "charliebot session dialog s"}), None),
      ("Bash", json!({"command": "charliebot taste query --domain app"}), Some("status_decision")),
      ("Bash", json!({"command": "cat /repo/CLAUDE.md"}), Some("rule_skill")),
      // A reader's selection of a rule file counts: its output is a selection, not the material.
      ("Bash", json!({"command": "grep -n budget /repo/CLAUDE.md"}), None),
      ("Bash", json!({"command": "git log -- prompts/"}), None),
      ("Bash", json!({"command": format!("cat {plan}")}), Some("approval_object")),
      ("Bash", json!({"command": "make test"}), None),
      ("Write", json!({"file_path": "/repo/x.rs"}), None),
    ];
    for (tool_name, tool_input, expected) in cases {
      let input = tool_input.as_object().unwrap().clone();
      assert_eq!(exempt_class(tool_name, &input).as_deref(), expected, "{tool_name} {tool_input}");
    }
  }

  #[test]
  fn the_write_tools_and_base64_data_count_nothing() {
    let home = RunHome::new();
    let event = json!({
      "hook_event_name": "PostToolUse",
      "tool_name": "Write",
      "tool_input": {"file_path": "/repo/x.rs", "content": "x".repeat(20_000)},
      "tool_use_id": "toolu_05",
      "tool_response": {"filesEdited": ["/repo/x.rs"]}
    });
    let (result, output) = run_event(&home, &event);
    result.unwrap();
    assert!(output.is_empty());
    assert_eq!(home.state().counted, 0);

    let event = json!({
      "hook_event_name": "PostToolUse",
      "tool_name": "Read",
      "tool_input": {"file_path": "/repo/picture.png"},
      "tool_use_id": "toolu_06",
      "tool_response": {"type": "image", "file": {"filePath": "/repo/picture.png", "base64": "iVBOR".repeat(4_000)}}
    });
    let (result, output) = run_event(&home, &event);
    result.unwrap();
    assert!(output.is_empty());
    assert_eq!(home.state().counted, 0);
  }

  #[test]
  fn the_persisted_wrapper_text_matches_the_observed_rendering() {
    // The observed 154.2KB output: its 211-character saved path, its 2,000-character preview and
    // its 141-character shell-cwd note render to a 2,468-character model-visible tool result.
    // The observed output's saved path is 211 characters; its slug is synthetic here.
    let path = format!("/claude/projects/{}/tool-results/b5ddrnipe.txt", "x".repeat(167));
    assert_eq!(path.len(), 211);
    let wrapper = persisted_wrapper_chars(&path, Some(157_913));
    assert_eq!(wrapper + 2_000 + 141, 2_468);
    assert_eq!(persisted_size_text(157_913), "154.2KB");
    assert_eq!(persisted_size_text(36_791), "35.9KB");
    assert_eq!(persisted_size_text(115_815), "113.1KB");
    assert_eq!(persisted_size_text(2_000), "2KB");
  }
}
