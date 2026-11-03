//! The task delegation command.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use charliebot_cli_common::args::{self, with_infer_long_args};
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::files;
use charliebot_cli_common::json_out;
use charliebot_cli_common::resolve;
use charliebot_core::config;
use charliebot_core::event_types;
use charliebot_store::chat_events::{self, LineRule};
use charliebot_store::metadata::{REQUESTER_WORDS_HEADING, requester_words_heading_line};
use charliebot_tasks::routes as task_routes;
use clap::error::ErrorKind;
use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};
use serde_json::{Map, Value, json};

const DELEGATE_EPILOG: &str = r#"Task spec (--task-spec-file), six sections:

  ## Goal - one concise deliverable.
  ## Source Files - absolute paths, or `- (none)`.
  ## Required Behavior - the executable contract, state-machine semantics, and boundary rules.
  ## Acceptance Tests - focused tests or verification commands.
  ## Reviewer Checklist - concrete checks beyond "tests passed".
  ## Out of Scope - things the worker must not change.

  --repo and --base-branch: give both, or omit both for a repo-less implement, quick-edit, or
  script-run task. verify forbids both.
  --keep-worktree 1 keeps the worktree after the worker exits and the reviewer merges; pass it when
  the worker launches a long-running process, e.g. a SLURM job, whose WorkDir lives in the
  worktree. 0 cleans up.
  --request-id: replay id; the same id returns the original child. Default: derived from the
  request content, so an identical spec re-delegated from the same session returns the existing
  child; pass an explicit id for intentional same-spec siblings.

Task types (--task-type):

  implement (default): worker commits; a reviewer rebases, ff-merges, pushes to the remote base branch.
  quick-edit: worker commits; no reviewer; the master merges and pushes; for trivial repo ops (cherry-picks, branch pushes, single-line or doc-only edits).
  script-run: the worktree (repo-less: the Run directory) is a sandbox to run scripts, submit jobs, or query state; the worker must not modify tracked files or commit; no reviewer, no merge.
  verify: a repo-less read-only plan verifier; no worktree, reviewer, or merge.

Requester Words:

  A spec for a first read of unfamiliar material, a diagnosis of an unexplained failure, an
  explanation of a mechanism or cause (an explain or debugging page included), an understanding
  page, or a design or plan draft (a new plan version that changes the design included) carries
  it: the words that asked the question, and each earlier user decision the task rests on, each
  quoted verbatim with its session id and event kind (user, agent, scheduled or thread).
  charliebot delegate checks each quote against its session's events of that kind; the section
  also gives its worker the user-decisions rule, and a spec without it is accepted.
  - 「<verbatim>」 (session <36-char session id>, <user|agent|scheduled|thread>)

Backend selection (--backend):

  Pass --backend only when the user or the line's goal names a backend. Omitted, the router that
  backends.routing configures chooses the backend of implement, quick-edit, script-run, and verify;
  when routing is unset or the router gives no answer, implement, quick-edit, and script-run take
  the next backends.pool member that is not cooling, or inherit the session backend when the pool
  is empty, and verify takes the next member that differs from the session backend. A worker out of
  quota continues on the next member in the same worktree. An explicit --backend replaces that
  routing for every task type, verify included, and is never failed over. The resolved backend runs
  every work Run of the worker session.
"#;

const LONG_OPTIONS: [&str; 8] =
  ["session", "repo", "task-spec-file", "base-branch", "backend", "keep-worktree", "request-id", "task-type"];

const TASK_SPEC_REQUIRED_HEADINGS: [&str; 6] =
  ["Goal", "Source Files", "Required Behavior", "Acceptance Tests", "Reviewer Checklist", "Out of Scope"];

#[derive(Parser)]
#[command(name = "delegate", about = "Delegate a task to a CharlieBot worker agent", after_help = DELEGATE_EPILOG)]
struct DelegateCli {
  #[arg(
    long,
    help = "Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the master environment)"
  )]
  session: Option<String>,
  #[arg(long, help = "Git repo path (rules in the epilog)")]
  repo: Option<String>,
  #[arg(long, required = true, help = "Markdown task spec (format in the epilog)")]
  task_spec_file: String,
  #[arg(long, help = "Worktree base branch (rules in the epilog)")]
  base_branch: Option<String>,
  #[arg(long, help = "Backend option id from $CHARLIEBOT_HOME/config.yaml (rules in the epilog)")]
  backend: Option<String>,
  #[arg(long, required = true, value_parser = clap::value_parser!(i32).range(0..=1), help = "0 or 1 (rule in the epilog)")]
  keep_worktree: i32,
  #[arg(long, help = "Replay id (v2 task-tree sessions; rule in the epilog)")]
  request_id: Option<String>,
  #[arg(long, value_enum, default_value = "implement", help = "Task profile (rules in the epilog)")]
  task_type: TaskType,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum TaskType {
  Implement,
  #[value(name = "quick-edit")]
  QuickEdit,
  #[value(name = "script-run")]
  ScriptRun,
  Verify,
}

