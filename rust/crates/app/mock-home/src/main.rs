//! The `mock-home` program: writes the synthetic session home of the improve rounds, reads a served copy back against
//! its manifest, resets a task-tree home after a server start, and prints the data version.
//!
//! Exit codes: 0 on success; 1 when `check` finds a mismatch, or on any error; 2 for a usage error.

use std::path::PathBuf;
use std::process::ExitCode;

use charliebot_mock_home::check::{CheckOutcome, check};
use charliebot_mock_home::generate::generate;
use charliebot_mock_home::task_tree::reset;
use charliebot_mock_home::{DATA_VERSION, DEFAULT_SEED, PRODUCTION_SESSIONS, Shape};
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
    /// The kind of home. `task-tree` adds the production startup's task tree for startup measurements.
    #[arg(long, value_enum, default_value_t = Shape::Default)]
    shape: Shape,
  },
  /// Read a served session home back against the manifest in OUTPUT.
  Check {
    output: PathBuf,
    /// The server's base URL, such as the trial's CHARLIEBOT_SERVE_URL.
    base_url: String,
  },
  /// Return the task-tree home in OUTPUT to its generated state: the boot marker, the file times and the worktrees.
  Reset { output: PathBuf },
  /// Print the data version. The home lives in `~/improve/mock-home/<version>/`.
  Version,
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

fn main() -> ExitCode {
  let result = match Cli::parse().command {
    Action::Generate { output, sessions, seed, shape } => {
      generate(&output, sessions, seed, shape).map(|()| ExitCode::SUCCESS)
    }
    Action::Check { output, base_url } => check(&output, &base_url).map(check_exit_code),
    Action::Reset { output } => reset(&output).map(|()| ExitCode::SUCCESS),
    Action::Version => {
      println!("{DATA_VERSION}");
      Ok(ExitCode::SUCCESS)
    }
  };
  match result {
    Ok(code) => code,
    Err(error) => {
      eprintln!("mock-home: {error:#}");
      ExitCode::FAILURE
    }
  }
}
