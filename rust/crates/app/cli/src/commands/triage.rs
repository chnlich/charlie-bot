//! The `triage` command: `triage scan [--since T] [--until T] [--dry-run]`.
//!
//! The dry run collects a window of failed runs and server-log error lines, groups them by signature and prints one
//! JSON verdict per signature. It writes nothing. A scan without `--dry-run` refuses until the ledger lands.

use std::ffi::OsString;
use std::sync::Arc;

use charliebot_agents::cli_host::LazyHost;
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_core::home;
use charliebot_failure_triage::{
  BackendJudge, JudgeBackends, ScanOptions, ledger_path, log_dir_from_environment, preflight, scan,
};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "triage", subcommand_required = true, arg_required_else_help = true)]
struct TriageCli {
  #[command(subcommand)]
  command: TriageCommand,
}

#[derive(Subcommand)]
enum TriageCommand {
  /// Collect a time window of failures, judge each signature and print one JSON row per signature.
  Scan(ScanArgs),
}

#[derive(Args)]
struct ScanArgs {
  /// Window start as an RFC 3339 timestamp (default: seven days before --until).
  #[arg(long)]
  since: Option<String>,
  /// Window end as an RFC 3339 timestamp, exclusive (default: now).
  #[arg(long)]
  until: Option<String>,
  /// Print the verdicts without writing the ledger.
  #[arg(long)]
  dry_run: bool,
}

/// Run the `triage` subcommand.
pub fn main(args: &[OsString], lazy_host: &LazyHost) -> Result<(), CliExit> {
  let command = with_infer_long_args(TriageCli::command());
  let argv = std::iter::once(OsString::from("charliebot triage")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = TriageCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    TriageCommand::Scan(args) => scan_command(args, lazy_host),
  }
}

fn scan_command(args: ScanArgs, lazy_host: &LazyHost) -> Result<(), CliExit> {
  let host = lazy_host.get().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let home = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let log_dir = log_dir_from_environment(&host.base_env.values);
  let options = ScanOptions::new(home.clone(), log_dir.clone(), args.since.as_deref(), args.until.as_deref())
    .map_err(CliExit::usage)?;
  preflight(&host.cfg, &log_dir).map_err(|error| CliExit::error(format!("{error:#}")))?;
  if !args.dry_run {
    let ledger = ledger_path(&home);
    return Err(CliExit::error(format!(
      "the failure triage ledger at {} is not implemented yet; run with --dry-run",
      ledger.display()
    )));
  }
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| CliExit::error(error.to_string()))?;
  let judge = BackendJudge::new(JudgeBackends::new(
    &host.cfg,
    Arc::clone(&host.registry),
    &host.base_env.values,
    Arc::clone(&host.pool),
  ));
  let rows = runtime.block_on(scan(&options, &judge)).map_err(|error| CliExit::error(format!("{error:#}")))?;
  for row in rows {
    let value = serde_json::to_value(&row).map_err(|error| CliExit::error(error.to_string()))?;
    print!("{}", json_out::render(&value, None));
  }
  Ok(())
}
