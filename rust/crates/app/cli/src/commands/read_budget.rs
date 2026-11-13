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
use charliebot_core::event_types::{
  self, EDIT_TOOL, MULTI_EDIT_TOOL, NOTEBOOK_EDIT_TOOL, READ_TOOL, SKILL_TOOL, TOOL_INPUT_FILE_PATH, WRITE_TOOL,
};
use charliebot_core::home;
use charliebot_core::posix_path;
use charliebot_core::subagent::SUBAGENT_TOOL_NAMES;
use charliebot_store::layout;
use charliebot_store::run_records::{RAW_LOG_NAME, RUN_EVENTS_NAME};
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
const WITHHOLDABLE_TOOLS: [&str; 2] = ["Bash", READ_TOOL];

/// The tools whose `tool_response` echoes written content the model never reads, so they count 0.
const WRITING_TOOLS: [&str; 4] = [WRITE_TOOL, EDIT_TOOL, MULTI_EDIT_TOOL, NOTEBOOK_EDIT_TOOL];

/// Claude Code's persisted-output preview holds at most this many characters of the output ("first
/// 2KB"), cut back to the last line end when that line end lies past the preview's halfway mark.
const PERSISTED_PREVIEW_CHARS: usize = 2000;

/// The paths of rule and skill material, whose reads Direct Work keeps outside the budget.
const RULE_PATH: &str = r"(/skills/|(^|[\s:/])prompts/|SKILL\.md|LESSONS\.md|CLAUDE\.md|AGENTS\.md|\.claude/rules/)";

/// The approval objects under a session's artifacts directory.
const APPROVAL_PATH: &str = r"/artifacts/(plan|understanding)_[^/\s]*\.html";

/// A child's conclusion: a run directory's report or findings file.
const CHILD_REPORT: &str = r"/data/runs/[^/\s]+/(report|findings)\.md";

/// CharlieBot's own coordination surface: session status and tree, child and wake records,
/// delegation and trigger receipts, plans, digest, decisions (taste, memory), page checks.
/// `charliebot session dialog` reads another session's record, and the integration subcommands read
/// outside material, so both count. The session arm excludes `dialog` and `conclusion` by their
/// captured subcommand words.
const COORDINATION_CMD: &str =
  r"\bcharliebot\s+(delegate|worker|plan|digest|taste|memory|web|artifact|publish|improve)\b";
const SESSION_CMD: &str = r"\bcharliebot\s+session\s+([\w-]+)";
const SESSION_DIALOG: &str = "dialog";
const SESSION_CONCLUSION: &str = "conclusion";

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
  /// This run's Claude process carries the Agent tool
  #[arg(long)]
  agent_tool: bool,
  /// The delegation gate check_task_authorization passes for this session
  #[arg(long)]
  can_delegate: bool,
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
  run(&mut std::io::stdin().lock(), &run_dir, cli.agent_tool, cli.can_delegate, &mut output)
}

