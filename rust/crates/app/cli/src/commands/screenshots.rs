//! The `screenshots` verbs: the screenshot checks a web page change passes before it merges.
//!
//! `check` reads the changed paths on stdin, one per line, and passes at once when none alters a CharlieBot
//! web page. Otherwise the directory it names must hold two PNGs of each window size, the states before and
//! after the change, and the review record it names must show a read of each screenshot the PR body names. The
//! pass prints a `web page change: <n> path(s)` line, which is how a caller tells a web page change from any
//! other. `sizes` prints each window size, one `WIDTHxHEIGHT` per line, which is how
//! `scripts/switch-server.sh` reads the set for its live capture without a second copy of it. `resolve` asks the
//! live server whether it resolves one View line's view, which the switch checks before it captures that view.
//!
//! Exit codes: 0 passed, 1 a missing or mismatched screenshot or a view the live server does not resolve, 2 a usage
//! error.

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
  /// Read the changed paths on stdin; with a web page change, require two PNGs of each window size in --dir and a
  /// review record that shows a read of each screenshot the --pr-body names.
  Check {
    /// The evidence directory that holds the round's before and after screenshots.
    #[arg(long, value_name = "DIR")]
    dir: Option<PathBuf>,
    /// The PR body, whose screenshot file names the review record must show a read of.
    #[arg(long, value_name = "FILE")]
    pr_body: Option<PathBuf>,
    /// The review's `events.ndjson`: the raw event stream that `charliebot backend run` wrote in the review's
    /// directory.
    #[arg(long, value_name = "FILE")]
    review_record: Option<PathBuf>,
  },
  /// Print each CSS window size, one `WIDTHxHEIGHT` per line.
  Sizes,
  /// Pass when the live server resolves the View line's view: a `/?session=<id>` view names a session it knows, and
  /// any other view answers 2xx.
  Resolve {
    /// The View line's URL path, such as `/?session=<id>` or `/plans`.
    view: String,
  },
}

pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let cli: ScreenshotsCli = cli_args::parse("charliebot screenshots", args);
  match cli.command {
    ScreenshotsCommand::Check { dir, pr_body, review_record } => {
      check(dir.as_deref(), pr_body.as_deref(), review_record.as_deref())
    }
    ScreenshotsCommand::Sizes => {
      for (width, height) in web_screenshots::WINDOW_SIZES {
        println!("{width}x{height}");
      }
      Ok(())
    }
    ScreenshotsCommand::Resolve { view } => web_screenshots::check_view_resolves(&view, &web_screenshots::live_status)
      .map_err(|error| CliExit::error(format!("{error:#}"))),
  }
}

/// The `check` verb: no web page change passes, and a web page change needs the directory's screenshots.
fn check(dir: Option<&Path>, pr_body: Option<&Path>, review_record: Option<&Path>) -> Result<(), CliExit> {
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
  let pr_body = pr_body.ok_or_else(|| {
    CliExit::error("the changed paths alter a web page, so this check needs --pr-body naming the pull request body")
  })?;
  let review_record = review_record.ok_or_else(|| {
    CliExit::error(
      "the changed paths alter a web page, so this check needs --review-record naming the review's events.ndjson",
    )
  })?;
  let body = std::fs::read_to_string(pr_body)
    .map_err(|error| CliExit::error(format!("cannot read the PR body {}: {error}", pr_body.display())))?;
  web_screenshots::check_review_reads(review_record, &web_screenshots::named_screenshots(&body))
    .map_err(|error| CliExit::error(format!("{error:#}")))?;
  let pages = changed.iter().filter(|path| web_screenshots::is_web_page_path(path)).count();
  println!("web page change: {pages} path(s)");
  Ok(())
}
