//! The `slack` verbs: reply on a session's own Slack thread, acknowledge read thread
//! messages, send one authenticated Slack Web API request, download a message file, and register a draft of a
//! message in the operator's name.
//!
//! `reply` posts the file's text to the thread the session was summoned from and prints the server's readback. `ack`
//! advances the session's read watermark and prints the readback; every read message's id must be passed. `request`
//! reads the token inside this process, sends one request and prints the response body. `download` reads one Slack
//! file's bytes into a named path. The user token is read-only in this CLI: it admits only the read methods in
//! `token::USER_READ_METHODS` and a GET of a file on Slack's file host, and the bot token keeps every method.
//! `draft` registers a channel post, thread reply, DM, or group DM whose card the operator approves; the server sends
//! the card's body from the operator's own account.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::channel_verb::{ReplyArgs, post_reply};
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_cli_common::request::{self, RequestArgs};
use charliebot_cli_common::{draft as draft_register, files, resolve};
use charliebot_core::{config, home, paths};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};

use crate::api::{ACK_PATH, REPLY_PATH};
use crate::download;
use crate::draft_target;
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
  /// Download a Slack-hosted file, such as a message's `url_private`, to a local path
  Download(DownloadCommand),
  /// Register a message in the operator's name for approval on a session card
  Draft(DraftArgs),
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
struct DownloadCommand {
  /// The file URL from a message's `files[]`: its `url_private` or `url_private_download`
  url: String,
  /// Path to write the file's bytes to; the path is written only when the response is the file
  #[arg(long, value_name = "PATH")]
  output: PathBuf,
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

#[derive(Args)]
struct DraftArgs {
  /// Slack channel id for a channel post or thread reply
  #[arg(long, value_name = "CHANNEL")]
  channel: Option<String>,
  /// Thread timestamp; turns the channel post into a thread reply
  #[arg(long, value_name = "TS")]
  thread: Option<String>,
  /// One or more Slack user ids, comma-separated: one user is a DM, several are a group DM
  #[arg(long, value_name = "ID", value_delimiter = ',')]
  users: Option<Vec<String>>,
  /// File holding the message body; - reads stdin
  #[arg(long)]
  file: String,
  /// Draft id this one replaces, whose card loses its button
  #[arg(long, value_name = "DRAFT_ID")]
  replaces: Option<String>,
}

/// Parse the Slack arguments and run one verb. Parser errors print and exit on their own (argparse-compatible: help 0,
/// usage error 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let cli: SlackCli = cli_args::parse("charliebot slack", args);
  match cli.command {
    SlackCommand::Reply(args) => reply(&args),
    SlackCommand::Ack(args) => ack(&args),
    SlackCommand::Request(args) => run_request(&args),
    SlackCommand::Download(args) => run_download(&args),
    SlackCommand::Draft(args) => draft(&args),
  }
}

/// Register a Slack draft. The target is validated and the body read before anything is sent, so an unparseable
/// target registers nothing.
fn draft(args: &DraftArgs) -> Result<(), CliExit> {
  let (target, body) = prepare_draft(args)?;
  draft_register::register("slack", target, body, args.replaces.as_deref())
}

/// The validated target and the body, with no network call.
fn prepare_draft(args: &DraftArgs) -> Result<(Value, String), CliExit> {
  let target = match (&args.channel, &args.users) {
    (Some(_), Some(_)) => return Err(CliExit::usage("--channel and --users are exclusive")),
    (None, None) => return Err(CliExit::usage("one of --channel or --users is required")),
    (Some(channel), None) => json!({"channel": channel, "thread": args.thread}),
    (None, Some(users)) => {
      if args.thread.is_some() {
        return Err(CliExit::usage("--thread applies only to --channel"));
      }
      json!({"users": users})
    }
  };
  let target = draft_target::validate_target(&target).map_err(CliExit::usage)?;
  let body = files::read_exact_text(&args.file)?;
  Ok((target, body))
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
  request::write_stdout(&execute_request(&args.request, kind, &token)?)
}

