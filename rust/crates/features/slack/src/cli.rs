//! The `slack` verbs (`src/features/slack/cli.py`): reply on a session's own Slack thread and acknowledge read thread
//! messages.
//!
//! `reply` posts the file's text to the thread the session was summoned from and prints the server's readback. `ack`
//! advances the session's read watermark and prints the readback; every read message's id must be passed.

use std::ffi::OsString;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_cli_common::reply::post_reply;
use charliebot_cli_common::resolve;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::json;

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
  }
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