impl TaskType {
  fn as_str(self) -> &'static str {
    match self {
      Self::Implement => "implement",
      Self::QuickEdit => "quick-edit",
      Self::ScriptRun => "script-run",
      Self::Verify => "verify",
    }
  }
}

pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let mut command = with_infer_long_args(DelegateCli::command());
  let (known_args, unknown_args) = partition_arguments(args);
  let argv = std::iter::once(OsString::from("charliebot delegate")).chain(known_args);
  let matches = match command.clone().try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = DelegateCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");

  if unknown_args.iter().any(|argument| argument.starts_with("--reviewer-")) {
    parser_error(
      &mut command,
      ErrorKind::UnknownArgument,
      "the reviewer context file option was removed: put reviewer instructions under the task spec's ## Reviewer Checklist",
    );
  }
  if !unknown_args.is_empty() {
    parser_error(
      &mut command,
      ErrorKind::UnknownArgument,
      format!("unrecognized arguments: {}", unknown_args.join(" ")),
    );
  }

  let task_type = cli.task_type.as_str();
  if let Err(message) = validate_repo_and_branch(cli.task_type, cli.repo.as_deref(), cli.base_branch.as_deref()) {
    parser_error(&mut command, ErrorKind::ValueValidation, message);
  }
  if cli.task_type != TaskType::Verify
    && let Some(repo) = &cli.repo
  {
    args::validate_repo_path(&mut command, repo);
  }

  let session_id = resolve::resolve_from_env(cli.session.as_deref())?;
  let task_spec = files::read_required_text_file("--task-spec-file", &cli.task_spec_file)?;
  validate_task_spec_markdown(&task_spec)?;
  let home = charliebot_core::home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  validate_requester_words(&task_spec, &home)?;

  let keep_worktree = cli.keep_worktree == 1;
  let mut invocation = Map::new();
  invocation.insert(task_routes::DELEGATE_TASK_TYPE_FIELD.to_string(), json!(task_type));
  invocation.insert(task_routes::DELEGATE_REPO_PATH_FIELD.to_string(), optional_string(&cli.repo));
  invocation.insert(task_routes::DELEGATE_BASE_BRANCH_FIELD.to_string(), optional_string(&cli.base_branch));
  invocation.insert(task_routes::DELEGATE_TASK_SPEC_FILE_FIELD.to_string(), json!(cli.task_spec_file));
  invocation.insert(task_routes::DELEGATE_REVIEWER_CONTEXT_FILE_FIELD.to_string(), Value::Null);
  invocation.insert(task_routes::DELEGATE_KEEP_WORKTREE_FIELD.to_string(), json!(keep_worktree));
  invocation.insert(task_routes::DELEGATE_BACKEND_FIELD.to_string(), optional_string(&cli.backend));

  let mut payload = Map::new();
  payload.insert(task_routes::DELEGATE_SESSION_ID_FIELD.to_string(), json!(session_id));
  payload.insert(task_routes::DELEGATE_DESCRIPTION_FIELD.to_string(), json!(task_spec));
  payload.insert(task_routes::DELEGATE_KEEP_WORKTREE_FIELD.to_string(), json!(keep_worktree));
  payload.insert(task_routes::DELEGATE_TASK_TYPE_FIELD.to_string(), json!(task_type));
  payload.insert(event_types::DELEGATE_INVOCATION.to_string(), Value::Object(invocation));
  if let Some(base_branch) = &cli.base_branch {
    payload.insert(task_routes::DELEGATE_BASE_BRANCH_FIELD.to_string(), json!(base_branch));
  }
  if let Some(backend) = &cli.backend {
    payload.insert(task_routes::DELEGATE_BACKEND_FIELD.to_string(), json!(backend));
  }
  if let Some(repo) = &cli.repo {
    payload.insert(task_routes::DELEGATE_REPO_PATH_FIELD.to_string(), json!(repo));
  }
  if let Some(request_id) = &cli.request_id {
    payload.insert(task_routes::DELEGATE_REQUEST_ID_FIELD.to_string(), json!(request_id));
  }

  let readback_session = session_id.clone();
  let readback_description = task_spec.clone();
  let readback_task_type = task_type.to_string();
  let readback_request_id = cli.request_id.clone();
  let readback = move || {
    Ok(charliebot_store::task_child::find_local_task_child(
      &home,
      &readback_session,
      &readback_description,
      Some(&readback_task_type),
      readback_request_id.as_deref(),
    ))
  };
  let result = client::post(task_routes::DELEGATE_PATH, Value::Object(payload), Some(Box::new(readback)), &[])?;

  eprintln!("Worker spawned in the background; the completion summary arrives as an async wake-up.");
  let session_id = result.get("session_id").ok_or_else(|| CliExit::error("the delegate response lacks session_id"))?;
  let run_id = result.get("run_id").ok_or_else(|| CliExit::error("the delegate response lacks run_id"))?;
  let mut output = Map::new();
  output.insert("session_id".to_string(), session_id.clone());
  output.insert("run_id".to_string(), run_id.clone());
  print!("{}", json_out::render(&Value::Object(output), Some(2)));
  Ok(())
}

