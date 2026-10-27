//! The `discord` verbs (`src/features/discord/cli.py`): reply on a session's own Discord thread, read thread messages,
//! and check the bot's setup.
//!
//! `reply` posts the file's text to the thread the session was summoned from and prints the server's readback. `read`
//! reads the session's own thread and marks the unread messages it returned as read, or with `--url` reads the linked
//! channel or thread and marks nothing. `check` prints the setup readback and exits 1 when `ok` is false.

use std::ffi::OsString;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_cli_common::reply::post_reply;
use charliebot_cli_common::resolve;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};

/// `constants.DISCORD_READ_LIMIT_MIN`, `_MAX` and `_DEFAULT`: the page-size bounds the parser and the endpoint share.
const READ_LIMIT_MIN: i64 = 1;
const READ_LIMIT_MAX: i64 = 100;
const READ_LIMIT_DEFAULT: i64 = 50;

#[derive(Parser)]
#[command(name = "discord", about = "CharlieBot Discord thread verbs")]
struct DiscordCli {
  #[command(subcommand)]
  command: DiscordCommand,
}

#[derive(Subcommand)]
enum DiscordCommand {
  /// Post a reply to this session's Discord thread
  Reply(ReplyArgs),
  /// Read thread messages, marking the returned unread ones read
  Read(ReadArgs),
  /// Verify the Discord bot setup; exits 1 when ok is false
  Check,
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
  /// Discord link to read instead of this session's own thread; marks nothing read
  #[arg(long)]
  url: Option<String>,
  /// Messages per page, 1..100 (default 50)
  #[arg(long, default_value_t = READ_LIMIT_DEFAULT, value_name = "N")]
  limit: i64,
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the master environment)
  #[arg(long)]
  session: Option<String>,
}

/// Parse the Discord arguments and run one verb. Parser errors print and exit on their own (argparse-compatible: help
/// 0, usage error 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(DiscordCli::command());
  let argv = std::iter::once(OsString::from("charliebot discord")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = DiscordCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    DiscordCommand::Reply(args) => reply(&args),
    DiscordCommand::Read(args) => read(&args),
    DiscordCommand::Check => check(),
  }
}

fn reply(args: &ReplyArgs) -> Result<(), CliExit> {
  post_reply("/api/internal/discord/reply", args.session.as_deref(), &args.file, None)
}

fn read(args: &ReadArgs) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  if !(READ_LIMIT_MIN..=READ_LIMIT_MAX).contains(&args.limit) {
    return Err(CliExit::usage(format!(
      "--limit must be between {READ_LIMIT_MIN} and {READ_LIMIT_MAX}, got: {}",
      args.limit
    )));
  }
  let result = client::post(
    "/api/internal/discord/read",
    json!({ "session_id": session_id, "url": args.url, "limit": args.limit }),
    None,
    &[],
  )?;
  print!("{}", json_out::render(&result, None));
  Ok(())
}

fn check() -> Result<(), CliExit> {
  let result = client::post("/api/internal/discord/check", json!({}), None, &[])?;
  print!("{}", json_out::render(&result, None));
  if result.get("ok").and_then(Value::as_bool) != Some(true) {
    std::process::exit(1);
  }
  Ok(())
}
