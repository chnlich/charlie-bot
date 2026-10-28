//! The `web` verbs (`src/features/web_reply/cli.py`): record a web reply, print the For-you layer, and set, confirm or
//! show the session status.
//!
//! `status set` cold-reads each unread Waiting on you item before it stores anything. The item's host-served link is
//! read, the ask probe goes to the light backends in pool rotation, and each answer is recorded with `ask-check`. When
//! any item was read, the status is not stored: the verb exits 3 so the caller revises and runs it again.

use std::ffi::OsString;
use std::future::Future;

use charliebot_agents::cli_host::LazyHost;
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::files;
use charliebot_cli_common::json_out;
use charliebot_cli_common::reply::post_reply;
use charliebot_cli_common::resolve;
use charliebot_compat::file_urls;
use charliebot_core::event_types;
use charliebot_core::pyfmt;
use charliebot_tasks::cold_read;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Map, Value, json};

#[derive(Parser)]
#[command(name = "web", about = "CharlieBot web session verbs")]
struct WebCli {
  #[command(subcommand)]
  command: WebCommand,
}

#[derive(Subcommand)]
enum WebCommand {
  /// Record a web reply for this session
  Reply(ReplyArgs),
  /// Print this session's For-you layer as plain text
  Dialog(SessionArgs),
  /// Set or show this session's status
  Status {
    #[command(subcommand)]
    command: StatusCommand,
  },
}

#[derive(Subcommand)]
enum StatusCommand {
  /// Cold read Waiting on you items before storing the status
  Set(StatusSetArgs),
  /// Confirm that the stored status needs no change
  Confirm,
  /// Print this session's status and the waiting items of its child sessions
  Show(SessionArgs),
}

#[derive(Args)]
struct ReplyArgs {
  /// File holding the reply text; - reads stdin
  #[arg(long)]
  file: String,
  /// Post the reply as written and record REASON; skips the language check
  #[arg(long, value_name = "REASON")]
  keep_language: Option<String>,
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the master environment)
  #[arg(long)]
  session: Option<String>,
}

#[derive(Args)]
struct SessionArgs {
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the master environment)
  #[arg(long)]
  session: Option<String>,
}

#[derive(Args)]
struct StatusSetArgs {
  /// JSON object with any of goal, now, waiting, remaining; prints cold reads and exits 3 before storing; - reads stdin
  #[arg(long)]
  file: String,
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the master environment)
  #[arg(long)]
  session: Option<String>,
}

/// Parse the web arguments and run one verb. Parser errors print and exit on their own (argparse-compatible: help 0,
/// usage error 2).
pub fn main(args: &[OsString], host: &LazyHost) -> Result<(), CliExit> {
  let command = with_infer_long_args(WebCli::command());
  let argv = std::iter::once(OsString::from("charliebot web")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = WebCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    WebCommand::Reply(args) => reply(&args),
    WebCommand::Dialog(args) => dialog(&args),
    WebCommand::Status { command } => match command {
      StatusCommand::Set(args) => status_set(&args, host),
      StatusCommand::Confirm => status_confirm(),
      StatusCommand::Show(args) => status_show(&args),
    },
  }
}

fn reply(args: &ReplyArgs) -> Result<(), CliExit> {
  let extra = args.keep_language.as_ref().map(|reason| Map::from_iter([("keep_language".to_string(), json!(reason))]));
  post_reply("/api/internal/web/reply", args.session.as_deref(), &args.file, extra)
}

fn dialog(args: &SessionArgs) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  let result = client::get("/api/internal/web/dialog", Some(vec![("session_id".to_string(), session_id)]))?;
  let mut blocks = Vec::new();
  for entry in list(field(&result, "entries")?)? {
    let label = match pyfmt::python_str(field(entry, "role")?).as_str() {
      "user" => "You",
      event_types::WEB_REPLY => "Reply",
      event_types::WEB_REPLY_NOTICE => "Notice",
      role => return Err(CliExit::error(format!("unknown dialog role {role}"))),
    };
    blocks.push(format!("{label}: {}", pyfmt::python_str(field(entry, "content")?)));
  }
  if !blocks.is_empty() {
    println!("{}", blocks.join("\n\n"));
  }
  Ok(())
}

