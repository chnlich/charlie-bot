//! The `backup` verb: `restore` restores one snapshot of the profile home from its restic repository.

use std::ffi::OsString;
use std::path::PathBuf;

use charliebot_agents::cli_host::LazyHost;
use charliebot_backup::config::{BackupConfig, NAME};
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "backup", about = "Restore the CharlieBot profile from its restic repository")]
struct BackupCli {
  #[command(subcommand)]
  verb: BackupVerb,
}

#[derive(Subcommand)]
enum BackupVerb {
  /// Restore one snapshot into an empty directory
  Restore {
    /// Snapshot id, or `latest`
    #[arg(long)]
    snapshot: String,
    /// The empty directory the snapshot restores into
    #[arg(long)]
    target: PathBuf,
  },
}

/// Parse the arguments and run one verb. Parser errors print and exit on their own: help exits 0, a usage error
/// exits 2.
pub fn main(args: &[OsString], lazy_host: &LazyHost) -> Result<(), CliExit> {
  let command = with_infer_long_args(BackupCli::command());
  let argv = std::iter::once(OsString::from("charliebot backup")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = BackupCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  let BackupVerb::Restore { snapshot, target } = cli.verb;
  let host = lazy_host.get().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let cfg = &host.cfg;
  let settings = cfg
    .section::<BackupConfig>(NAME)
    .settings(&cfg.charliebot_home)
    .map_err(|error| CliExit::error(format!("{error:#}")))?;
  settings.restore(&cfg.charliebot_home, &snapshot, &target).map_err(|error| CliExit::error(format!("{error:#}")))?;
  println!("Restored snapshot {snapshot} into {}", target.display());
  Ok(())
}
