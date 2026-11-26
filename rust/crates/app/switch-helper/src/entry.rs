//! The switch-helper entry: the subcommand name and the dispatcher that runs one helper.

use std::ffi::OsString;

use charliebot_cli_common::exit::CliExit;

use crate::{api, common, home};

/// The `charliebot` subcommand that reaches [`main`]: the CLI dispatcher lists it in its vocabulary and routes it here.
pub const SUBCOMMAND: &str = "switch-helper";

/// The entry point that runs one switch helper. `--help` exits 0 so that the switch can tell whether a release's
/// binary carries the subcommand at all. An unknown name is a usage error.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let Some(name) = args.first().and_then(|argument| argument.to_str()) else {
    return Err(CliExit::usage(common::USAGE));
  };
  let rest = &args[1..];
  match name {
    "-h" | "--help" => {
      println!("{}", common::USAGE);
      Ok(())
    }
    "home-helper" => home::main(rest),
    "reattach" => home::reattach(rest),
    "json-field" => api::json_field(rest),
    "newest-run-fields" => api::newest_run_fields(rest),
    // A no-match exits 1 in both cases, but only a bad call prints a diagnostic. The switch calls this helper on a
    // poll, so a silent no-match keeps the launch log free of errors.
    "launch-has" => api::launch_has(rest),
    "assistant-messages" => api::assistant_messages(rest),
    "run-states" => api::run_states(rest),
    "delegation-body" => api::delegation_body(rest),
    "run-error" => api::run_error(rest),
    "run-first-command" => api::run_first_command(rest),
    other => Err(CliExit::usage(format!("unknown switch helper {other:?}. {}", common::USAGE))),
  }
}