fn run<R: Read, W: Write>(
  input: &mut R,
  run_dir: &Path,
  agent_tool: bool,
  can_delegate: bool,
  output: &mut W,
) -> Result<(), CliExit> {
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
      tool_result(&event, run_dir, agent_tool, can_delegate)?
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
fn tool_result(event: &Value, run_dir: &Path, agent_tool: bool, can_delegate: bool) -> Result<Option<Value>, CliExit> {
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
  let cwd = event.get("cwd").and_then(Value::as_str);
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
      state.persisted.push(PersistedOutput { path: path.clone(), exempt: exempt_class(tool_name, tool_input, cwd) });
    }
    let exempt =
      read_back_exemption(tool_name, tool_input, state).unwrap_or_else(|| exempt_class(tool_name, tool_input, cwd));
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
        let notice = withhold_notice(size, BUDGET_CHARS.saturating_sub(state.counted), agent_tool, can_delegate);
        decision = Some(withhold_decision(tool_name, response, &notice));
        return;
      }
      state.notices += 1;
      let notice = over_budget_notice(state.counted + size, agent_tool, can_delegate);
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
  if tool_name != READ_TOOL {
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

fn withhold_notice(size: usize, left: usize, agent_tool: bool, can_delegate: bool) -> String {
  let handoff = match (agent_tool, can_delegate) {
    (true, true) => {
      "To read all of it, hand the read to a subagent (the `Agent` tool) or to a repo-less worker with `--task-type script-run`."
    }
    (true, false) => "To read all of it, hand the read to a subagent (the `Agent` tool).",
    (false, true) => "To read all of it, hand the read to a repo-less worker with `--task-type script-run`.",
    (false, false) => "The runtime refuses a worker handoff on this line, so read large material in capped parts.",
  };
  format!(
    "Output withheld: {size} characters exceed the {left} characters left in this turn's 16,000-character \
    small-material budget (Direct Work, prompts/master.md). The call already ran; only its output is withheld. \
    To read part of it yourself, rerun the read capped within {left} characters. {handoff}\n\
    The budget does not count these reads of decision material:\n\
    - a child's conclusion, through `charliebot session conclusion --session <child id>`;\n\
    - the user's words, through `charliebot session dialog --from user`;\n\
    - a plan or understanding page through its file under artifacts/, and a rule or skill file read whole."
  )
}

fn over_budget_notice(total: usize, agent_tool: bool, can_delegate: bool) -> String {
  let handoff = match (agent_tool, can_delegate) {
    (true, true) => {
      " When you act as a lead, hand each further read of large material to a subagent (the `Agent` tool) or to a repo-less worker with `--task-type script-run`."
    }
    (true, false) => {
      " When you act as a lead, hand each further read of large material to a subagent (the `Agent` tool)."
    }
    (false, true) => {
      " When you act as a lead, hand each further read of large material to a repo-less worker with `--task-type script-run`."
    }
    (false, false) => "",
  };
  format!(
    "This turn's material totals {total} characters, over the 16,000-character small-material budget of Direct \
    Work (prompts/master.md). Each later model call re-reads this material. Cap each further read by its form.{handoff}"
  )
}

/// The Direct Work item a tool call reads, or `None` when its output counts toward the budget.
fn exempt_class(tool_name: &str, tool_input: &Map<String, Value>, cwd: Option<&str>) -> Option<String> {
  let class = match tool_name {
    name if SUBAGENT_TOOL_NAMES.contains(&name) => Some("child_conclusion"),
    SKILL_TOOL => Some("rule_skill"),
    READ_TOOL => tool_input.get(TOOL_INPUT_FILE_PATH).and_then(Value::as_str).and_then(|p| path_class(p, cwd)),
    "Bash" => tool_input.get("command").and_then(Value::as_str).and_then(|c| bash_class(c, cwd)),
    _ => None,
  };
  class.map(str::to_string)
}

fn path_class(path: &str, cwd: Option<&str>) -> Option<&'static str> {
  if child_report_pattern().is_match(path) {
    return Some("child_conclusion");
  }
  if rule_path_pattern().is_match(path) {
    return Some("rule_skill");
  }
  if approval_path_pattern().is_match(path) {
    return Some("approval_object");
  }
  let resolved = resolve_token(path, cwd)?;
  if report_resolved_pattern().is_match(&resolved) {
    return Some("child_conclusion");
  }
  if approval_resolved_pattern().is_match(&resolved) {
    return Some("approval_object");
  }
  None
}

fn bash_class(command: &str, cwd: Option<&str>) -> Option<&'static str> {
  if is_standalone_conclusion(command, cwd) {
    return Some("child_conclusion");
  }
  if is_standalone_user_dialog(command, cwd) {
    return Some("user_words");
  }
  if status_decision(command) {
    return Some("status_decision");
  }
  // A run log is never a decision read: a command that names one counts, whatever else it names.
  if names_run_log(command) {
    return None;
  }
  if child_report_pattern().is_match(command) {
    return Some("child_conclusion");
  }
  if rule_path_pattern().is_match(command) && !shell_reader_pattern().is_match(command) {
    return Some("rule_skill");
  }
  if approval_path_pattern().is_match(command) {
    return Some("approval_object");
  }
  let names = resolved_names(command, cwd);
  if let Some(candidate) = resolved_class(&names) {
    let existing_paths = named_existing_paths(command, cwd);
    if existing_paths.iter().all(|p| is_decision_path(p)) {
      return Some(candidate);
    }
  }
  None
}

/// Whether the command is one of CharlieBot's coordination commands: any of the coordination verbs,
/// or a `charliebot session` subcommand that is not `dialog` or `conclusion`.
fn status_decision(command: &str) -> bool {
  if coordination_cmd_pattern().is_match(command) {
    return true;
  }
  session_cmd_pattern().captures_iter(command).any(|captures| {
    captures.get(1).is_some_and(|word| word.as_str() != SESSION_DIALOG && word.as_str() != SESSION_CONCLUSION)
  })
}

