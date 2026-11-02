//! The `triage` command: `triage scan [--since T] [--until T] [--cron TASK]`.
//!
//! The scan collects a window of failed runs and server-log error lines, groups them by signature and prints the
//! window followed by one JSON verdict per signature. It writes nothing.

use std::ffi::OsString;
use std::sync::Arc;

use charliebot_agents::cli_host::LazyHost;
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_cli_common::resolve;
use charliebot_core::home;
use charliebot_failure_triage::{BackendJudge, JudgeBackends, ScanOptions, log_dir_from_environment, preflight, scan};
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
  #[arg(long, conflicts_with = "cron")]
  since: Option<String>,
  /// Window end as an RFC 3339 timestamp, exclusive (default: now).
  #[arg(long, conflicts_with = "cron")]
  until: Option<String>,
  /// Take the window from the two latest fires of this cron task in the bound session.
  #[arg(long, value_name = "TASK")]
  cron: Option<String>,
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
  let options = match args.cron.as_deref() {
    Some(task) => {
      let session_id = resolve::resolve_from_env(None)?;
      ScanOptions::for_cron(home.clone(), log_dir.clone(), &session_id, task)
        .map_err(|error| CliExit::error(format!("{error:#}")))?
    }
    None => ScanOptions::new(home.clone(), log_dir.clone(), args.since.as_deref(), args.until.as_deref())
      .map_err(CliExit::usage)?,
  };
  preflight(&host.cfg, &log_dir).map_err(|error| CliExit::error(format!("{error:#}")))?;
  let window = serde_json::json!({
    "window": {
      "since": options.since.to_string(),
      "until": options.until.to_string(),
    }
  });
  print!("{}", json_out::render(&window, None));
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

#[cfg(test)]
mod tests {
  use super::*;

  fn matches(args: &[&str]) -> Result<clap::ArgMatches, clap::Error> {
    let command = with_infer_long_args(TriageCli::command());
    command.try_get_matches_from(std::iter::once("charliebot triage").chain(args.iter().copied()))
  }

  #[test]
  fn a_scan_takes_bounds_and_no_longer_accepts_dry_run() {
    let parsed = matches(&["scan", "--since", "2026-11-01T00:00:00Z", "--until", "2026-11-02T00:00:00Z"]).unwrap();
    let (_, scan) = parsed.subcommand().unwrap();
    assert_eq!(scan.get_one::<String>("since").map(String::as_str), Some("2026-11-01T00:00:00Z"));
    let error = matches(&["scan", "--since", "2026-11-01T00:00:00Z", "--until", "2026-11-02T00:00:00Z", "--dry-run"])
      .unwrap_err();
    assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument, "{error}");
  }

  #[test]
  fn cron_is_exclusive_with_the_window_bounds() {
    for flag in ["--since", "--until"] {
      let error = matches(&["scan", "--cron", "failure-triage", flag, "2026-11-01T00:00:00Z"]).unwrap_err();
      assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict, "{error}");
    }
  }
}