fn execute_request(args: &RequestArgs, kind: TokenKind, token: &str) -> Result<Vec<u8>, CliExit> {
  let request = args.prepare()?;
  token::admit(kind, &args.url)?;
  let response = request::send(&request.bearer(token), &[token])?;
  if !(200..300).contains(&response.status) || slack_error(&response.body) {
    return Err(request::http_failure(response.status, &response.body, &[token]));
  }
  Ok(request::redact_bytes(&response.body, &[token]))
}

fn run_download(args: &DownloadCommand) -> Result<(), CliExit> {
  let kind = token_kind(&args.token);
  let home_dir = home::charliebot_home_dir().map_err(|_| credential_failure(kind, None))?;
  run_download_at(&home_dir, args)
}

fn run_download_at(home_dir: &Path, args: &DownloadCommand) -> Result<(), CliExit> {
  let kind = token_kind(&args.token);
  let path = home_dir.join(config::CREDENTIALS_FILENAME);
  let token = token::token(home_dir, kind).map_err(|_| credential_failure(kind, Some(&path)))?;
  if token.trim().is_empty() {
    return Err(credential_failure(kind, Some(&path)));
  }
  token::admit_download(kind, &args.url)?;
  let bytes = download::download(&args.url, &args.output, kind, &token)?;
  print!("{}", json_out::render(&json!({"path": paths::display(&args.output), "bytes": bytes}), None));
  Ok(())
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
    None => home::display_path(config::CREDENTIALS_FILENAME),
  };
  CliExit::contract(json!({"error": format!("credentials.slack.{} is unavailable in {where_text}", kind.key())}), 3)
}

fn reply(args: &ReplyArgs) -> Result<(), CliExit> {
  post_reply(REPLY_PATH, args.session.as_deref(), &args.file, None)
}

