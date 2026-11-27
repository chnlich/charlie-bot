//! `charliebot usage rebuild-runs`: recomputes a ledger's native run records from their raw run logs under the
//! current capture rules. The rebuild reads every origin before it writes a row, and it fails on a missing origin.

use std::ffi::OsString;
use std::path::PathBuf;

use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::exit::{CliExit, charliebot_home_or_exit};
use charliebot_cli_common::json_out;
use charliebot_usage_page::{UsageLedger, default_ledger_path, host_name, rebuild_run_records};
use clap::{Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(name = "usage", about = "Maintain the token usage ledger")]
struct UsageCli {
  #[command(subcommand)]
  verb: UsageVerb,
}

#[derive(Subcommand)]
enum UsageVerb {
  /// Recompute this host's native run records from their raw logs and print the count rebuilt
  RebuildRuns {
    /// The ledger to rebuild; defaults to the profile home's usage/ledger.sqlite3
    #[arg(long)]
    ledger: Option<PathBuf>,
  },
}

pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let cli: UsageCli = cli_args::parse("charliebot usage", args);
  let UsageVerb::RebuildRuns { ledger } = cli.verb;

  let registry = charliebot_packages::registry().map_err(|error| CliExit::error(error.to_string()))?;
  let home = charliebot_home_or_exit()?;
  let cfg = registry.load_config(&home).map_err(|error| CliExit::error(error.to_string()))?;
  let path = ledger.unwrap_or_else(|| default_ledger_path(&cfg.charliebot_home));
  if !path.is_file() {
    return Err(CliExit::error(format!("no ledger at {}", path.display())));
  }
  let host = host_name().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let mut ledger = UsageLedger::open(path.clone()).map_err(|error| CliExit::error(format!("{error:#}")))?;
  let rebuilt =
    rebuild_run_records(&mut ledger, &host, &cfg, &registry).map_err(|error| CliExit::error(format!("{error:#}")))?;
  ledger.close().map_err(|error| CliExit::error(format!("{error:#}")))?;
  json_out::print_result(&json!({"ledger": path.display().to_string(), "host": host, "rebuilt": rebuilt}));
  Ok(())
}
