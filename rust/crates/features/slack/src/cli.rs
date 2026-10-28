//! The `slack` verbs (`src/features/slack/cli.py`): reply on a session's own Slack thread, acknowledge read thread
//! messages, and send one authenticated Slack Web API request.
//!
//! `reply` posts the file's text to the thread the session was summoned from and prints the server's readback. `ack`
//! advances the session's read watermark and prints the readback; every read message's id must be passed. `request`
//! reads the token inside this process, sends one request and prints the response body.

use std::ffi::OsString;
use std::path::Path;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_cli_common::reply::post_reply;
use charliebot_cli_common::request::{self, RequestArgs};
use charliebot_cli_common::resolve;
use charliebot_core::{config, home, paths};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};

use crate::token::{self, TokenKind};

#[derive(Parser)]
#[command(name = "slack", about = "CharlieBot Slack thread verbs")]
struct SlackCli {
  #[command(subcommand)]
  command: SlackCommand,
}

#[derive(Subcommand)]
enum SlackCommand {
  /// Post a reply to this session's Slack thread
  Reply(ReplyArgs),
  /// Mark read thread messages, advancing the read watermark
  Ack(AckArgs),
  /// Send one authenticated Slack Web API request
  Request(RequestCommand),
}

#[derive(Args)]
struct ReplyArgs {
  /// File holding the reply text; - reads stdin
  #[arg(long)]
  file: String,
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the master environment)
  #[arg(long)]
  session: Option<String>,
}

#[derive(Args)]
struct RequestCommand {
  #[command(flatten)]
  request: RequestArgs,
  /// Which Slack token to authenticate with: the user token by default, the bot token with `bot`.
  #[arg(long, value_name = "KIND", default_value = "user", value_parser = ["user", "bot"])]
  token: String,
}

#[derive(Args)]
struct AckArgs {
  /// Slack ts of each read message; every read id at or below the newest must be included
  #[arg(long = "message-id", required = true, num_args = 1.., value_name = "TS")]
  message_id: Vec<String>,
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the master environment)
  #[arg(long)]
  session: Option<String>,
}

/// Parse the Slack arguments and run one verb. Parser errors print and exit on their own (argparse-compatible: help 0,
/// usage error 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(SlackCli::command());
  let argv = std::iter::once(OsString::from("charliebot slack")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = SlackCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    SlackCommand::Reply(args) => reply(&args),
    SlackCommand::Ack(args) => ack(&args),
    SlackCommand::Request(args) => run_request(&args),
  }
}

fn run_request(args: &RequestCommand) -> Result<(), CliExit> {
  let kind = token_kind(&args.token);
  let home_dir = home::charliebot_home_dir().map_err(|_| credential_failure(kind, None))?;
  run_request_at(&home_dir, args)
}

fn run_request_at(home_dir: &Path, args: &RequestCommand) -> Result<(), CliExit> {
  let kind = token_kind(&args.token);
  let path = home_dir.join(config::CREDENTIALS_FILENAME);
  let token = token::token(home_dir, kind).map_err(|_| credential_failure(kind, Some(&path)))?;
  if token.trim().is_empty() {
    return Err(credential_failure(kind, Some(&path)));
  }
  request::write_stdout(&execute_request(&args.request, &token)?)
}

fn execute_request(args: &RequestArgs, token: &str) -> Result<Vec<u8>, CliExit> {
  let request = args.prepare()?;
  let response = request::send(&request.bearer(token), &[token])?;
  if !(200..300).contains(&response.status) || slack_error(&response.body) {
    return Err(request::http_failure(response.status, &response.body, &[token]));
  }
  Ok(request::redact_bytes(&response.body, &[token]))
}

fn token_kind(name: &str) -> TokenKind {
  if name == "bot" { TokenKind::Bot } else { TokenKind::User }
}

/// Slack answers HTTP 200 with `"ok": false` for an API-level failure.
fn slack_error(body: &[u8]) -> bool {
  serde_json::from_slice::<Value>(body).ok().and_then(|payload| payload.get("ok").and_then(Value::as_bool))
    == Some(false)
}

fn credential_failure(kind: TokenKind, path: Option<&Path>) -> CliExit {
  let where_text = match path {
    Some(path) => paths::display(path),
    None => "$CHARLIEBOT_HOME/credentials.yaml".to_string(),
  };
  CliExit::contract(json!({"error": format!("credentials.slack.{} is unavailable in {where_text}", kind.key())}), 3)
}

fn reply(args: &ReplyArgs) -> Result<(), CliExit> {
  post_reply("/api/internal/slack/reply", args.session.as_deref(), &args.file, None)
}