/// Whether the command has shell syntax beyond one CharlieBot command piped into filters: a list or
/// background operator (`;`, `&`, `||`), a newline, a redirection in (`<`), or a subshell or substitution
/// (`(`, `)`, a backtick). Each one can run a second read beside the exempt one.
fn has_shell_control(command: &str) -> bool {
  command.contains(['&', ';', '<', '(', ')', '`', '\n']) || command.contains("||")
}

/// Whether the command names a run log, `events.jsonl` or `agent.raw.ndjson`. Quotes and backslashes go
/// first, as the shell would remove them, so `ev""ents.jsonl` names one too.
fn names_run_log(command: &str) -> bool {
  let words: String = command.chars().filter(|c| !matches!(c, '\'' | '"' | '\\')).collect();
  words.contains(RUN_EVENTS_NAME) || words.contains(RAW_LOG_NAME)
}

fn is_standalone_conclusion(command: &str, cwd: Option<&str>) -> bool {
  if has_shell_control(command) {
    return false;
  }
  let stages: Vec<&str> = command.split('|').map(str::trim).collect();
  if stages.is_empty() {
    return false;
  }
  let words: Vec<&str> = stages[0].split_whitespace().collect();
  if words.len() < 3 || words[0] != "charliebot" || words[1] != "session" || words[2] != "conclusion" {
    return false;
  }
  validate_pipeline_filters(&stages[1..], cwd)
}

fn is_standalone_user_dialog(command: &str, cwd: Option<&str>) -> bool {
  if has_shell_control(command) {
    return false;
  }
  let stages: Vec<&str> = command.split('|').map(str::trim).collect();
  if stages.is_empty() {
    return false;
  }
  let words: Vec<&str> = stages[0].split_whitespace().collect();
  if words.len() < 3 || words[0] != "charliebot" || words[1] != "session" || words[2] != "dialog" {
    return false;
  }
  let has_from_user = words.windows(2).any(|w| w[0] == "--from" && w[1] == "user") || words.contains(&"--from=user");
  if !has_from_user {
    return false;
  }
  validate_pipeline_filters(&stages[1..], cwd)
}

fn validate_pipeline_filters(stages: &[&str], cwd: Option<&str>) -> bool {
  const ALLOWED_FILTERS: [&str; 7] = ["head", "tail", "grep", "rg", "sed", "cut", "wc"];
  for stage in stages {
    let tokens: Vec<&str> = stage.split_whitespace().collect();
    if tokens.is_empty() {
      return false;
    }
    if !ALLOWED_FILTERS.contains(&tokens[0]) {
      return false;
    }
    for arg in &tokens[1..] {
      let trimmed = arg.trim_matches(['\'', '"', '`']);
      if trimmed.starts_with('-') {
        continue;
      }
      // A glob, a brace or a variable names files the hook cannot list, so the filter's read counts.
      if trimmed.contains(['*', '?', '[', '{', '$']) {
        return false;
      }
      if let Some(resolved) = resolve_token(trimmed, cwd)
        && resolved != "/"
        && Path::new(&resolved).exists()
      {
        return false;
      }
    }
  }
  true
}

fn resolve_token(tok: &str, base: Option<&str>) -> Option<String> {
  let home = std::env::var("HOME").unwrap_or_default();
  let cb_home = std::env::var(home::CHARLIEBOT_HOME_ENV).unwrap_or_default();
  let mut s = tok.to_string();
  for (var, val) in [
    ("${CHARLIEBOT_HOME}", cb_home.as_str()),
    (home::DISPLAY_HOME, cb_home.as_str()),
    ("${HOME}", home.as_str()),
    ("$HOME", home.as_str()),
  ] {
    if s.starts_with(var) {
      s = format!("{}{}", val, &s[var.len()..]);
    }
  }
  if s.starts_with("~/") {
    s = format!("{}{}", home, &s[1..]);
  }
  if s.contains('$') {
    return None;
  }
  if !s.starts_with('/') {
    let base = base?;
    s = format!("{base}/{s}");
  }
  Some(posix_path::normpath(&s))
}

