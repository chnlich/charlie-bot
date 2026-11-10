//! The argument-parser shape every subcommand shares: unique long-option prefixes.

use std::ffi::OsString;
use std::path::Path;

use clap::error::ErrorKind;
use clap::{Command, CommandFactory, FromArgMatches};

/// The `--repo` validator: it rejects a value that is not an absolute path or does not exist as a
/// directory.
///
/// It raises the parser's usage error (exit 2) before any network call, so a bad repo path never
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

/// The typed CLI of `program` parsed from `args`, with the parser's own exit for `--help`, `--version` and a usage
/// error.
///
/// The program name stands in argv[0] so help and usage errors name the verb the operator typed, and
/// [`with_infer_long_args`] lets a unique long-option prefix name its option. `from_arg_matches` cannot fail after
/// the parser accepted the same matches.
pub fn parse<C>(program: &str, args: &[OsString]) -> C
where
  C: CommandFactory + FromArgMatches,
{
  let command = with_infer_long_args(C::command());
  let argv = std::iter::once(OsString::from(program)).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  C::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments")
}

/// The command with `infer_long_args` enabled on it and on every nested subcommand.
///
/// A unique prefix of a long option names it (`--task-spec` names `--task-spec-file`), and agents
/// may pass one. The `clap` parser needs the setting on each command that owns long options.
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
  use clap::Parser;

  use super::*;

  /// A parser with one long option, for the parse helper's own test.
  #[derive(Parser)]
  struct TestCli {
    #[arg(long)]
    message: String,
  }

  #[test]
  fn parse_reads_a_unique_prefix_of_a_long_option() {
    let parsed: TestCli = parse("charliebot test", &[OsString::from("--mess"), OsString::from("x")]);
    assert_eq!(parsed.message, "x");
  }

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