fn ack(args: &AckArgs) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  let result = client::post(
    "/api/internal/slack/ack",
    json!({ "session_id": session_id, "message_ids": args.message_id }),
    None,
    &[],
  )?;
  print!("{}", json_out::render(&result, None));
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Write};
  use std::net::{TcpListener, TcpStream};
  use std::thread;
  use std::time::Duration;

  use super::*;

  const USER_TOKEN: &str = "synthetic-user-token";
  const BOT_TOKEN: &str = "synthetic-bot-token";
  const TIMEOUT: Duration = Duration::from_secs(2);

  const TABLE: &str =
    concat!("slack:\n", "  user_token: synthetic-user-token\n", "  bot_token: synthetic-bot-token\n",);

  fn credentials_home(text: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(config::CREDENTIALS_FILENAME), text).unwrap();
    home
  }

  fn arguments(url: &str, kind: &str) -> RequestCommand {
    RequestCommand {
      request: RequestArgs {
        method: "GET".to_string(),
        url: url.to_string(),
        data_file: None,
        json: None,
        header: Vec::new(),
      },
      token: kind.to_string(),
    }
  }

  fn error_text(error: &CliExit) -> String {
    serde_json::to_string(&error.body).unwrap()
  }

  fn assert_no_token(text: &str) {
    assert!(!text.contains(USER_TOKEN));
    assert!(!text.contains(BOT_TOKEN));
  }

  /// One accepted connection answering a scripted status and body.
  fn stub(status: u16, body: &[u8]) -> (String, std::sync::mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}/api", listener.local_addr().unwrap());
    let body = body.to_vec();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      sender.send(read_request(&mut stream)).unwrap();
      let response = format!(
        "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      let _ = stream.write_all(response.as_bytes());
      let _ = stream.write_all(&body);
    });
    (url, receiver)
  }

  fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
      let read = stream.read(&mut buffer).unwrap();
      if read == 0 {
        return request;
      }
      request.extend_from_slice(&buffer[..read]);
      if request.windows(4).any(|window| window == b"\r\n\r\n") {
        return request;
      }
    }
  }

  #[test]
  fn the_default_token_is_the_user_token() {
    let home = credentials_home(TABLE);
    let (url, receiver) = stub(200, br#"{"ok":true,"user_id":"U1"}"#);

    let token = token::token(home.path(), token_kind("user")).unwrap();
    let body = execute_request(&arguments(&url, "user").request, &token).unwrap();
    let request = String::from_utf8(receiver.recv_timeout(TIMEOUT).unwrap()).unwrap();

    assert_eq!(token, USER_TOKEN);
    assert_eq!(body, br#"{"ok":true,"user_id":"U1"}"#.to_vec());
    assert!(request.to_ascii_lowercase().contains(&format!("authorization: bearer {USER_TOKEN}")));
    assert!(request.starts_with("GET /api HTTP/1.1"));
    assert_no_token(&String::from_utf8_lossy(&body));
  }

  #[test]
  fn the_bot_flag_reads_the_bot_token() {
    let home = credentials_home(TABLE);
    let (url, receiver) = stub(200, br#"{"ok":true}"#);

    let token = token::token(home.path(), token_kind("bot")).unwrap();
    execute_request(&arguments(&url, "bot").request, &token).unwrap();
    let request = String::from_utf8(receiver.recv_timeout(TIMEOUT).unwrap()).unwrap();

    assert_eq!(token, BOT_TOKEN);
    assert!(request.to_ascii_lowercase().contains(&format!("authorization: bearer {BOT_TOKEN}")));
  }

  #[test]
  fn an_ok_false_body_exits_one_and_redacts_the_token() {
    let (url, receiver) = stub(200, format!(r#"{{"ok":false,"error":"invalid_auth {USER_TOKEN}"}}"#).as_bytes());

    let error = execute_request(&arguments(&url, "user").request, USER_TOKEN).unwrap_err();
    let _ = receiver.recv_timeout(TIMEOUT);
    let text = error_text(&error);

    assert_eq!(error.code, 1);
    assert_eq!(error.body["status"], 200);
    assert!(error.body["response"].as_str().unwrap().contains("[redacted]"));
    assert_no_token(&text);
  }

  #[test]
  fn a_non_2xx_status_exits_one() {
    let (url, receiver) = stub(429, br#"{"error":"rate_limited"}"#);

    let error = execute_request(&arguments(&url, "user").request, USER_TOKEN).unwrap_err();
    let _ = receiver.recv_timeout(TIMEOUT);

    assert_eq!(error.code, 1);
    assert_eq!(error.body["status"], 429);
    assert_no_token(&error_text(&error));
  }

  #[test]
  fn a_missing_token_is_a_credential_failure() {
    let home = credentials_home("slack:\n  bot_token: synthetic-bot-token\n");

    let error = run_request_at(home.path(), &arguments("http://127.0.0.1:1/api", "user")).unwrap_err();

    assert_eq!(error.code, 3);
    assert!(error_text(&error).contains("credentials.slack.user_token"));
    assert_no_token(&error_text(&error));
  }

  #[test]
  fn a_success_body_redacts_the_token() {
    let (url, receiver) = stub(200, format!(r#"{{"ok":true,"echo":"{USER_TOKEN}"}}"#).as_bytes());

    let body = execute_request(&arguments(&url, "user").request, USER_TOKEN).unwrap();
    let _ = receiver.recv_timeout(TIMEOUT);

    assert_eq!(body, br#"{"ok":true,"echo":"[redacted]"}"#.to_vec());
    assert_no_token(&String::from_utf8_lossy(&body));
  }

  #[test]
  fn usage_errors_exit_two_without_a_token() {
    let error = execute_request(&arguments("relative/path", "user").request, USER_TOKEN).unwrap_err();
    assert_eq!(error.code, 2);
    assert_no_token(&error_text(&error));
  }
}
