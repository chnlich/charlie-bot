//! The `screenshots` verbs: the screenshot checks a web page change passes before it merges.
//!
//! `check` reads the changed paths on stdin, one per line, and passes at once when none alters a CharlieBot
//! web page. Otherwise the directory it names must hold two PNGs of each window size, the states before and
//! after the change, and the pass prints a `web page change: <n> path(s)` line, which is how a caller tells a
//! web page change from any other. `sizes` prints each window size, one `WIDTHxHEIGHT` per line, which is how
//! `scripts/switch-server.sh` reads the set for its live capture without a second copy of it.
//!
//! Exit codes: 0 passed, 1 a missing or mismatched screenshot, 2 a usage error.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_tasks::web_screenshots;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "screenshots", about = "Check the screenshots that a web page change carries")]
struct ScreenshotsCli {
  #[command(subcommand)]
  command: ScreenshotsCommand,
}

#[derive(Subcommand)]
enum ScreenshotsCommand {
  /// Read the changed paths on stdin; with a web page change, require two PNGs of each window size in --dir.
  Check {
    /// The evidence directory that holds the round's before and after screenshots.
    #[arg(long, value_name = "DIR")]
    dir: Option<PathBuf>,
  },
  /// Print each CSS window size, one `WIDTHxHEIGHT` per line.
  Sizes,
}

pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let cli: ScreenshotsCli = cli_args::parse("charliebot screenshots", args);
  match cli.command {
    ScreenshotsCommand::Check { dir } => check(dir.as_deref()),
    ScreenshotsCommand::Sizes => {
      for (width, height) in web_screenshots::WINDOW_SIZES {
        println!("{width}x{height}");
      }
      Ok(())
    }
  }
}

/// The `check` verb: no web page change passes, and a web page change needs the directory's screenshots.
fn check(dir: Option<&Path>) -> Result<(), CliExit> {
  let mut input = String::new();
  std::io::stdin()
    .read_to_string(&mut input)
    .map_err(|error| CliExit::error(format!("cannot read the changed paths from stdin: {error}")))?;
  let changed: Vec<String> = input.lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_string).collect();
  if !web_screenshots::changes_web_page(&changed) {
    return Ok(());
  }
  let dir = dir.ok_or_else(|| {
    CliExit::error(
      "the changed paths alter a web page, so this check needs --dir naming the evidence directory that holds two \
       screenshots of each window size",
    )
  })?;
  web_screenshots::check_screenshot_dir(dir, web_screenshots::ROUND_SHOTS_PER_SIZE)
    .map_err(|error| CliExit::error(format!("{error:#}")))?;
  let pages = changed.iter().filter(|path| web_screenshots::is_web_page_path(path)).count();
  println!("web page change: {pages} path(s)");
  Ok(())
}
