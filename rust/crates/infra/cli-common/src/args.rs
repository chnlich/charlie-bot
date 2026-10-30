//! The argument-parser shape every subcommand shares: argparse's unique long-option prefixes.

use std::path::Path;

use clap::Command;
use clap::error::ErrorKind;

/// `validate_repo_path`: reject a `--repo` value that is not an absolute path or does not exist
/// as a directory.
///
/// Raises the parser's usage error (exit 2) before any network call, so a bad repo path never
/// reaches the internal API.
pub fn validate_repo_path(command: &mut Command, repo: &str) {
  if !repo.starts_with('/') {
    command
      .error(
        ErrorKind::ValueValidation,
        format!(
          "--repo must be an absolute path (starting with '/'), got: {}",
          charliebot_core::text_repr::str_repr(repo)
        ),
      )
      .exit();
  }
  if !Path::new(repo).is_dir() {
    command
      .error(
        ErrorKind::ValueValidation,
        format!("--repo does not exist: {}", charliebot_core::text_repr::str_repr(repo)),
      )
      .exit();
  }
}

/// `infer_long_args(true)` over the command and every nested subcommand.
///
/// argparse accepts a unique prefix of a long option (`--task-spec` names `--task-spec-file`), and
/// nothing in the Python tree turns that off, so agents may pass one; clap needs the setting on
/// each command that owns long options.
pub fn with_infer_long_args(command: Command) -> Command {
  let mut command = command.infer_long_args(true);
  for subcommand in command.get_subcommands_mut() {
    with_infer_long_args_in_place(subcommand);
  }
  command
}

fn with_infer_long_args_in_place(command: &mut Command) {
  // `Command`'s builders take and return the value, so an in-place walk swaps through.
  *command = std::mem::replace(command, Command::new("_")).infer_long_args(true);
  for subcommand in command.get_subcommands_mut() {
    with_infer_long_args_in_place(subcommand);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_setting_reaches_nested_subcommands_without_duplicating_them() {
    let command =
      Command::new("session").subcommand(Command::new("send").arg(clap::Arg::new("message").long("message")));
    let command = with_infer_long_args(command);
    assert_eq!(command.get_subcommands().count(), 1);
    // No public getter for the setting: parsing a unique prefix is the observable proof.
    let matches = command.get_matches_from(["session", "send", "--mess", "x"]);
    let send = matches.subcommand_matches("send").expect("the send subcommand parsed");
    assert_eq!(send.get_one::<String>("message").map(String::as_str), Some("x"));
  }
}
