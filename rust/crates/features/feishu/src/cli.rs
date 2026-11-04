//! The `feishu` verbs. `reply` posts on a session's own Feishu thread, and `read` reads that thread and marks the
//! unread messages as read. `download` saves one image or file of it into the session directory. `check` prints the
//! app's setup readback and exits 1 when `ok` is false. `request`, `consent-url` and `exchange-code` drive the user
//! token.

use std::ffi::OsString;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_cli_common::reply::post_reply;
use charliebot_cli_common::request::{self, RequestArgs};
use charliebot_cli_common::resolve;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};

use crate::user_token::{self, Endpoints};

/// The page-size bounds the parser and the endpoint share.
const READ_LIMIT_MIN: i64 = 1;
const READ_LIMIT_MAX: i64 = 100;
const READ_LIMIT_DEFAULT: i64 = 50;

#[derive(Parser)]
#[command(name = "feishu", subcommand_required = true, arg_required_else_help = true)]
struct FeishuCli {
  #[command(subcommand)]
  command: FeishuCommand,
}

#[derive(Subcommand)]
enum FeishuCommand {
  /// Post a reply to this session's Feishu thread
  Reply(ReplyArgs),
  /// Read thread messages, marking the returned unread ones read
  Read(ReadArgs),
  /// Download one image or file of this session's thread into the session directory
  Download(DownloadArgs),
  /// Verify the Feishu app setup; exits 1 when ok is false
  Check,
  /// Send one authenticated Feishu Open API request as the user
  Request(RequestCommand),
  /// Print the browser URL that starts a re-authorization
  ConsentUrl(ConsentUrlArgs),
  /// Exchange an authorization code for tokens and store them
  ExchangeCode(ExchangeCodeArgs),
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
struct ReadArgs {
  /// Messages per page, 1..100 (default 50)
  #[arg(long, default_value_t = READ_LIMIT_DEFAULT, value_name = "N")]
  limit: i64,
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the master environment)
  #[arg(long)]
  session: Option<String>,
}

#[derive(Args)]
struct DownloadArgs {
  /// The message that carries the resource
  message_id: String,
  /// The image key or file key the message's content names
  file_key: String,
  /// Whether the resource is an image or a file
  #[arg(long = "type", value_parser = ["image", "file"], value_name = "KIND")]
  kind: String,
  /// Where to write the bytes; must resolve inside the session directory
  #[arg(long)]
  output: String,
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the master environment)
  #[arg(long)]
  session: Option<String>,
}

#[derive(Args)]
struct RequestCommand {
  #[command(flatten)]
  request: RequestArgs,
}

#[derive(Args)]
struct ConsentUrlArgs {
  /// One granted scope; repeat or list several.
  #[arg(long = "scope", required = true, num_args = 1.., value_name = "SCOPE")]
  scope: Vec<String>,
}

#[derive(Args)]
struct ExchangeCodeArgs {
  /// The `code` query parameter of the browser redirect.
  code: String,
}

/// Parse the Feishu arguments and run one verb. Parser errors print and exit on their own (argparse-compatible: help
/// 0, usage error 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(FeishuCli::command());
  let argv = std::iter::once(OsString::from("charliebot feishu")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = FeishuCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    FeishuCommand::Reply(args) => reply(&args),
    FeishuCommand::Read(args) => read(&args),
    FeishuCommand::Download(args) => download(&args),
    FeishuCommand::Check => check(),
    FeishuCommand::Request(args) => {
      let home_dir = user_token::home_dir()?;
      request::write_stdout(&user_token::execute_request(&home_dir, &args.request, &Endpoints::production())?)
    }
    FeishuCommand::ConsentUrl(args) => {
      let home_dir = user_token::home_dir()?;
      println!("{}", user_token::consent_url(&home_dir, &args.scope, &Endpoints::production())?);
      Ok(())
    }
    FeishuCommand::ExchangeCode(args) => {
      let home_dir = user_token::home_dir()?;
      user_token::exchange_code(&home_dir, &args.code, &Endpoints::production())
    }
  }
}

fn reply(args: &ReplyArgs) -> Result<(), CliExit> {
  post_reply("/api/internal/feishu/reply", args.session.as_deref(), &args.file, None)
}

fn read(args: &ReadArgs) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  if !(READ_LIMIT_MIN..=READ_LIMIT_MAX).contains(&args.limit) {
    return Err(CliExit::usage(format!(
      "--limit must be between {READ_LIMIT_MIN} and {READ_LIMIT_MAX}, got: {}",
      args.limit
    )));
  }
  let result =
    client::post("/api/internal/feishu/read", json!({ "session_id": session_id, "limit": args.limit }), None, &[])?;
  print!("{}", json_out::render(&result, None));
  Ok(())
}

/// The server writes the file, so the output path goes over as an absolute path resolved here, in the caller's
/// working directory.
fn download(args: &DownloadArgs) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  let output = std::path::absolute(&args.output)
    .map_err(|error| CliExit::usage(format!("--output {} is not a usable path: {error}", args.output)))?;
  let output = output.to_str().ok_or_else(|| CliExit::usage(format!("--output {} is not UTF-8", args.output)))?;
  let result = client::post(
    "/api/internal/feishu/download",
    json!({
      "session_id": session_id,
      "message_id": args.message_id,
      "file_key": args.file_key,
      "kind": args.kind,
      "output": output,
    }),
    None,
    &[],
  )?;
  print!("{}", json_out::render(&result, None));
  Ok(())
}

fn check() -> Result<(), CliExit> {
  let result = client::post("/api/internal/feishu/check", json!({}), None, &[])?;
  print!("{}", json_out::render(&result, None));
  if result.get("ok").and_then(Value::as_bool) != Some(true) {
    std::process::exit(1);
  }
  Ok(())
}
