//! The `mock-home` program: writes the synthetic session home of the improve rounds, reads a served copy back against
//! its manifest, and prints the data version.
//!
//! Exit codes: 0 on success; 1 when `check` finds a mismatch, or on any error; 2 for a usage error.

use std::path::PathBuf;
use std::process::ExitCode;

use charliebot_mock_home::check::check;
use charliebot_mock_home::generate::generate;
use charliebot_mock_home::{DATA_VERSION, DEFAULT_SEED, PRODUCTION_SESSIONS};
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
  },
  /// Read a served session home back against the manifest in OUTPUT.
  Check {
    output: PathBuf,
    /// The server's base URL, such as the trial's CHARLIEBOT_SERVE_URL.
    base_url: String,
  },
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

fn main() -> ExitCode {
  let result = match Cli::parse().command {
    Action::Generate { output, sessions, seed } => generate(&output, sessions, seed).map(|()| true),
    Action::Check { output, base_url } => check(&output, &base_url),
    Action::Version => {
      println!("{DATA_VERSION}");
      Ok(true)
    }
  };
  match result {
    Ok(true) => ExitCode::SUCCESS,
    Ok(false) => ExitCode::FAILURE,
    Err(error) => {
      eprintln!("mock-home: {error:#}");
      ExitCode::FAILURE
    }
  }
}
