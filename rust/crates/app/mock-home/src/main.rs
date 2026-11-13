//! The `mock-home` program writes the synthetic session home of the improve rounds. It reads a served copy back
//! against its manifest, resets a task-tree home after a server start, prints the data version, and runs the steps of
//! the startup lab's recovery check (`recovery_check`).
//!
//! Exit codes: 0 on success; 1 when `check` finds a mismatch, or on any error; 2 for a usage error.

use std::path::PathBuf;
use std::process::ExitCode;

use charliebot_mock_home::check::{CheckOutcome, check};
use charliebot_mock_home::generate::generate;
use charliebot_mock_home::recovery_check::{digest, seed};
use charliebot_mock_home::task_tree::{append_growth, reset};
use charliebot_mock_home::{DATA_VERSION, DEFAULT_SEED, Language, PRODUCTION_SESSIONS, Shape};
use clap::{Parser, Subcommand};

/// Generate a synthetic session home, or read one back from a running server.
#[derive(Parser)]
#[command(name = "mock-home")]
struct Cli {
  #[command(subcommand)]
  command: Action,
}

#[derive(Subcommand)]
enum Action {
  /// Write a synthetic session home into OUTPUT, which must be absent or empty.
  Generate {
    output: PathBuf,
    /// The number of sessions.
    #[arg(long, default_value_t = PRODUCTION_SESSIONS, value_parser = positive_count)]
    sessions: usize,
    /// The seed. One seed gives the same bytes on every run.
    #[arg(long, default_value_t = DEFAULT_SEED)]
    seed: u64,
    /// The kind of home. `task-tree` adds the production startup's task tree for startup measurements; `view` gives each
    /// session a task spec sized to a live view's rows.
    #[arg(long, value_enum, default_value_t = Shape::Default)]
    shape: Shape,
    /// The language of the seeded prose, titles and table labels. `mixed` takes Chinese or English per sentence; the
    /// README screenshots use `english` so every rendered line reads in one language.
    #[arg(long, value_enum, default_value_t = Language::Mixed)]
    language: Language,
  },
  /// Read a served session home back against the manifest in OUTPUT.
  Check {
    output: PathBuf,
    /// The server's base URL, such as the trial's CHARLIEBOT_SERVE_URL.
    base_url: String,
  },
  /// Return the task-tree home in OUTPUT to its generated state: the boot marker, the file times and the worktrees.
  Reset {
    output: PathBuf,
    /// Accept a log longer than its generated size, which `append-growth` leaves.
    #[arg(long)]
    allow_growth: bool,
  },
  /// Append BYTES of chat-content events to the full-pass logs of 1 MB or more, in proportion to their size.
  AppendGrowth { output: PathBuf, bytes: u64 },
  /// Print the data version. The home lives in `~/improve/mock-home/<version>/`.
  Version,
  /// Add the recovery check's sessions to SESSIONS, a copy of a task-tree home, and list their live runs in LIVE_RUNS.
  RecoverySeed { sessions: PathBuf, live_runs: PathBuf },
  /// Give a live run of the recovery check in SESSIONS the process PID: its pid, start time and a fresh started_at.
  RecoveryAttach { sessions: PathBuf, session_id: String, run_id: String, pid: u32 },
  /// Print how many live runs in LIVE_RUNS of an open task have no ended_at yet in SESSIONS, a trial's sessions.
  RecoveryUnended { sessions: PathBuf, live_runs: PathBuf },
  /// Print the digest of the files under the trial home HOME that its server wrote after STAMP, against SEED.
  RecoveryFiles {
    seed: PathBuf,
    home: PathBuf,
    stamp: PathBuf,
    /// The lab directory, whose trial, run and build paths the digest replaces.
    #[arg(long)]
    lab: PathBuf,
  },
  /// Print the counted events of the trial server log LOG, with the ids of SEED kept and fresh ids replaced.
  RecoveryItems {
    log: PathBuf,
    seed: PathBuf,
    /// The lab directory, whose trial, run and build paths the digest replaces.
    #[arg(long)]
    lab: PathBuf,
  },
}

/// A session count of at least one.
fn positive_count(text: &str) -> Result<usize, String> {
  match text.parse::<usize>() {
    Ok(0) => Err(format!("must be at least 1: {text}")),
    Ok(count) => Ok(count),
    Err(error) => Err(error.to_string()),
  }
}

/// The exit code of a completed `check`: success when every check held, failure on a mismatch.
fn check_exit_code(outcome: CheckOutcome) -> ExitCode {
  match outcome {
    CheckOutcome::Matches => ExitCode::SUCCESS,
    CheckOutcome::Mismatches => ExitCode::FAILURE,
  }
}

/// Prints a digest to stdout.
fn print(digest: anyhow::Result<String>) -> anyhow::Result<ExitCode> {
  print!("{}", digest?);
  Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
  let result = match Cli::parse().command {
    Action::Generate { output, sessions, seed, shape, language } => {
      generate(&output, sessions, seed, shape, language).map(|()| ExitCode::SUCCESS)
    }
    Action::Check { output, base_url } => check(&output, &base_url).map(check_exit_code),
    Action::Reset { output, allow_growth } => reset(&output, allow_growth).map(|()| ExitCode::SUCCESS),
    Action::AppendGrowth { output, bytes } => append_growth(&output, bytes).map(|()| ExitCode::SUCCESS),
    Action::Version => {
      println!("{DATA_VERSION}");
      Ok(ExitCode::SUCCESS)
    }
    Action::RecoverySeed { sessions, live_runs } => seed::seed(&sessions, &live_runs).map(|()| ExitCode::SUCCESS),
    Action::RecoveryAttach { sessions, session_id, run_id, pid } => {
      seed::attach_live_run(&sessions, &session_id, &run_id, pid).map(|()| ExitCode::SUCCESS)
    }
    Action::RecoveryUnended { sessions, live_runs } => seed::unended_live_runs(&sessions, &live_runs).map(|count| {
      println!("{count}");
      ExitCode::SUCCESS
    }),
    Action::RecoveryFiles { seed, home, stamp, lab } => print(digest::files(&seed, &home, &stamp, &lab)),
    Action::RecoveryItems { log, seed, lab } => print(digest::items(&log, &seed, &lab)),
  };
  match result {
    Ok(code) => code,
    Err(error) => {
      eprintln!("mock-home: {error:#}");
      ExitCode::FAILURE
    }
  }
}
