//! The `land` verb: the implement landing's thin CLI entry over the tasks crate's gate module.
//!
//! `charliebot land` serves the task of the current run: the server writes `CHARLIEBOT_SESSION_ID`
//! into every run's environment, and the session resolution refuses an explicit `--task` naming a
//! different session, so only the task's own run can land it. A blocked landing's recovery is a new
//! implement delegation (skills/charliebot/SKILL.md, "Merge-back failover"). One JSON line goes to
//! stdout with the record `landing.json` also holds, and each state carries its own exit code
//! (`landed` and `landed-no-ci` exit 0).

use std::ffi::OsString;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::{self, CliExit};
use charliebot_cli_common::json_out;
use charliebot_cli_common::resolve;
use clap::{Arg, Command};

use crate::commands::session::home_dir;

const ABOUT: &str = "Land an implement task: rebase, push the task branch, keep one draft PR, move the base \
     branch only after CI passed on the head sha, and leave no PR of the task branch open";

pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(Command::new("land").about(ABOUT).arg(
    Arg::new("task").long("task").value_name("SESSION_ID").help(
      "Task session id (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the run environment)",
    ),
  ));
  let matches = match command
    .clone()
    .try_get_matches_from(std::iter::once(OsString::from("charliebot land")).chain(args.iter().cloned()))
  {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let task = matches.get_one::<String>("task").map(String::as_str);
  let session_id = resolve::resolve_from_env(task)?;
  let home = home_dir()?;
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| CliExit::error(format!("tokio runtime failed to start: {}", exit::source_chain(&error))))?;
  match runtime.block_on(charliebot_tasks::land::land(&home, &session_id)) {
    Ok(record) => {
      let exit_code = charliebot_tasks::land::LandState::try_from(record.state.as_str())
        .map_err(|error| CliExit::error(format!("landing record holds an unknown state: {error}")))?
        .exit_code();
      json_out::print_result(&serde_json::to_value(&record).map_err(|error| CliExit::error(error.to_string()))?);
      if exit_code != 0 {
        std::process::exit(exit_code);
      }
      Ok(())
    }
    Err(error) => Err(CliExit::error(format!("landing failed: {error:#}"))),
  }
}