fn ack(args: &AckArgs) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  let result = client::post(ACK_PATH, json!({ "session_id": session_id, "message_ids": args.message_id }), None, &[])?;
  print!("{}", json_out::render(&result, None));
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Write};
  use std::net::{SocketAddr, TcpListener, TcpStream};
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
    stub_with_type(status, "application/json", body)
  }

  /// One accepted connection answering a scripted status, content type and body.
  fn stub_with_type(status: u16, content_type: &str, body: &[u8]) -> (String, std::sync::mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}/api", listener.local_addr().unwrap());
    let body = body.to_vec();
    let content_type = content_type.to_string();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      sender.send(read_request(&mut stream)).unwrap();
      let response = format!(
        "HTTP/1.1 {status} Status\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      let _ = stream.write_all(response.as_bytes());
      let _ = stream.write_all(&body);
    });
    (url, receiver)
  }

  /// A local endpoint that records an accidental request and can be woken without waiting when no request is made.
  fn no_call_stub() -> (String, SocketAddr, std::sync::mpsc::Receiver<bool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}/api");
    let (sender, receiver) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      let request = read_request(&mut stream);
      let received_request = !request.is_empty();
      sender.send(received_request).unwrap();
      if received_request {
        let body = br#"{"ok":true}"#;
        let response = format!(
          "HTTP/1.1 200 Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
          body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
      }
    });
    (url, address, receiver, server)
  }

  fn assert_no_request(address: SocketAddr, receiver: std::sync::mpsc::Receiver<bool>, server: thread::JoinHandle<()>) {
    let no_request = matches!(receiver.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
    if no_request {
      drop(TcpStream::connect(address).expect("the local stub wakes for cleanup"));
    }
    server.join().unwrap();
    assert!(no_request, "the request was sent before the policy refusal");
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
    let body =
      execute_request(&arguments(&format!("{url}/auth.test"), "user").request, TokenKind::User, &token).unwrap();
    let request = String::from_utf8(receiver.recv_timeout(TIMEOUT).unwrap()).unwrap();

    assert_eq!(token, USER_TOKEN);
    assert_eq!(body, br#"{"ok":true,"user_id":"U1"}"#.to_vec());
    assert!(request.to_ascii_lowercase().contains(&format!("authorization: bearer {USER_TOKEN}")));
    assert!(request.starts_with("GET /api/auth.test HTTP/1.1"));
    assert_no_token(&String::from_utf8_lossy(&body));
  }

  #[test]
  fn auth_test_reaches_the_send_step_with_the_user_token() {
    assert_user_read_method_reaches_send_step("auth.test");
  }

  #[test]
  fn users_list_reaches_the_send_step_with_the_user_token() {
    assert_user_read_method_reaches_send_step("users.list");
  }

  #[test]
  fn conversations_list_reaches_the_send_step_with_the_user_token() {
    assert_user_read_method_reaches_send_step("conversations.list");
  }

  #[test]
  fn conversations_history_reaches_the_send_step_with_the_user_token() {
    assert_user_read_method_reaches_send_step("conversations.history");
  }

  #[test]
  fn conversations_replies_reaches_the_send_step_with_the_user_token() {
    assert_user_read_method_reaches_send_step("conversations.replies");
  }

  #[test]
  fn search_messages_reaches_the_send_step_with_the_user_token() {
    assert_user_read_method_reaches_send_step("search.messages");
  }

  fn assert_user_read_method_reaches_send_step(method: &str) {
    let home = credentials_home(TABLE);
    let (url, receiver) = stub(200, br#"{"ok":true}"#);
    let command = arguments(&format!("{url}/{method}"), "user");

    run_request_at(home.path(), &command).unwrap();
    let request = String::from_utf8(receiver.recv_timeout(TIMEOUT).unwrap()).unwrap();

    assert!(request.starts_with(&format!("GET /api/{method} HTTP/1.1")), "{request}");
    assert!(request.to_ascii_lowercase().contains(&format!("authorization: bearer {USER_TOKEN}")));
  }

  #[test]
  fn a_user_token_write_is_refused_before_any_network_call() {
    let home = credentials_home(TABLE);
    for method in ["chat.postMessage", "reactions.add", "conversations.open", "files.upload"] {
      let (url, address, receiver, server) = no_call_stub();
      let command = arguments(&format!("{url}/{method}"), "user");

      let error = run_request_at(home.path(), &command).unwrap_err();
      let text = error_text(&error);

      assert_eq!(error.code, 1);
      assert!(text.contains(method), "{text}");
      assert!(text.contains("charliebot slack draft") && text.contains("--token bot"), "{text}");
      assert_no_token(&text);
      assert_no_request(address, receiver, server);
    }
  }

  #[test]
  fn a_user_token_url_without_a_method_is_refused() {
    let home = credentials_home(TABLE);
    let (url, address, receiver, server) = no_call_stub();
    let command = arguments(&url, "user");

    let error = run_request_at(home.path(), &command).unwrap_err();

    assert_eq!(error.code, 1);
    assert!(error_text(&error).contains("not a Slack Web API method URL"));
    assert_no_token(&error_text(&error));
    assert_no_request(address, receiver, server);
  }

  #[test]
  fn the_bot_token_keeps_every_method() {
    let home = credentials_home(TABLE);
    let (url, receiver) = stub(200, br#"{"ok":true}"#);
    let command = arguments(&format!("{url}/chat.postMessage"), "bot");

    run_request_at(home.path(), &command).unwrap();
    let request = String::from_utf8(receiver.recv_timeout(TIMEOUT).unwrap()).unwrap();

    assert!(request.starts_with("GET /api/chat.postMessage HTTP/1.1"));
    assert!(request.to_ascii_lowercase().contains(&format!("authorization: bearer {BOT_TOKEN}")));
  }

  #[test]
  fn the_bot_flag_reads_the_bot_token() {
    let home = credentials_home(TABLE);
    let (url, receiver) = stub(200, br#"{"ok":true}"#);

    let token = token::token(home.path(), token_kind("bot")).unwrap();
    execute_request(&arguments(&url, "bot").request, TokenKind::Bot, &token).unwrap();
    let request = String::from_utf8(receiver.recv_timeout(TIMEOUT).unwrap()).unwrap();

    assert_eq!(token, BOT_TOKEN);
    assert!(request.to_ascii_lowercase().contains(&format!("authorization: bearer {BOT_TOKEN}")));
  }

  #[test]
  fn an_ok_false_body_exits_one_and_redacts_the_token() {
    let (url, receiver) = stub(200, format!(r#"{{"ok":false,"error":"invalid_auth {USER_TOKEN}"}}"#).as_bytes());

    let error =
      execute_request(&arguments(&format!("{url}/search.messages"), "user").request, TokenKind::User, USER_TOKEN)
        .unwrap_err();
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

    let error =
      execute_request(&arguments(&format!("{url}/conversations.history"), "user").request, TokenKind::User, USER_TOKEN)
        .unwrap_err();
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

    let body =
      execute_request(&arguments(&format!("{url}/users.list"), "user").request, TokenKind::User, USER_TOKEN).unwrap();
    let _ = receiver.recv_timeout(TIMEOUT);

    assert_eq!(body, br#"{"ok":true,"echo":"[redacted]"}"#.to_vec());
    assert_no_token(&String::from_utf8_lossy(&body));
  }

  #[test]
  fn usage_errors_exit_two_without_a_token() {
    let error = execute_request(&arguments("relative/path", "user").request, TokenKind::User, USER_TOKEN).unwrap_err();
    assert_eq!(error.code, 2);
    assert_no_token(&error_text(&error));
  }

  fn download_args(url: &str, output: &Path, kind: &str) -> DownloadCommand {
    DownloadCommand { url: url.to_string(), output: output.to_path_buf(), token: kind.to_string() }
  }

  fn output_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.png");
    (dir, path)
  }

  #[test]
  fn a_download_admits_the_slack_file_host_for_both_tokens() {
    let url = format!("https://{}/files-pri/T0B9ZNJRCLA-F0CBAJF074H/image.png", token::FILE_HOST);

    for kind in [TokenKind::User, TokenKind::Bot] {
      token::admit_download(kind, &url).unwrap();
    }
    // A Web API method URL stays admitted only by `slack request`, not by the download verb.
    let api = token::admit_download(TokenKind::User, "https://slack.com/api/conversations.history").unwrap_err();
    assert_eq!(api.code, 1);
    assert!(error_text(&api).contains("not a Slack file URL"), "{}", error_text(&api));
    assert_no_token(&error_text(&api));
    let plain = token::admit_download(TokenKind::User, "http://files.slack.com/files-pri/T-F/image.png");
    assert_eq!(plain.unwrap_err().code, 1, "a file URL over http is not admitted");
  }

  #[test]
  fn a_user_token_download_writes_the_file_bytes_and_sends_the_token() {
    let home = credentials_home(TABLE);
    let (_dir, path) = output_path();
    let png = b"\x89PNG\r\n\x1a\nsynthetic-image-bytes";
    let (url, receiver) = stub_with_type(200, "image/png", png);
    let file_url = format!("{url}/files-pri/T0B9ZNJRCLA-F0CBAJF074H/image.png");
    let user_token = token::token(home.path(), TokenKind::User).unwrap();

    let bytes = download::download(&file_url, &path, TokenKind::User, &user_token).unwrap();
    let request = String::from_utf8(receiver.recv_timeout(TIMEOUT).unwrap()).unwrap();

    assert_eq!(bytes, png.len());
    assert_eq!(std::fs::read(&path).unwrap(), png);
    assert!(request.starts_with("GET /api/files-pri/T0B9ZNJRCLA-F0CBAJF074H/image.png HTTP/1.1"), "{request}");
    assert!(request.to_ascii_lowercase().contains(&format!("authorization: bearer {USER_TOKEN}")));
    assert_no_token(&String::from_utf8_lossy(&std::fs::read(&path).unwrap()));
  }

  #[test]
  fn a_sign_in_page_exits_one_and_leaves_no_file() {
    let (_dir, path) = output_path();
    let (url, receiver) = stub_with_type(200, "text/html; charset=utf-8", b"<!DOCTYPE html><html>Sign in</html>");

    let error =
      download::download(&format!("{url}/files-pri/T-F/image.png"), &path, TokenKind::User, USER_TOKEN).unwrap_err();
    let _ = receiver.recv_timeout(TIMEOUT);

    assert_eq!(error.code, 1);
    assert!(error_text(&error).contains("sign-in page"), "{}", error_text(&error));
    assert!(error_text(&error).contains("files:read"), "{}", error_text(&error));
    assert_no_token(&error_text(&error));
    assert!(!path.exists(), "a non-file answer leaves no output file");
  }

  #[test]
  fn a_sign_in_redirect_exits_one_and_leaves_no_file() {
    let (_dir, path) = output_path();
    let (url, receiver) = stub_with_type(302, "text/html; charset=utf-8", b"<a href=\"signin\">Found</a>.");

    let error =
      download::download(&format!("{url}/files-pri/T-F/image.png"), &path, TokenKind::User, USER_TOKEN).unwrap_err();
    let _ = receiver.recv_timeout(TIMEOUT);

    assert_eq!(error.code, 1);
    assert!(error_text(&error).contains("redirected"), "{}", error_text(&error));
    assert!(error_text(&error).contains("files:read"), "{}", error_text(&error));
    assert!(!path.exists());
  }

  #[test]
  fn a_slack_api_error_exits_one_and_leaves_no_file() {
    let (_dir, path) = output_path();
    let (url, receiver) = stub_with_type(200, "application/json", br#"{"ok":false,"error":"file_not_found"}"#);

    let error =
      download::download(&format!("{url}/files-pri/T-F/image.png"), &path, TokenKind::Bot, BOT_TOKEN).unwrap_err();
    let _ = receiver.recv_timeout(TIMEOUT);

    assert_eq!(error.code, 1);
    assert!(error_text(&error).contains("file_not_found"), "{}", error_text(&error));
    assert!(error_text(&error).contains("slack.bot_token"), "{}", error_text(&error));
    assert_no_token(&error_text(&error));
    assert!(!path.exists());
  }

  #[test]
  fn a_download_off_the_slack_file_host_is_refused_before_any_network_call() {
    let home = credentials_home(TABLE);
    let (_dir, path) = output_path();
    let (url, address, receiver, server) = no_call_stub();

    let error = run_download_at(home.path(), &download_args(&format!("{url}/files-pri/T-F/image.png"), &path, "user"))
      .unwrap_err();
    let text = error_text(&error);

    assert_eq!(error.code, 1);
    assert!(text.contains("not a Slack file URL"), "{text}");
    assert_no_token(&text);
    assert_no_request(address, receiver, server);
    assert!(!path.exists());
  }

  #[test]
  fn slack_request_still_refuses_a_file_url_for_the_user_token() {
    let home = credentials_home(TABLE);
    let (url, address, receiver, server) = no_call_stub();
    let command = arguments(&format!("{url}/files-pri/T-F/image.png"), "user");

    let error = run_request_at(home.path(), &command).unwrap_err();

    assert_eq!(error.code, 1);
    assert!(error_text(&error).contains("not a Slack Web API method URL"), "{}", error_text(&error));
    assert_no_token(&error_text(&error));
    assert_no_request(address, receiver, server);
  }

  #[test]
  fn a_download_without_a_token_is_a_credential_failure() {
    let home = credentials_home("slack:\n  user_token: synthetic-user-token\n");
    let (_dir, path) = output_path();

    let error =
      run_download_at(home.path(), &download_args("https://files.slack.com/files-pri/T-F/image.png", &path, "bot"))
        .unwrap_err();

    assert_eq!(error.code, 3);
    assert!(error_text(&error).contains("credentials.slack.bot_token"));
    assert_no_token(&error_text(&error));
  }

  fn draft_args(channel: Option<&str>, thread: Option<&str>, users: Option<&[&str]>, file: &str) -> DraftArgs {
    DraftArgs {
      channel: channel.map(str::to_string),
      thread: thread.map(str::to_string),
      users: users.map(|users| users.iter().map(|user| user.to_string()).collect()),
      file: file.to_string(),
      replaces: None,
    }
  }

  fn body_file(text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("body.txt"), text).unwrap();
    dir
  }

  #[test]
  fn a_channel_draft_targets_the_channel_with_an_optional_thread() {
    let dir = body_file("the body");
    let file = dir.path().join("body.txt");
    let file = file.to_str().unwrap();

    let (target, body) = prepare_draft(&draft_args(Some("C0SYNTHETIC"), None, None, file)).unwrap();
    assert_eq!(target, json!({"channel": "C0SYNTHETIC", "thread": null}));
    assert_eq!(body, "the body");

    let (target, _) = prepare_draft(&draft_args(Some("C0SYNTHETIC"), Some("1700000000.000100"), None, file)).unwrap();
    assert_eq!(target, json!({"channel": "C0SYNTHETIC", "thread": "1700000000.000100"}));
  }

  #[test]
  fn a_users_draft_names_one_user_or_several() {
    let dir = body_file("the body");
    let file = dir.path().join("body.txt");
    let file = file.to_str().unwrap();

    let (dm, _) = prepare_draft(&draft_args(None, None, Some(&["U0SYNTHETIC"]), file)).unwrap();
    assert_eq!(dm, json!({"users": ["U0SYNTHETIC"]}));
    let (group, _) = prepare_draft(&draft_args(None, None, Some(&["U0SYNTHETIC", "U1SYNTHETIC"]), file)).unwrap();
    assert_eq!(group, json!({"users": ["U0SYNTHETIC", "U1SYNTHETIC"]}));
  }

  #[test]
  fn the_target_forms_are_exclusive_and_a_thread_needs_a_channel() {
    let dir = body_file("the body");
    let file = dir.path().join("body.txt");
    let file = file.to_str().unwrap();

    let both = prepare_draft(&draft_args(Some("C0SYNTHETIC"), None, Some(&["U0SYNTHETIC"]), file)).unwrap_err();
    assert_eq!(both.code, 2);
    assert!(error_text(&both).contains("--channel and --users are exclusive"), "{}", error_text(&both));

    let neither = prepare_draft(&draft_args(None, None, None, file)).unwrap_err();
    assert_eq!(neither.code, 2);
    assert!(error_text(&neither).contains("one of --channel or --users is required"));

    let thread_on_users =
      prepare_draft(&draft_args(None, Some("1700000000.000100"), Some(&["U0SYNTHETIC"]), file)).unwrap_err();
    assert_eq!(thread_on_users.code, 2);
    assert!(error_text(&thread_on_users).contains("--thread applies only to --channel"));
  }

  #[test]
  fn an_unparseable_target_exits_two_before_registering() {
    let dir = body_file("the body");
    let file = dir.path().join("body.txt");
    let file = file.to_str().unwrap();

    let error = prepare_draft(&draft_args(Some("not a channel id"), None, None, file)).unwrap_err();
    assert_eq!(error.code, 2, "an invalid target never reaches the registration call");
    assert!(error_text(&error).contains("target.channel"));

    let error = prepare_draft(&draft_args(None, None, Some(&[]), file)).unwrap_err();
    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("at least one Slack user"));
  }

  #[test]
  fn a_missing_body_exits_two_before_registering() {
    let error = prepare_draft(&draft_args(Some("C0SYNTHETIC"), None, None, "/no/such/body")).unwrap_err();
    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("--file not found"));
  }
}
