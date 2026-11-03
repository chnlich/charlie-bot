//! The `presend-check` command: `presend-check replay [--parallel N]`.
//!
//! Reads one JSON object per line on stdin, `{"child": <session id>, "event": <event id>}`, and runs the pre-send
//! check on each message as of its event, with the server's check function and the configured light backends. Prints
//! one JSON object per input line, in input order: the backend, the numbered flags and the check's wall time, or the
//! cause when that check failed. A failed check is a line, not a failed command.

use std::ffi::OsString;
use std::io::BufRead;
use std::sync::Arc;

use charliebot_agents::cli_host::LazyHost;
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_tasks::presend_check::{CheckHost, ReplayItem, replay_checks, replay_row};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "presend-check", subcommand_required = true, arg_required_else_help = true)]
struct PresendCli {
  #[command(subcommand)]
  command: PresendCommand,
}

#[derive(Subcommand)]
enum PresendCommand {
  /// Run the pre-send check on the messages listed on stdin, each as of its event.
  Replay(ReplayArgs),
}

#[derive(Args)]
struct ReplayArgs {
  /// Checks in flight at once.
  #[arg(long, default_value_t = 1)]
  parallel: usize,
}

/// Run the `presend-check` subcommand.
pub fn main(args: &[OsString], lazy_host: &LazyHost) -> Result<(), CliExit> {
  let command = with_infer_long_args(PresendCli::command());
  let argv = std::iter::once(OsString::from("charliebot presend-check")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = PresendCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    PresendCommand::Replay(args) => replay_command(args, lazy_host),
  }
}

fn replay_command(args: ReplayArgs, lazy_host: &LazyHost) -> Result<(), CliExit> {
  if args.parallel == 0 {
    return Err(CliExit::usage("--parallel must be at least 1"));
  }
  let items = read_items(std::io::stdin().lock())?;
  let host = lazy_host.get().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let check_host = CheckHost {
    cfg: &host.cfg,
    pool: Arc::clone(&host.pool),
    registry: Arc::clone(&host.registry),
    base_env: &host.base_env,
    repo_root: &host.repo_root,
  };
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| CliExit::error(error.to_string()))?;
  let outcomes = runtime.block_on(replay_checks(&check_host, &items, args.parallel));
  for (item, outcome) in items.iter().zip(&outcomes) {
    println!("{}", replay_row(item, outcome));
  }
  Ok(())
}

/// One item per non-blank stdin line. A line that is not an item object is a usage error, named by its line number.
fn read_items(input: impl BufRead) -> Result<Vec<ReplayItem>, CliExit> {
  let mut items = Vec::new();
  for (index, line) in input.lines().enumerate() {
    let line = line.map_err(|error| CliExit::error(error.to_string()))?;
    if line.trim().is_empty() {
      continue;
    }
    let item = serde_json::from_str::<ReplayItem>(&line)
      .map_err(|error| CliExit::usage(format!("stdin line {}: {error}", index + 1)))?;
    items.push(item);
  }
  Ok(items)
}