fn status_set(args: &StatusSetArgs, lazy_host: &LazyHost) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  let text = files::read_reply_text(&args.file)?;
  let request = json!({ "session_id": session_id, "text": text });
  let unread = client::post("/api/internal/web/status/unread", request.clone(), None, &[])?;
  let unread = list(field(&unread, "unread")?)?;
  if unread.is_empty() {
    let result = client::post("/api/internal/web/status", request, None, &[])?;
    print!("{}", json_out::render(&result, None));
    return Ok(());
  }

  let host = lazy_host.get().map_err(anyhow_exit)?;
  let (system_prompt, questions) = cold_read::load_ask_texts(&host.repo_root).map_err(anyhow_exit)?;
  for item in unread {
    let link = item.get("link").and_then(Value::as_str);
    let linked_page = match file_urls::host_file(link).map_err(CliExit::error)? {
      None => None,
      Some(path) => Some(files::read_text_mode(&path)?),
    };
    let probe_prompt = cold_read::ask_probe_input(item, linked_page.as_deref(), &questions);
    let probe = block_on(cold_read::run_light_probe(host, &probe_prompt, &system_prompt))?.map_err(anyhow_exit)?;
    let Some(backend) = probe.backend_id else {
      for (backend, error) in &probe.attempts {
        eprintln!("attempt {backend} failed: {error}");
      }
      if probe.attempts.is_empty() {
        eprintln!("No light backend was available for this item.");
      }
      std::process::exit(1);
    };
    let answer = probe.answer.expect("a successful ask probe has the answering backend's response");
    client::post(
      "/api/internal/web/ask-check",
      json!({ "session_id": session_id, "item": item, "backend": backend, "answer": answer }),
      None,
      &[],
    )?;
    println!("Waiting on you: {}\nBackend: {backend}\n{answer}\n", pyfmt::python_str(field(item, "text")?));
  }
  println!("Nothing was stored.");
  std::process::exit(3);
}

fn status_confirm() -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(None)?;
  let result = client::post("/api/internal/web/status/confirm", json!({ "session_id": session_id }), None, &[])?;
  print!("{}", json_out::render(&result, None));
  Ok(())
}

fn status_show(args: &SessionArgs) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  let result = client::get("/api/internal/web/status", Some(vec![("session_id".to_string(), session_id)]))?;
  println!("{}", render_status(&result)?);
  Ok(())
}

/// `_render_status`: the GET result as plain text, the session's Goal, Remaining, Now and Waiting on you, then one
/// block per child with waiting items, then the Now of each open child.
fn render_status(result: &Value) -> Result<String, CliExit> {
  let mut lines: Vec<String> = Vec::new();
  let status = field(result, event_types::WEB_STATUS)?;
  if status.is_null() {
    lines.push("No status yet".to_string());
  } else {
    lines.push(format!("Goal: {}", or_not_set(field(status, "goal")?)));
    let remaining: &[Value] = match status.get("remaining") {
      None | Some(Value::Null) => &[],
      Some(value) => list(value)?,
    };
    if !remaining.is_empty() {
      let done: Vec<bool> = remaining.iter().map(remaining_done).collect::<Result<_, _>>()?;
      let open = done.iter().filter(|done| !**done).count();
      lines.push(format!(
        "Remaining: {open} of {} left (updated {})",
        remaining.len(),
        pyfmt::python_str(field(status, "remaining_updated_at")?)
      ));
      for (item, done) in remaining.iter().zip(done) {
        let marker = if done { "✓" } else { "○" };
        lines.push(format!("  {marker} {}", pyfmt::python_str(field(item, "text")?)));
      }
    }
    lines.push(format!(
      "Now: {} (updated {})",
      or_not_set(field(status, "now")?),
      pyfmt::python_str(field(status, "updated_at")?)
    ));
    let waiting = list(field(status, "waiting")?)?;
    if waiting.is_empty() {
      lines.push("Waiting on you: Nothing".to_string());
    } else {
      lines.push("Waiting on you:".to_string());
      for item in waiting {
        lines.extend(item_lines(item)?);
      }
    }
  }
  for row in list(field(result, "descendants")?)? {
    lines.push(String::new());
    lines.push(format!("{} ({})", pyfmt::python_str(field(row, "name")?), short_session_id(row)?));
    for item in list(field(row, "waiting")?)? {
      lines.extend(item_lines(item)?);
    }
  }
  let children = list(field(result, "children")?)?;
  if !children.is_empty() {
    lines.push(String::new());
    lines.push("Child sessions' Now:".to_string());
    for row in children {
      lines.push(format!(
        "{} ({}): {} (updated {})",
        pyfmt::python_str(field(row, "name")?),
        short_session_id(row)?,
        pyfmt::python_str(field(row, "now")?),
        pyfmt::python_str(field(row, "updated_at")?)
      ));
    }
  }
  Ok(lines.join("\n"))
}

/// `_item_lines`: the item line, its `since` and `since_exact` lines, and its context when one is stored, indented below.
fn item_lines(item: &Value) -> Result<Vec<String>, CliExit> {
  let mut lines = vec![
    item_line(item)?,
    format!("  since: {}", pyfmt::python_str(field(item, "since")?)),
    format!("  since_exact: {}", pyfmt::json_dumps(field(item, "since_exact")?, &pyfmt::PYTHON_JSON_DEFAULT)),
  ];
  if let Some(context) = item.get("context").filter(|context| !context.is_null()) {
    let context = pyfmt::python_str(context);
    let context_lines = py_splitlines(&context);
    if let Some((first, rest)) = context_lines.split_first() {
      lines.push(format!("  context: {first}"));
      lines.extend(rest.iter().map(|line| format!("  {line}")));
    }
  }
  Ok(lines)
}

