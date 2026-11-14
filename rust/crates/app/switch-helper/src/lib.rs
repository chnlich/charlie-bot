//! The `charliebot switch-helper` subcommands: the reads `scripts/switch-server.sh` performs on the profile home, on
//! a run record and on an API response body.
//!
//! The switch script runs the `charliebot` binary of the release it reads with (see the script's header). Every
//! helper prints exactly what the switch compares and exits with the code it branches on. A helper that matched
//! prints nothing and exits 0; one that did not match exits 1. The nine names each take the arguments the switch
//! passes. `reattach REPO SHA BACKEND_TYPE` is an operator's check of one backend type's re-attachment declaration at
//! a build; the switch does not call it.

use std::ffi::OsString;

use charliebot_cli_common::exit::CliExit;

mod api;
mod common;
mod home;
mod process;
mod scan;

/// Run one switch helper. `--help` exits 0 so that the switch can tell whether a release's binary carries the
/// subcommand at all; an unknown name is a usage error.
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