fn is_decision_path(path: &str) -> bool {
  approval_resolved_pattern().is_match(path)
    || report_resolved_pattern().is_match(path)
    || rule_path_pattern().is_match(path)
}

fn resolved_names(command: &str, cwd: Option<&str>) -> Vec<String> {
  let mut base = cwd.map(str::to_string);
  let mut out = Vec::new();
  for seg in segment_split_pattern().split(command) {
    if let Some(captures) = cd_pattern().captures(seg)
      && let Some(target) = captures.get(1)
    {
      let nb = resolve_token(target.as_str(), base.as_deref());
      base = nb;
      continue;
    }
    for m in token_split_pattern().find_iter(seg) {
      let tok = m.as_str();
      if named_pattern().is_match(tok)
        && let Some(r) = resolve_token(tok, base.as_deref())
      {
        out.push(r);
      }
    }
  }
  out
}

fn named_existing_paths(command: &str, cwd: Option<&str>) -> Vec<String> {
  let mut base = cwd.map(str::to_string);
  let mut out = Vec::new();
  for seg in segment_split_pattern().split(command) {
    if let Some(captures) = cd_pattern().captures(seg)
      && let Some(target) = captures.get(1)
    {
      let nb = resolve_token(target.as_str(), base.as_deref());
      base = nb;
      continue;
    }
    for m in token_split_pattern().find_iter(seg) {
      let tok = m.as_str();
      if let Some(r) = resolve_token(tok, base.as_deref())
        && r != "/"
        && Path::new(&r).exists()
      {
        out.push(r);
      }
    }
  }
  out
}

