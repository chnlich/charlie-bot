//! The `disk-sweep` verb: `list` prints every candidate the scheduled `disk_sweep` handler would consider, with its
//! verdict. It runs the same scan and checks as the handler and removes nothing.

use std::ffi::OsString;

use charliebot_agents::cli_host::LazyHost;
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_disk_sweep::sweep::{SweepMode, SweepRoots, run_sweep};
use chrono::Utc;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "disk-sweep", about = "Inspect what the disk sweep would remove, without removing anything")]
struct DiskSweepCli {
  #[command(subcommand)]
  verb: DiskSweepVerb,
}

#[derive(Subcommand)]
enum DiskSweepVerb {
  /// Print every candidate as a TSV row (kind, path, freed_bytes, action, reason), then a total line
  List,
}

/// Parse the arguments and run one verb. Parser errors print and exit on their own: help exits 0, a usage error
/// exits 2.
pub fn main(args: &[OsString], lazy_host: &LazyHost) -> Result<(), CliExit> {
  let command = with_infer_long_args(DiskSweepCli::command());
  let argv = std::iter::once(OsString::from("charliebot disk-sweep")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = DiskSweepCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  let DiskSweepVerb::List = cli.verb;
  let host = lazy_host.get().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let roots = SweepRoots::from_config(&host.cfg);
  // `List` never calls the remover; the registry supplies it because the sweep's signature takes one.
  let remove_worktree = host.registry.worktree_remover().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| CliExit::error(format!("{error:#}")))?;
  let report = runtime
    .block_on(run_sweep(roots, SweepMode::List, Utc::now(), remove_worktree))
    .map_err(|error| CliExit::error(format!("{error:#}")))?;
  print!("{}", report.list_lines());
  Ok(())
}
