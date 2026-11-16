//! The helper entries a binary serves before it registers a package.
//!
//! The OpenCode backend spawns its relay as a child of `std::env::current_exe()`, and host-auth runs readbacks through
//! the same executable as a child subreaper. One function here owns those checks and exit codes, so neither binary
//! names the feature crate that starts its helper.

use std::ffi::OsString;

/// The exit status of the helper entry that `args` names, or `None` when the binary should run its own command.
///
/// `args` excludes the program name. The relay reports its failure on stderr and exits 1.
pub fn run(args: &[OsString]) -> Option<u8> {
  if let Some(code) = charliebot_host_auth::readback_supervisor_entry(args) {
    return Some(code);
  }
  if args.first().is_some_and(|arg| arg == charliebot_opencode::relay::RELAY_ARG) {
    return Some(match charliebot_opencode::relay::run_args(&args[1..]) {
      Ok(()) => 0,
      Err(error) => {
        eprintln!("OpenCode relay: {error:#}");
        1
      }
    });
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_readback_supervisor_argument_ends_the_binary_before_registration() {
    let args = [OsString::from(charliebot_host_auth::READBACK_SUPERVISOR_ARG)];
    assert_eq!(run(&args), Some(2), "the supervisor refuses an empty command");
  }

  #[test]
  fn the_relay_argument_ends_the_binary_before_registration() {
    let args = [OsString::from(charliebot_opencode::relay::RELAY_ARG)];
    assert_eq!(run(&args), Some(1), "the relay's missing arguments fail the helper rather than fall through");
  }

  #[test]
  fn an_argument_that_names_no_helper_leaves_the_binary_its_own_command() {
    assert_eq!(run(&[]), None);
    assert_eq!(run(&[OsString::from("--check-config")]), None);
  }
}