fn partition_arguments(args: &[OsString]) -> (Vec<OsString>, Vec<String>) {
  let mut known = Vec::new();
  let mut unknown = Vec::new();
  let mut index = 0;
  let mut options_ended = false;
  while index < args.len() {
    let argument = args[index].to_string_lossy();
    if options_ended {
      unknown.push(argument.into_owned());
      index += 1;
      continue;
    }
    if argument == "--" {
      options_ended = true;
      index += 1;
      continue;
    }
    if argument == "-h" || argument == "--help" {
      known.push(args[index].clone());
      index += 1;
      continue;
    }
    if is_known_long_option(&argument) {
      let has_inline_value = argument.contains('=');
      known.push(args[index].clone());
      index += 1;
      if !has_inline_value && index < args.len() {
        known.push(args[index].clone());
        index += 1;
      }
      continue;
    }
    unknown.push(argument.into_owned());
    index += 1;
  }
  (known, unknown)
}

fn is_known_long_option(argument: &str) -> bool {
  let Some(argument) = argument.strip_prefix("--") else {
    return false;
  };
  let option = argument.split_once('=').map_or(argument, |(option, _)| option);
  LONG_OPTIONS.iter().filter(|known| known.starts_with(option)).count() == 1
}

fn parser_error(command: &mut clap::Command, kind: ErrorKind, message: impl std::fmt::Display) -> ! {
  command.error(kind, message).exit()
}

fn validate_repo_and_branch(task_type: TaskType, repo: Option<&str>, base_branch: Option<&str>) -> Result<(), String> {
  if task_type == TaskType::Verify {
    if repo.is_some() {
      return Err("--repo is forbidden when --task-type verify".to_string());
    }
    if base_branch.is_some() {
      return Err("--base-branch is forbidden when --task-type verify".to_string());
    }
  } else if repo.is_some() != base_branch.is_some() {
    return Err(format!(
      "--repo and --base-branch are given together for a repo task or omitted together for a repo-less one; exactly one was given for --task-type {}",
      task_type.as_str()
    ));
  }
  Ok(())
}

fn validate_task_spec_markdown(content: &str) -> Result<(), CliExit> {
  let lines: Vec<&str> = content.lines().collect();
  let missing_headings: Vec<&str> = TASK_SPEC_REQUIRED_HEADINGS
    .iter()
    .copied()
    .filter(|heading| !lines.iter().any(|line| is_heading(line, heading)))
    .collect();
  if !missing_headings.is_empty() {
    return Err(CliExit::usage(format!(
      "task spec missing required headings: {}",
      missing_headings.iter().map(|heading| format!("## {heading}")).collect::<Vec<_>>().join(", ")
    )));
  }

  let source_start = lines
    .iter()
    .position(|line| is_heading(line, "Source Files"))
    .expect("the required Source Files heading was found");
  let mut saw_source_entry = false;
  for line in lines.iter().skip(source_start + 1).take_while(|line| !is_next_heading(line)) {
    let stripped = line.trim();
    if !stripped.starts_with("- ") {
      continue;
    }
    saw_source_entry = true;
    if stripped == "- (none)" {
      continue;
    }
    if stripped.starts_with("- /") {
      let source_path = stripped[2..].trim();
      if !Path::new(source_path).exists() {
        return Err(CliExit::usage(format!("task spec source file not found: {source_path}")));
      }
      continue;
    }
    return Err(CliExit::usage(format!(
      "task spec source file entries must be absolute paths or - (none): {stripped}"
    )));
  }
  if !saw_source_entry {
    return Err(CliExit::usage("task spec Source Files section must list source files or - (none)"));
  }
  Ok(())
}