fn resolved_class(paths: &[String]) -> Option<&'static str> {
  if paths.iter().any(|p| report_resolved_pattern().is_match(p)) {
    return Some("child_conclusion");
  }
  if paths.iter().any(|p| approval_resolved_pattern().is_match(p)) {
    return Some("approval_object");
  }
  None
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
  if tool_name == READ_TOOL {
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
hook_pattern!(approval_resolved_pattern, r"/artifacts/(plan|understanding)_[^/]*\.html$");
hook_pattern!(report_resolved_pattern, r"/data/runs/[^/]+/(report|findings)\.md$");
hook_pattern!(named_pattern, r"((plan|understanding)_[^/]*\.html$|(report|findings)\.md$)");
hook_pattern!(cd_pattern, r"^\s*cd\s+([^\s]+)\s*$");
hook_pattern!(segment_split_pattern, r"&&|\|\||;|\n|\|");
hook_pattern!(token_split_pattern, r#"[^\s'"'`|;&<>()=,]+"#);

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
    run_event_with_flags(home, event, false, false)
  }

  fn run_event_with_flags(
    home: &RunHome,
    event: &Value,
    agent_tool: bool,
    can_delegate: bool,
  ) -> (Result<(), CliExit>, Vec<u8>) {
    let mut output = Vec::new();
    let result = run(
      &mut Cursor::new(serde_json::to_string(event).unwrap().into_bytes()),
      &home.run_dir,
      agent_tool,
      can_delegate,
      &mut output,
    );
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

  fn bash_event_with_cwd(command: impl Into<String>, stdout: impl Into<String>, cwd: &str) -> Value {
    json!({
      "hook_event_name": "PostToolUse",
      "tool_name": "Bash",
      "cwd": cwd,
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

  fn read_event_with_cwd(file_path: &str, content: &str, cwd: &str) -> Value {
    json!({
      "hook_event_name": "PostToolUse",
      "tool_name": "Read",
      "cwd": cwd,
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
    withhold_notice(size, left, false, false)
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
        run(&mut Cursor::new(serde_json::to_string(&event).unwrap().into_bytes()), &run_dir, false, false, &mut output).unwrap();
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
      let error =
        run(&mut Cursor::new(event.to_string().into_bytes()), &run_dir, false, false, &mut output).unwrap_err();
      assert_eq!(error.code, 2, "{needle}");
      assert!(error.body["error"].as_str().unwrap().contains(needle), "{error:?}");
      assert!(output.is_empty());
    }
    let mut output = Vec::new();
    let error = run(&mut Cursor::new(b"not json".to_vec()), &home.run_dir, false, false, &mut output).unwrap_err();
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
      assert_eq!(exempt_class(tool_name, &input, None).as_deref(), expected, "{tool_name} {tool_input}");
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

  #[test]
  fn notice_variants_match_section_4_1_word_for_word() {
    let size = 20_000;
    let left = 1_000;
    let total = 20_000;

    // Variant 1: Neither flag
    let v1_withhold = withhold_notice(size, left, false, false);
    let expected_v1_withhold = "\
Output withheld: 20000 characters exceed the 1000 characters left in this turn's 16,000-character small-material budget (Direct Work, prompts/master.md). The call already ran; only its output is withheld. To read part of it yourself, rerun the read capped within 1000 characters. The runtime refuses a worker handoff on this line, so read large material in capped parts.
The budget does not count these reads of decision material:
- a child's conclusion, through `charliebot session conclusion --session <child id>`;
- the user's words, through `charliebot session dialog --from user`;
- a plan or understanding page through its file under artifacts/, and a rule or skill file read whole.";
    assert_eq!(v1_withhold, expected_v1_withhold);

    let v1_over_budget = over_budget_notice(total, false, false);
    let expected_v1_over_budget = "\
This turn's material totals 20000 characters, over the 16,000-character small-material budget of Direct Work (prompts/master.md). Each later model call re-reads this material. Cap each further read by its form.";
    assert_eq!(v1_over_budget, expected_v1_over_budget);

    // Variant 2: Only --can-delegate
    let v2_withhold = withhold_notice(size, left, false, true);
    let expected_v2_withhold = "\
Output withheld: 20000 characters exceed the 1000 characters left in this turn's 16,000-character small-material budget (Direct Work, prompts/master.md). The call already ran; only its output is withheld. To read part of it yourself, rerun the read capped within 1000 characters. To read all of it, hand the read to a repo-less worker with `--task-type script-run`.
The budget does not count these reads of decision material:
- a child's conclusion, through `charliebot session conclusion --session <child id>`;
- the user's words, through `charliebot session dialog --from user`;
- a plan or understanding page through its file under artifacts/, and a rule or skill file read whole.";
    assert_eq!(v2_withhold, expected_v2_withhold);

    let v2_over_budget = over_budget_notice(total, false, true);
    let expected_v2_over_budget = "\
This turn's material totals 20000 characters, over the 16,000-character small-material budget of Direct Work (prompts/master.md). Each later model call re-reads this material. Cap each further read by its form. When you act as a lead, hand each further read of large material to a repo-less worker with `--task-type script-run`.";
    assert_eq!(v2_over_budget, expected_v2_over_budget);

    // Variant 3: Both flags (--agent-tool and --can-delegate)
    let v3_withhold = withhold_notice(size, left, true, true);
    let expected_v3_withhold = "\
Output withheld: 20000 characters exceed the 1000 characters left in this turn's 16,000-character small-material budget (Direct Work, prompts/master.md). The call already ran; only its output is withheld. To read part of it yourself, rerun the read capped within 1000 characters. To read all of it, hand the read to a subagent (the `Agent` tool) or to a repo-less worker with `--task-type script-run`.
The budget does not count these reads of decision material:
- a child's conclusion, through `charliebot session conclusion --session <child id>`;
- the user's words, through `charliebot session dialog --from user`;
- a plan or understanding page through its file under artifacts/, and a rule or skill file read whole.";
    assert_eq!(v3_withhold, expected_v3_withhold);

    let v3_over_budget = over_budget_notice(total, true, true);
    let expected_v3_over_budget = "\
This turn's material totals 20000 characters, over the 16,000-character small-material budget of Direct Work (prompts/master.md). Each later model call re-reads this material. Cap each further read by its form. When you act as a lead, hand each further read of large material to a subagent (the `Agent` tool) or to a repo-less worker with `--task-type script-run`.";
    assert_eq!(v3_over_budget, expected_v3_over_budget);

    // Bonus: Only --agent-tool
    let v4_withhold = withhold_notice(size, left, true, false);
    assert!(v4_withhold.contains("To read all of it, hand the read to a subagent (the `Agent` tool)."));
    let v4_over_budget = over_budget_notice(total, true, false);
    assert!(
      v4_over_budget
        .contains("When you act as a lead, hand each further read of large material to a subagent (the `Agent` tool).")
    );
  }

  #[test]
  fn path_forms_and_mixed_commands_match_section_4_2() {
    let temp_session = tempfile::tempdir().unwrap();
    let session_path = temp_session.path();
    let artifacts_dir = session_path.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).unwrap();
    let plan_file = artifacts_dir.join("plan_x.html");
    std::fs::write(&plan_file, "plan content").unwrap();
    let und_file = artifacts_dir.join("understanding_x.html");
    std::fs::write(&und_file, "understanding content").unwrap();
    let specs_dir = session_path.join("specs");
    std::fs::create_dir_all(&specs_dir).unwrap();

    let cwd_str = session_path.to_str().unwrap();

    // 1. Relative path `sed -n 1,9p artifacts/plan_x.html` (cwd is session dir) is exempt
    assert_eq!(bash_class("sed -n 1,9p artifacts/plan_x.html", Some(cwd_str)), Some("approval_object"));

    // 2. `cd /x/artifacts && cat plan_y.html` (cwd in other place) is exempt
    assert_eq!(bash_class("cd /x/artifacts && cat plan_y.html", Some("/other/cwd")), Some("approval_object"));

    // 3. `cat tmp/plan_v1.html` is counted (tmp is not artifacts/)
    assert_eq!(bash_class("cat tmp/plan_v1.html", Some(cwd_str)), None);

    // 4. `cat $d/report.md` is counted (unexpanded variable)
    assert_eq!(bash_class("cat $d/report.md", Some(cwd_str)), None);

    // 5. `cd /s/data/runs/r && cat report.md` is exempt
    assert_eq!(bash_class("cd /s/data/runs/r && cat report.md", Some(cwd_str)), Some("child_conclusion"));

    // 6. `wc -c artifacts/understanding_x.html; ls specs` (specs exists on disk) is counted
    assert_eq!(bash_class("wc -c artifacts/understanding_x.html; ls specs", Some(cwd_str)), None);

    // 7. `charliebot session conclusion --session s` is exempt
    assert_eq!(bash_class("charliebot session conclusion --session s", Some(cwd_str)), Some("child_conclusion"));

    // 8. `charliebot session conclusion --session s | head -c 3000` is exempt
    assert_eq!(
      bash_class("charliebot session conclusion --session s | head -c 3000", Some(cwd_str)),
      Some("child_conclusion")
    );

    // 9. `charliebot session conclusion --session s; cat events.jsonl` is counted
    assert_eq!(bash_class("charliebot session conclusion --session s; cat events.jsonl", Some(cwd_str)), None);

    // 10. `jq select(.type=="result")` is counted (old exemption deleted)
    assert_eq!(
      bash_class(
        r#"jq -r 'select(.type=="result") | .result' /home/sessions/s/data/runs/r/events.jsonl"#,
        Some(cwd_str)
      ),
      None
    );

    // 11. `charliebot session dialog --from user` is exempt
    assert_eq!(bash_class("charliebot session dialog --from user", Some(cwd_str)), Some("user_words"));
    assert_eq!(bash_class("charliebot session dialog s --from user | head -n 20", Some(cwd_str)), Some("user_words"));

    // 12. `charliebot session dialog --turns 5` is counted
    assert_eq!(bash_class("charliebot session dialog --turns 5", Some(cwd_str)), None);
  }

  #[test]
  fn a_command_that_reads_beyond_the_named_read_counts() {
    let temp_session = tempfile::tempdir().unwrap();
    let session_path = temp_session.path();
    let artifacts_dir = session_path.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).unwrap();
    std::fs::write(artifacts_dir.join("plan_x.html"), "plan content").unwrap();
    let cwd_str = session_path.to_str().unwrap();

    // A background list, a substitution, a redirection in: each runs a second read beside the exempt one.
    assert_eq!(bash_class("charliebot session conclusion --session s & cat /abs/data/x.md", Some(cwd_str)), None);
    assert_eq!(bash_class("charliebot session conclusion --session \"$(cat /abs/x)\"", Some(cwd_str)), None);
    assert_eq!(
      bash_class("charliebot session dialog --from user | head -c 100 </abs/events.jsonl", Some(cwd_str)),
      None
    );
    // A glob in a filter's file argument names files the hook cannot check.
    assert_eq!(
      bash_class("charliebot session dialog --from user | head /abs/data/runs/*/events.jsonl", Some(cwd_str)),
      None
    );
    // A run log named beside a decision read, a child report or a rule file counts whole, glob or not.
    assert_eq!(bash_class("cat artifacts/plan_x.html; cat /abs/data/runs/*/events.jsonl", Some(cwd_str)), None);
    assert_eq!(bash_class("cat /abs/data/runs/r/report.md; cat /abs/data/runs/r/events.jsonl", Some(cwd_str)), None);
    assert_eq!(bash_class("cat /repo/CLAUDE.md; cat /abs/data/runs/r/agent.raw.ndjson", Some(cwd_str)), None);
    assert_eq!(bash_class("cat artifacts/plan_x.html; cat ev\"\"ents.jsonl", Some(cwd_str)), None);
    // The plain forms still exempt.
    assert_eq!(
      bash_class("charliebot session conclusion --session s | head -c 3000", Some(cwd_str)),
      Some("child_conclusion")
    );
    assert_eq!(bash_class("cat artifacts/plan_x.html", Some(cwd_str)), Some("approval_object"));
  }

  #[test]
  fn replay_fixtures_match_table_in_section_4_2() {
    let temp_session = tempfile::tempdir().unwrap();
    let session_path = temp_session.path();
    let artifacts_dir = session_path.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).unwrap();
    let plan_02 = artifacts_dir.join("plan_02_v4.html");
    std::fs::write(&plan_02, "plan content").unwrap();
    let und_file = artifacts_dir.join("understanding_reset_aware_lb_v1.html");
    std::fs::write(&und_file, "understanding content").unwrap();

    let specs_dir = session_path.join("specs").join("v6");
    std::fs::create_dir_all(&specs_dir).unwrap();

    let cwd_str = session_path.to_str().unwrap();

    // R10: f3286173
    // `sed -n '249,269p;331,354p' artifacts/plan_02_v4.html | head -c 12000`
    let r10_cmd = "sed -n '249,269p;331,354p' artifacts/plan_02_v4.html | head -c 12000";
    assert_eq!(bash_class(r10_cmd, Some(cwd_str)), Some("approval_object"));

    // R55: 5d3fbd02 raw material
    let r55_cmd =
      "ls ~/scripts/20261109_quota_reset_replay/; wc -c ~/scripts/report.md; grep -n '^#' ~/scripts/report.md";
    assert_eq!(bash_class(r55_cmd, Some(cwd_str)), None);

    // 0ad72837: mixed command with existing specs/v6
    let cmd_0ad72837 = "wc -c artifacts/understanding_reset_aware_lb_v1.html; ls specs/v6; grep -c LESSONS LESSONS.md";
    assert_eq!(bash_class(cmd_0ad72837, Some(cwd_str)), None);

    // Full hook event execution for R10, R55, and 0ad72837
    let home = RunHome::new();
    home.seed_counted(12_226); // 16,000 - 3,774 left

    // R10 passes without being counted because it is exempt (approval_object)
    let (res, out) =
      run_event_with_flags(&home, &bash_event_with_cwd(r10_cmd, "x".repeat(4_610), cwd_str), false, true);
    res.unwrap();
    assert!(out.is_empty(), "R10 is exempt and passes unwithheld");
    assert_eq!(home.state().counted, 12_226);

    // R55: counted was 12,439 in replay, 1,487 fits within budget (12,439 + 1,487 = 13,926 <= 16,000)
    home.seed_counted(12_439);
    let (res, out) =
      run_event_with_flags(&home, &bash_event_with_cwd(r55_cmd, "x".repeat(1_487), cwd_str), false, true);
    res.unwrap();
    assert!(out.is_empty(), "R55 fits within budget and passes unwithheld");
    assert_eq!(home.state().counted, 13_926);

    // 0ad72837: 3,433 characters exceed remaining budget (left: 2,208) -> withheld
    home.seed_counted(13_792); // 16,000 - 2,208 left
    let (res, out) =
      run_event_with_flags(&home, &bash_event_with_cwd(cmd_0ad72837, "x".repeat(3_433), cwd_str), false, true);
    res.unwrap();
    let decision = decision_of(&out);
    let replaced = decision["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str().unwrap();
    assert_eq!(replaced, withhold_notice(3_433, 2_208, false, true));
    assert_eq!(home.state().counted, 13_792);

    // Read tool with relative artifacts path
    let rel_read = read_event_with_cwd("artifacts/plan_x.html", &"x".repeat(20_000), cwd_str);
    let (res, out) = run_event_with_flags(&home, &rel_read, false, false);
    res.unwrap();
    assert!(out.is_empty(), "relative Read of approval object is exempt");
  }
}
