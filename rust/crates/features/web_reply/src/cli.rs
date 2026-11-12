//! The `web` verbs: record a web reply, print the For-you layer, and set, confirm or
//! show the session status.
//!
//! `status set` runs the fork assertions of each unread item's linked page, then cold-reads each unread Waiting on you
//! item before it stores anything. The item's host-served link is read, the ask probe goes to the light backends in
//! pool rotation, and each answer is recorded with `ask-check`. When a page fails its fork assertions or any item was
//! read, the status is not stored: the verb exits 3 so the caller revises and runs it again.

use std::ffi::OsString;
use std::future::Future;

use charliebot_agents::cli_host::{CliHost, LazyHost};
use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::files;
use charliebot_cli_common::json_out;
use charliebot_cli_common::resolve;
use charliebot_cli_common::response::field;
use charliebot_compat::file_urls;
use charliebot_core::event_types;
use charliebot_core::str_methods::splitlines;
use charliebot_core::text_repr;
use charliebot_tasks::cold_read;
use clap::{Args, Parser, Subcommand};
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
  /// JSON object with any of goal, now, waiting; prints cold reads and exits 3 before storing; - reads stdin
  #[arg(long)]
  file: String,
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the master environment)
  #[arg(long)]
  session: Option<String>,
}

/// Parse the web arguments and run one verb. Parser errors print and exit on their own (argparse-compatible: help 0,
/// usage error 2).
pub fn main(args: &[OsString], host: &LazyHost) -> Result<(), CliExit> {
  let cli: WebCli = cli_args::parse("charliebot web", args);
  match cli.command {
    WebCommand::Reply(args) => reply(&args, host),
    WebCommand::Dialog(args) => dialog(&args),
    WebCommand::Status { command } => match command {
      StatusCommand::Set(args) => status_set(&args, host),
      StatusCommand::Confirm => status_confirm(),
      StatusCommand::Show(args) => status_show(&args),
    },
  }
}

fn reply(args: &ReplyArgs, lazy_host: &LazyHost) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  let text = files::read_reply_text(&args.file)?;
  let request = json!({ "session_id": &session_id, "text": &text });
  let unread_result = client::post("/api/internal/web/reply/unread", request, None, &[])?;
  let unread = unread_result
    .get("unread")
    .and_then(Value::as_bool)
    .ok_or_else(|| CliExit::error("the reply-unread response lacks a boolean unread field"))?;

  if unread {
    let code_findings: Vec<String> = list(field(&unread_result, "code_findings")?)?
      .iter()
      .map(|finding| {
        finding
          .as_str()
          .map(str::to_owned)
          .ok_or_else(|| CliExit::error("the reply-unread response carries a non-string code finding"))
      })
      .collect::<Result<_, _>>()?;

    let findings = if !code_findings.is_empty() {
      client::post(
        "/api/internal/web/reply-check",
        json!({
          "session_id": &session_id,
          "text": &text,
          "backend": "code-assertions",
          "findings": &code_findings,
        }),
        None,
        &[],
      )?;
      code_findings
    } else {
      let host = lazy_host.get().map_err(anyhow_exit)?;
      let (system_prompt, questions) = cold_read::load_reply_texts(&host.repo_root).map_err(anyhow_exit)?;
      let probe_prompt = cold_read::reply_probe_input(&text, &questions);
      let probe = block_on(cold_read::run_probe(host, &probe_prompt, &system_prompt))?.map_err(anyhow_exit)?;
      let Some(backend) = probe.backend_id else {
        for (backend, error) in &probe.attempts {
          eprintln!("attempt {backend} failed: {error}");
        }
        if probe.attempts.is_empty() {
          eprintln!("No light backend was available for this reply.");
        }
        std::process::exit(1);
      };
      let answer = probe.answer.expect("a successful reply probe has the answering backend's response");
      let model_findings = cold_read::parse_reply_findings(&answer).map_err(anyhow_exit)?;
      if !model_findings.is_empty() {
        client::post(
          "/api/internal/web/reply-check",
          json!({
            "session_id": &session_id,
            "text": &text,
            "backend": &backend,
            "findings": &model_findings,
            "answer": &answer,
          }),
          None,
          &[],
        )?;
      }
      model_findings
    };

    if !findings.is_empty() {
      for finding in &findings {
        println!("FAIL: {finding}");
      }
      println!("Nothing was posted.");
      std::process::exit(3);
    }
  }

  let mut payload = Map::new();
  payload.insert("session_id".to_string(), json!(session_id));
  payload.insert("text".to_string(), json!(text));
  if let Some(reason) = &args.keep_language {
    payload.insert("keep_language".to_string(), json!(reason));
  }
  let readback = client::post("/api/internal/web/reply", Value::Object(payload), None, &[])?;
  print!("{}", json_out::render(&readback, None));
  Ok(())
}

