//! `charliebot hook <check>`: one entry of the turn-end hook list (`hooks/turn_end.json`).
//!
//! The command reads the shared turn input on stdin and prints the check's answer in Claude Code's Stop-hook form:
//! nothing for a pass, `systemMessage` for a pass with a notice, and `decision: "block"` with a `reason` for a miss.
//! Every failure exits 1, which the server records as an error. Exit 2 would read as a block, so it is never used here.

use std::ffi::OsString;
use std::io::{Read, Write};

use charliebot_chat_threads::thread_entry::{THREAD_REPLY_ENTRY, ThreadPlatform, check_thread_reply};
use charliebot_cli_common::exit::CliExit;
use charliebot_discord::thread_contract::DISCORD;
use charliebot_feishu::thread_contract::FEISHU;
use charliebot_slack::thread_contract::SLACK;
use charliebot_tasks::turn_hooks::{CheckResult, TurnInput};
use charliebot_web_reply::audit::{
  STATUS_REREAD_ENTRY, WEB_REPLY_ENTRY, WEB_STATUS_ENTRY, check_status_reread, check_web_reply, check_web_status,
};

use crate::commands::pending_wake::{self, PENDING_WAKE_ENTRY};

/// Answers the check named by the single argument from the turn input on stdin.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let [name] = args else {
    return Err(CliExit::error(
      "hook takes one check name: web-reply, web-status, thread-reply, pending-wake or status-reread",
    ));
  };
  let name = name.to_str().ok_or_else(|| CliExit::error("the hook check name is not UTF-8"))?;
  let mut input_text = String::new();
  std::io::stdin()
    .read_to_string(&mut input_text)
    .map_err(|error| CliExit::error(format!("could not read the turn input: {error}")))?;
  let input: TurnInput =
    serde_json::from_str(&input_text).map_err(|error| CliExit::error(format!("invalid turn input: {error}")))?;
  let answer = match name {
    WEB_REPLY_ENTRY => check_web_reply(&input).map_err(|error| CliExit::error(format!("{error:#}")))?,
    WEB_STATUS_ENTRY => check_web_status(&input),
    STATUS_REREAD_ENTRY => check_status_reread(&input),
    THREAD_REPLY_ENTRY => match thread_platform(input.kind.as_deref()) {
      Some(platform) => check_thread_reply(&platform, &input).map_err(|error| CliExit::error(format!("{error:#}")))?,
      None => CheckResult::Pass,
    },
    PENDING_WAKE_ENTRY => pending_wake::check(&input)?,
    other => return Err(CliExit::error(format!("unknown hook check {other}"))),
  };
  if let Some(stdout) = answer.stdout() {
    writeln!(std::io::stdout().lock(), "{stdout}")
      .map_err(|error| CliExit::error(format!("could not write the answer: {error}")))?;
  }
  Ok(())
}

/// The thread platform a session kind names: the kind is the platform's name for a session bound to its thread.
fn thread_platform(kind: Option<&str>) -> Option<ThreadPlatform> {
  match kind {
    Some(name) if name == SLACK.name => Some(SLACK),
    Some(name) if name == DISCORD.name => Some(DISCORD),
    Some(name) if name == FEISHU.name => Some(FEISHU),
    _ => None,
  }
}