/// The event kinds a requester quote names. Each kind matches the chat-log events it stands for: a user message is a
/// `user` event with string content; an agent message is an `agent_message` event; a scheduled-task prompt is a
/// `scheduled_trigger` event; a Slack or Discord thread message is the `agent_message` event that carries the
/// platform's summon block (`event_types::is_platform_summon`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequesterKind {
  User,
  Agent,
  Scheduled,
  Thread,
}

impl RequesterKind {
  fn parse(name: &str) -> Option<Self> {
    match name {
      "user" => Some(Self::User),
      "agent" => Some(Self::Agent),
      "scheduled" => Some(Self::Scheduled),
      "thread" => Some(Self::Thread),
      _ => None,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Self::User => "user",
      Self::Agent => "agent",
      Self::Scheduled => "scheduled",
      Self::Thread => "thread",
    }
  }

  fn matches(self, event: &Value) -> bool {
    match self {
      Self::User => event_types::is_real_user_message(event),
      Self::Agent => event.get("type").and_then(Value::as_str) == Some(event_types::AGENT_MESSAGE),
      Self::Scheduled => event.get("type").and_then(Value::as_str) == Some(event_types::SCHEDULED_TRIGGER),
      Self::Thread => event_types::is_platform_summon(event),
    }
  }
}

/// One `- 「quote」 (session <id>, <kind>)` line of the Requester Words section.
struct RequesterQuote<'a> {
  quote: &'a str,
  session_id: &'a str,
  kind: RequesterKind,
}

/// Split a quote line at its last ` (session ` tag, so a quote may itself hold a 」.
fn parse_requester_line(line: &str) -> Option<(&str, &str, &str)> {
  let body = line.strip_prefix("- 「")?;
  let (quote, tail) = body.rsplit_once("」 (session ")?;
  let (session_id, kind) = tail.strip_suffix(')')?.split_once(", ")?;
  Some((quote, session_id, kind))
}

/// Check every quote of a `## Requester Words` section against the events of its named kind in its named session.
/// A spec without the section is accepted. One refusal lists every quote that fails.
fn validate_requester_words(task_spec: &str, home: &Path) -> Result<(), CliExit> {
  let lines: Vec<&str> = task_spec.lines().collect();
  let Some(start) = requester_words_heading_line(task_spec) else {
    return Ok(());
  };
  let mut quotes = Vec::new();
  for line in lines.iter().skip(start + 1).take_while(|line| !is_next_heading(line)).map(|line| line.trim()) {
    if line.is_empty() {
      continue;
    }
    let Some((quote, session_id, kind)) = parse_requester_line(line) else {
      return Err(CliExit::usage(format!(
        "## {REQUESTER_WORDS_HEADING} line must read `- 「quote」 (session <id>, <kind>)`: {line}"
      )));
    };
    let Some(kind) = RequesterKind::parse(kind) else {
      return Err(CliExit::usage(format!(
        "## {REQUESTER_WORDS_HEADING} kind must be user, agent, scheduled or thread: {line}"
      )));
    };
    if quote.is_empty() {
      return Err(CliExit::usage(format!("## {REQUESTER_WORDS_HEADING} quote is empty: {line}")));
    }
    quotes.push(RequesterQuote { quote, session_id, kind });
  }
  if quotes.is_empty() {
    return Err(CliExit::usage(format!("## {REQUESTER_WORDS_HEADING} section lists no quote")));
  }

  let mut logs: BTreeMap<&str, Option<Vec<Value>>> = BTreeMap::new();
  for quote in &quotes {
    if !logs.contains_key(quote.session_id) {
      logs.insert(quote.session_id, session_events(home, quote.session_id)?);
    }
  }
  let mut failures = Vec::new();
  for quote in &quotes {
    let reason = match &logs[quote.session_id] {
      None => "the session has no chat log".to_string(),
      Some(events) if events.iter().any(|event| quote.kind.matches(event) && event_contains(event, quote.quote)) => {
        continue;
      }
      Some(_) => format!("no {} event of the session contains it", quote.kind.name()),
    };
    failures.push(format!("- 「{}」 (session {}, {}): {reason}", quote.quote, quote.session_id, quote.kind.name()));
  }
  if failures.is_empty() {
    return Ok(());
  }
  Err(CliExit::usage(format!(
    "## {REQUESTER_WORDS_HEADING} quotes not found in their named sessions:\n{}",
    failures.join("\n")
  )))
}