fn dialog(args: &SessionArgs) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  let result = client::get("/api/internal/web/dialog", Some(vec![("session_id".to_string(), session_id)]))?;
  let mut blocks = Vec::new();
  for entry in list(field(&result, "entries")?)? {
    let label = match text_repr::value_str(field(entry, "role")?).as_str() {
      "user" => "You",
      event_types::WEB_REPLY => "Reply",
      event_types::WEB_REPLY_NOTICE => "Notice",
      role => return Err(CliExit::error(format!("unknown dialog role {role}"))),
    };
    blocks.push(format!("{label}: {}", text_repr::value_str(field(entry, "content")?)));
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
  gate_fork_assertions(host, unread)?;
  let (system_prompt, questions) = cold_read::load_ask_texts(&host.repo_root).map_err(anyhow_exit)?;
  let policy_section = cold_read::ask_policy_section(&host.cfg.charliebot_home).map_err(anyhow_exit)?;
  for item in unread {
    let link = item.get("link").and_then(Value::as_str);
    let linked_page = match file_urls::host_file(link).map_err(CliExit::error)? {
      None => None,
      Some(path) => Some(files::read_text_mode(&path)?),
    };
    let probe_prompt =
      cold_read::ask_probe_input(item, linked_page.as_deref(), &policy_section, &questions).map_err(anyhow_exit)?;
    let probe = block_on(cold_read::run_probe(host, &probe_prompt, &system_prompt))?.map_err(anyhow_exit)?;
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
    println!("Waiting on you: {}\nBackend: {backend}\n{answer}\n", text_repr::value_str(field(item, "text")?));
  }
  println!("Nothing was stored.");
  std::process::exit(3);
}

