//! The `discord` verbs: reply on a session's own Discord thread, read thread messages,
//! and check the bot's setup.
//!
//! `reply` posts the file's text to the thread the session was summoned from and prints the server's readback. `read`
//! reads the session's own thread and marks the unread messages it returned as read, or with `--url` reads the linked
//! channel or thread and marks nothing. `check` prints the setup readback and exits 1 when `ok` is false.

use std::ffi::OsString;

use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::channel_verb::{READ_LIMIT_DEFAULT, ReplyArgs, post_check, post_read, post_reply};
use charliebot_cli_common::exit::CliExit;
use clap::{Args, Parser, Subcommand};
use serde_json::{Map, json};

use crate::api::{CHECK_PATH, READ_PATH, REPLY_PATH};

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
  let cli: DiscordCli = cli_args::parse("charliebot discord", args);
  match cli.command {
    DiscordCommand::Reply(args) => reply(&args),
    DiscordCommand::Read(args) => read(&args),
    DiscordCommand::Check => check(),
  }
}

fn reply(args: &ReplyArgs) -> Result<(), CliExit> {
  post_reply(REPLY_PATH, args.session.as_deref(), &args.file, None)
}

fn read(args: &ReadArgs) -> Result<(), CliExit> {
  let mut extra = Map::new();
  extra.insert("url".to_string(), json!(args.url));
  post_read(READ_PATH, args.session.as_deref(), args.limit, Some(extra))
}

fn check() -> Result<(), CliExit> {
  post_check(CHECK_PATH)
}