fn event_contains(event: &Value, quote: &str) -> bool {
  event.get("content").and_then(Value::as_str).is_some_and(|content| content.contains(quote))
}

/// The events of a session's chat log, its archives and its live log in reading order, or `None` when the session has
/// no chat log under the home.
fn session_events(home: &Path, session_id: &str) -> Result<Option<Vec<Value>>, CliExit> {
  let session_dir = config::sessions_dir_in(home).join(session_id);
  let paths = chat_events::session_log_paths(&session_dir)
    .map_err(|error| CliExit::error(format!("reading the chat log of session {session_id}: {error}")))?;
  if paths.is_empty() {
    return Ok(None);
  }
  let mut events = Vec::new();
  for path in paths {
    let lines = chat_events::read_lines(&path, LineRule::PerLine)
      .map_err(|error| CliExit::error(format!("reading {}: {error}", path.display())))?;
    for line in lines {
      let line = line.map_err(|error| CliExit::error(format!("reading {}: {error}", path.display())))?;
      if line.text.trim().is_empty() {
        continue;
      }
      let event = line.decode().map_err(|error| {
        CliExit::error(format!("{} line {} is not a JSON event: {error}", path.display(), line.number))
      })?;
      events.push(event);
    }
  }
  Ok(Some(events))
}

fn is_heading(line: &str, heading: &str) -> bool {
  line.strip_prefix("## ").is_some_and(|name| name.trim_end_matches([' ', '\t']) == heading)
}

fn is_next_heading(line: &&str) -> bool {
  line.starts_with("## ") && line.len() > 3
}

fn optional_string(value: &Option<String>) -> Value {
  value.as_ref().map_or(Value::Null, |value| json!(value))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn help_text() -> String {
    with_infer_long_args(DelegateCli::command()).render_help().to_string()
  }

  #[test]
  fn test_main_help_lists_verify_profile() {
    let help = help_text();
    assert!(help.contains("verify"));
    assert!(help.contains("read-only plan verifier"));
  }

  #[test]
  fn test_main_help_stays_bounded_and_names_every_delegation_fact() {
    let help = help_text();
    assert!(help.contains("Delegate a task to a CharlieBot worker agent"));
    assert!(help.chars().count() <= 4100);
    for name in [
      "--session",
      "--repo",
      "--task-spec-file",
      "--base-branch",
      "--backend",
      "--keep-worktree",
      "--request-id",
      "--task-type",
    ] {
      assert!(help.contains(name), "{name} is missing from help");
    }
    assert!(!help.contains("--context-work"), "--context-work is still named in help");
    assert!(help.contains("Requester Words"), "Requester Words is missing from help");
    for choice in ["implement", "quick-edit", "script-run", "verify"] {
      assert!(help.contains(choice), "{choice} is missing from help");
    }
    for section in [
      "## Goal",
      "## Source Files",
      "## Required Behavior",
      "## Acceptance Tests",
      "## Reviewer Checklist",
      "## Out of Scope",
    ] {
      assert!(help.contains(section), "{section} is missing from help");
    }
  }

  #[test]
  fn the_context_work_flag_is_unknown() {
    let args = ["--context-work", "0"].map(OsString::from);
    let (known, unknown) = partition_arguments(&args);
    assert!(known.is_empty(), "{known:?}");
    assert_eq!(unknown, ["--context-work", "0"]);
  }

  #[test]
  fn requester_line_splits_at_the_last_session_tag() {
    let session = "11111111-2222-4333-8444-555555555555";
    assert_eq!(parse_requester_line(&format!("- 「a」b」 (session {session}, user)")), Some(("a」b", session, "user")));
    assert_eq!(parse_requester_line("- 「a」 (session 11111111)"), None);
  }

  #[test]
  fn test_main_repo_task_types_take_repo_and_base_branch_together() {
    for task_type in [TaskType::Implement, TaskType::QuickEdit, TaskType::ScriptRun] {
      for (repo, base_branch) in [(Some("/repo"), None), (None, Some("main"))] {
        let error = validate_repo_and_branch(task_type, repo, base_branch).unwrap_err();
        assert!(error.contains("given together"), "{error}");
        assert!(error.contains(task_type.as_str()), "{error}");
      }
    }
  }
}