/// The gate before the cold reads: every unread item's host-served page runs the fork assertions of its genre, taken
/// from the page's file-name prefix. A failing page stops the command before any item is read.
fn gate_fork_assertions(host: &CliHost, unread: &[Value]) -> Result<(), CliExit> {
  let run = host.registry.fork_assertions_runner().map_err(anyhow_exit)?;
  for item in unread {
    let Some(path) = file_urls::host_file(item.get("link").and_then(Value::as_str)).map_err(CliExit::error)? else {
      continue;
    };
    let Some(failures) = run(&path, &host.cfg.charliebot_home).map_err(anyhow_exit)? else {
      continue;
    };
    if failures.is_empty() {
      continue;
    }
    println!("page {} fails its fork assertions:", path.display());
    for (name, detail) in failures {
      println!("FAIL {name}: {detail}");
    }
    println!("Nothing was stored.");
    std::process::exit(3);
  }
  Ok(())
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

/// The GET result as plain text, the session's Goal, Now and Waiting on you, then the derived items the
/// server holds for the session in their own block, then one block per child with waiting items, then the Now of each
/// open child.
fn render_status(result: &Value) -> Result<String, CliExit> {
  let mut lines: Vec<String> = Vec::new();
  let status = field(result, event_types::WEB_STATUS)?;
  if status.is_null() {
    lines.push("No status yet".to_string());
  } else {
    lines.push(format!("Goal: {}", or_not_set(field(status, "goal")?)));
    lines.push(format!(
      "Now: {} (updated {})",
      or_not_set(field(status, "now")?),
      text_repr::value_str(field(status, "updated_at")?)
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
  let derived = list(field(result, "derived_waiting")?)?;
  if !derived.is_empty() {
    lines.push(String::new());
    lines.push("Derived by the server (not stored):".to_string());
    for item in derived {
      lines.extend(derived_item_lines(item)?);
    }
  }
  for row in list(field(result, "descendants")?)? {
    lines.push(String::new());
    lines.push(format!("{} ({})", text_repr::value_str(field(row, "name")?), short_session_id(row)?));
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
        text_repr::value_str(field(row, "name")?),
        short_session_id(row)?,
        text_repr::value_str(field(row, "now")?),
        text_repr::value_str(field(row, "updated_at")?)
      ));
    }
  }
  Ok(lines.join("\n"))
}

/// The item line, its `since` and `since_exact` lines, and its context when one is stored, indented below.
fn item_lines(item: &Value) -> Result<Vec<String>, CliExit> {
  let mut lines = vec![
    item_line(item)?,
    format!("  since: {}", text_repr::value_str(field(item, "since")?)),
    format!("  since_exact: {}", text_repr::json_dumps(field(item, "since_exact")?, &text_repr::DEFAULT_JSON)),
  ];
  lines.extend(context_lines(item));
  Ok(lines)
}

/// A derived item's lines: its text, its start time and its context. The server gives a derived item no `since_exact`.
fn derived_item_lines(item: &Value) -> Result<Vec<String>, CliExit> {
  let mut lines = vec![item_line(item)?, format!("  since: {}", text_repr::value_str(field(item, "since")?))];
  lines.extend(context_lines(item));
  Ok(lines)
}

/// The item's context, indented below it: its first line after `context:` and each later line under it.
fn context_lines(item: &Value) -> Vec<String> {
  let Some(context) = item.get("context").filter(|context| !context.is_null()) else {
    return Vec::new();
  };
  let context = text_repr::value_str(context);
  let context_lines = splitlines(&context);
  let Some((first, rest)) = context_lines.split_first() else {
    return Vec::new();
  };
  let mut lines = vec![format!("  context: {first}")];
  lines.extend(rest.iter().map(|line| format!("  {line}")));
  lines
}

/// `- <text>`, with ` (<link>)` appended when the item names a link.
fn item_line(item: &Value) -> Result<String, CliExit> {
  let text = text_repr::value_str(field(item, "text")?);
  let link = field(item, "link")?;
  Ok(if text_repr::is_truthy(link) {
    format!("- {text} ({})", text_repr::value_str(link))
  } else {
    format!("- {text}")
  })
}

fn short_session_id(row: &Value) -> Result<String, CliExit> {
  Ok(text_repr::value_str(field(row, "session_id")?).chars().take(8).collect())
}

fn or_not_set(value: &Value) -> String {
  if text_repr::is_truthy(value) { text_repr::value_str(value) } else { "(not set)".to_string() }
}

fn list(value: &Value) -> Result<&Vec<Value>, CliExit> {
  value.as_array().ok_or_else(|| CliExit::error("the response carries a field that is not a list"))
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

  /// A GET result for a status that carries `status` and no descendant, child or derived rows.
  fn status_result(status: serde_json::Value) -> serde_json::Value {
    json!({"web_status": status, "descendants": [], "children": [], "derived_waiting": []})
  }

  #[test]
  fn a_status_prints_goal_now_and_the_waiting_items_in_order() {
    let result = status_result(json!({
      "goal": "Ship the panel",
      "now": "Reviewing",
      "waiting": [],
      "updated_at": "2026-11-02T03:10:00Z",
    }));

    let text = render_status(&result).expect("the status renders");

    assert_eq!(
      text.lines().collect::<Vec<_>>(),
      vec!["Goal: Ship the panel", "Now: Reviewing (updated 2026-11-02T03:10:00Z)", "Waiting on you: Nothing"]
    );
  }
}
