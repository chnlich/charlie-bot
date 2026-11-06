//! The `backup` verbs: restore a restic snapshot of the profile home into an empty directory.

use std::ffi::OsString;
use std::path::PathBuf;

use charliebot_agents::cli_host::LazyHost;
use charliebot_backup::backup;
use charliebot_backup::config::BackupConfig;
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(name = "backup", subcommand_required = true, arg_required_else_help = true)]
struct BackupCli {
  #[command(subcommand)]
  command: BackupCommand,
}

#[derive(Subcommand)]
enum BackupCommand {
  /// Restore a snapshot of the profile home into an empty directory.
  Restore(RestoreArgs),
}

#[derive(Args)]
struct RestoreArgs {
  /// The snapshot id, or `latest`.
  #[arg(long, value_name = "ID|latest")]
  snapshot: String,
  /// The empty directory the snapshot restores into.
  #[arg(long, value_name = "DIR")]
  target: PathBuf,
}

/// Run the `backup` subcommand.
pub fn main(args: &[OsString], lazy_host: &LazyHost) -> Result<(), CliExit> {
  let command = with_infer_long_args(BackupCli::command());
  let argv = std::iter::once(OsString::from("charliebot backup")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = BackupCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    BackupCommand::Restore(args) => restore_command(args, lazy_host),
  }
}

fn restore_command(args: RestoreArgs, lazy_host: &LazyHost) -> Result<(), CliExit> {
  let host = lazy_host.get().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let config = host.cfg.section::<BackupConfig>("backup");
  let repository = config.repository().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let password_file = config.password_file().map_err(|error| CliExit::error(format!("{error:#}")))?;
  backup::restore(&repository, &password_file, &args.snapshot, &args.target)
    .map_err(|error| CliExit::error(format!("{error:#}")))?;
  let restored = json!({"snapshot": args.snapshot, "restored_to": args.target});
  print!("{}", json_out::render(&restored, None));
  Ok(())
}
