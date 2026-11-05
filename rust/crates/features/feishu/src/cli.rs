//! The `feishu` verbs. `reply` posts on a session's topic. Plain `read` reads that topic and marks unread messages;
//! `--chat` and `--topic` read group messages without marking them. `download` saves a group image or file in the
//! session directory. `check` prints setup status, and the user-token verbs manage user authorization.

use std::ffi::OsString;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::channel_verb::{READ_LIMIT_DEFAULT, ReplyArgs, post_check, post_read, post_reply};
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_cli_common::request::{self, RequestArgs};
use charliebot_cli_common::resolve;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Map, json};

use crate::user_token::{self, Endpoints};

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
  /// Read this session's topic, the group's main chat, or another topic
  Read(ReadArgs),
  /// Download one image or file from this session's group into the session directory
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
struct ReadArgs {
  /// Messages per page, 1..100 (default 50)
  #[arg(long, default_value_t = READ_LIMIT_DEFAULT, value_name = "N")]
  limit: i64,
  /// Read the main chat of this session's group, newest first
  #[arg(long, conflicts_with = "topic")]
  chat: bool,
  /// Read one topic of this session's group, oldest first
  #[arg(long, value_name = "THREAD_ID", conflicts_with = "chat")]
  topic: Option<String>,
  /// Continue a main-chat or topic page
  #[arg(long, value_name = "TOKEN")]
  page: Option<String>,
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
  let extra = read_mode_fields(args)?;
  post_read("/api/internal/feishu/read", args.session.as_deref(), args.limit, extra)
}

fn read_mode_fields(args: &ReadArgs) -> Result<Option<Map<String, serde_json::Value>>, CliExit> {
  if args.page.is_some() && !args.chat && args.topic.is_none() {
    return Err(CliExit::usage("--page requires --chat or --topic"));
  }
  let fields = if args.chat {
    let mut fields = Map::from_iter([("mode".to_string(), json!("chat"))]);
    if let Some(page) = &args.page {
      fields.insert("page".to_string(), json!(page));
    }
    fields
  } else if let Some(topic) = &args.topic {
    let mut fields = Map::from_iter([
      ("mode".to_string(), json!("topic")),
      ("topic_id".to_string(), json!(topic)),
    ]);
    if let Some(page) = &args.page {
      fields.insert("page".to_string(), json!(page));
    }
    fields
  } else {
    return Ok(None);
  };
  Ok(Some(fields))
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
  post_check("/api/internal/feishu/check")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn read_modes_accept_only_their_declared_parameters() {
    assert!(FeishuCli::try_parse_from(["feishu", "read"]).is_ok());
    assert!(FeishuCli::try_parse_from(["feishu", "read", "--chat", "--page", "older"]).is_ok());
    assert!(FeishuCli::try_parse_from(["feishu", "read", "--topic", "omt_topic"]).is_ok());
    let topic = FeishuCli::try_parse_from(["feishu", "read", "--topic", "omt_topic", "--page", "older"])
      .unwrap();
    let FeishuCommand::Read(topic_args) = topic.command else {
      panic!("topic arguments produce the read command");
    };
    let fields = read_mode_fields(&topic_args).unwrap().unwrap();
    assert_eq!(fields["mode"], "topic");
    assert_eq!(fields["topic_id"], "omt_topic");
    assert_eq!(fields["page"], "older");
    assert!(FeishuCli::try_parse_from(["feishu", "read", "--chat", "--topic", "omt_topic"]).is_err());
    let FeishuCommand::Read(args) = FeishuCli::try_parse_from(["feishu", "read", "--page", "older"])
      .unwrap()
      .command
    else {
      panic!("read arguments produce the read command");
    };
    assert!(read_mode_fields(&args).is_err());
    assert!(FeishuCli::try_parse_from(["feishu", "read", "--group", "oc_other"]).is_err());
  }
}
