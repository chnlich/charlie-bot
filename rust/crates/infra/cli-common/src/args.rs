//! The argument-parser shape every subcommand shares: argparse's unique long-option prefixes.

use clap::Command;

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