/// A remaining item's `done` flag; a response without a boolean one is a broken server contract.
fn remaining_done(item: &Value) -> Result<bool, CliExit> {
  item
    .get("done")
    .and_then(Value::as_bool)
    .ok_or_else(|| CliExit::error("the response carries a remaining item without a boolean done"))
}

/// `_item_line`: `- <text>`, with ` (<link>)` appended when the item names a link.
fn item_line(item: &Value) -> Result<String, CliExit> {
  let text = pyfmt::python_str(field(item, "text")?);
  let link = field(item, "link")?;
  Ok(if is_truthy(link) { format!("- {text} ({})", pyfmt::python_str(link)) } else { format!("- {text}") })
}

fn short_session_id(row: &Value) -> Result<String, CliExit> {
  Ok(pyfmt::python_str(field(row, "session_id")?).chars().take(8).collect())
}

fn or_not_set(value: &Value) -> String {
  if is_truthy(value) { pyfmt::python_str(value) } else { "(not set)".to_string() }
}

/// A response field the verb reads; a missing one is a broken server contract.
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, CliExit> {
  value.get(key).ok_or_else(|| CliExit::error(format!("the response lacks {key}")))
}

fn list(value: &Value) -> Result<&Vec<Value>, CliExit> {
  value.as_array().ok_or_else(|| CliExit::error("the response carries a field that is not a list"))
}

/// Python truthiness of a `json.loads` value, for the `or` and `if` tests the rendering makes.
fn is_truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(flag) => *flag,
    Value::Number(number) => number.as_f64().is_none_or(|float| float != 0.0),
    Value::String(text) => !text.is_empty(),
    Value::Array(items) => !items.is_empty(),
    Value::Object(members) => !members.is_empty(),
  }
}

/// `str.splitlines()`: the line breaks Python recognizes, none kept, and no empty line after a final break.
fn py_splitlines(text: &str) -> Vec<&str> {
  let mut lines = Vec::new();
  let mut start = 0;
  let mut chars = text.char_indices().peekable();
  while let Some((index, ch)) = chars.next() {
    let break_end = match ch {
      '\r' if matches!(chars.peek(), Some((_, '\n'))) => {
        chars.next();
        index + 2
      }
      '\r' | '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}' => {
        index + ch.len_utf8()
      }
      _ => continue,
    };
    lines.push(&text[start..index]);
    start = break_end;
  }
  if start < text.len() {
    lines.push(&text[start..]);
  }
  lines
}

/// Runs one async probe to completion on a single-threaded runtime owned by this process.
fn block_on<F: Future>(future: F) -> Result<F::Output, CliExit> {
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| CliExit::error(error.to_string()))?;
  Ok(runtime.block_on(future))
}

fn anyhow_exit(error: anyhow::Error) -> CliExit {
  CliExit::error(format!("{error:#}"))
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::render_status;

  /// A GET result for a status that carries `status` and no descendant or child rows.
  fn status_result(status: serde_json::Value) -> serde_json::Value {
    json!({"web_status": status, "descendants": [], "children": []})
  }

  #[test]
  fn a_status_with_remaining_prints_the_count_and_one_line_per_item_after_the_goal() {
    let result = status_result(json!({
      "goal": "Ship the panel",
      "remaining": [
        {"text": "Move the server to Rust", "done": true},
        {"text": "Delete the Python server", "done": false},
      ],
      "remaining_updated_at": "2026-11-02T03:10:00Z",
      "now": "Reviewing",
      "waiting": [],
      "updated_at": "2026-11-02T03:10:00Z",
    }));

    let text = render_status(&result).expect("the status renders");

    assert_eq!(
      text.lines().collect::<Vec<_>>(),
      vec![
        "Goal: Ship the panel",
        "Remaining: 1 of 2 left (updated 2026-11-02T03:10:00Z)",
        "  ✓ Move the server to Rust",
        "  ○ Delete the Python server",
        "Now: Reviewing (updated 2026-11-02T03:10:00Z)",
        "Waiting on you: Nothing",
      ]
    );
  }

  #[test]
  fn a_status_without_remaining_prints_no_remaining_line() {
    let result = status_result(json!({
      "goal": "Ship the panel",
      "now": "Reviewing",
      "waiting": [],
      "updated_at": "2026-11-02T03:10:00Z",
    }));

    let text = render_status(&result).expect("the status renders");

    assert!(text.lines().all(|line| !line.starts_with("Remaining:")), "{text}");
    assert_eq!(text.lines().nth(1), Some("Now: Reviewing (updated 2026-11-02T03:10:00Z)"));
  }
}
