//! The `github` verbs: `draft` registers a pull request or comment in the operator's name.
//!
//! The command validates the target and reads the body before anything is sent, so an unparseable target registers
//! nothing. The server sends the approved card's body from the operator's own GitHub account.

use std::ffi::OsString;

use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::{draft as draft_register, files};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;

use crate::target;

#[derive(Parser)]
#[command(name = "github", about = "CharlieBot GitHub verbs")]
struct GithubCli {
  #[command(subcommand)]
  command: GithubCommand,
}

#[derive(Subcommand)]
enum GithubCommand {
  /// A pull request or comment, registered in the operator's name for approval on a session card
  Draft(DraftArgs),
}

#[derive(Args)]
struct DraftArgs {
  /// Repository as owner/repository
  #[arg(long, value_name = "OWNER/REPO")]
  repo: String,
  /// Pull-request title
  #[arg(long)]
  title: Option<String>,
  /// Pull-request head branch
  #[arg(long)]
  head: Option<String>,
  /// Pull-request base branch
  #[arg(long)]
  base: Option<String>,
  /// Pull-request or issue number for a comment
  #[arg(long, value_name = "N")]
  number: Option<u64>,
  /// File holding the body; - reads stdin
  #[arg(long)]
  file: String,
  /// Draft id this one replaces, whose card loses its button
  #[arg(long, value_name = "DRAFT_ID")]
  replaces: Option<String>,
}

/// The GitHub arguments, parsed and run as one verb. Parser errors print and exit on their own (help 0, usage error 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let cli: GithubCli = cli_args::parse("charliebot github", args);
  match cli.command {
    GithubCommand::Draft(args) => draft(&args),
  }
}

fn draft(args: &DraftArgs) -> Result<(), CliExit> {
  let (target, body) = prepare_draft(args)?;
  draft_register::register("github", target, body, args.replaces.as_deref())
}

/// The validated target and the body, with no network call.
fn prepare_draft(args: &DraftArgs) -> Result<(Value, String), CliExit> {
  let target = if let Some(number) = args.number {
    if args.title.is_some() || args.head.is_some() || args.base.is_some() {
      return Err(CliExit::usage("--number names a comment; --title, --head and --base belong to a pull request"));
    }
    target::Target::comment(&args.repo, number)
  } else {
    let (Some(title), Some(head), Some(base)) = (&args.title, &args.head, &args.base) else {
      return Err(CliExit::usage("a pull request needs --title, --head and --base, or use --number for a comment"));
    };
    target::Target::draft_pull_request(&args.repo, title, head, base)
  };
  let target = target.map(|target| target.to_value()).map_err(CliExit::usage)?;
  let body = files::read_exact_text(&args.file)?;
  Ok((target, body))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn draft_args() -> DraftArgs {
    DraftArgs {
      repo: "owner/repo".to_string(),
      title: None,
      head: None,
      base: None,
      number: None,
      file: String::new(),
      replaces: None,
    }
  }

  fn body_file(text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("body.txt"), text).unwrap();
    dir
  }

  fn with_body(mut args: DraftArgs, dir: &tempfile::TempDir) -> DraftArgs {
    args.file = dir.path().join("body.txt").to_str().unwrap().to_string();
    args
  }

  fn error_text(error: &CliExit) -> String {
    error.body.to_string()
  }

  #[test]
  fn a_pull_request_target_carries_the_title_head_and_base() {
    let dir = body_file("the body");
    let mut args = with_body(draft_args(), &dir);
    args.title = Some("A title".to_string());
    args.head = Some("feature".to_string());
    args.base = Some("main".to_string());

    let (target, body) = prepare_draft(&args).unwrap();

    assert_eq!(target, json!({"repo": "owner/repo", "title": "A title", "head": "feature", "base": "main"}));
    assert_eq!(body, "the body");
  }

  #[test]
  fn a_comment_target_carries_the_number() {
    let dir = body_file("the body");
    let mut args = with_body(draft_args(), &dir);
    args.number = Some(12);

    let (target, _) = prepare_draft(&args).unwrap();

    assert_eq!(target, json!({"repo": "owner/repo", "number": 12}));
  }

  #[test]
  fn the_two_forms_are_exclusive_and_a_pull_request_needs_three_flags() {
    let dir = body_file("the body");
    let mut args = with_body(draft_args(), &dir);
    args.number = Some(12);
    args.title = Some("A title".to_string());
    let error = prepare_draft(&args).unwrap_err();
    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("--number names a comment"));

    let mut args = with_body(draft_args(), &dir);
    args.title = Some("A title".to_string());
    args.head = Some("feature".to_string());
    let error = prepare_draft(&args).unwrap_err();
    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("needs --title, --head and --base"));
  }

  #[test]
  fn an_unparseable_target_exits_two_before_registering() {
    let dir = body_file("the body");

    let mut args = with_body(draft_args(), &dir);
    args.repo = "not-a-repo".to_string();
    args.number = Some(1);
    let error = prepare_draft(&args).unwrap_err();
    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("target.repo"));

    let mut args = with_body(draft_args(), &dir);
    args.number = Some(0);
    let error = prepare_draft(&args).unwrap_err();
    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("target.number"));
  }

  #[test]
  fn a_missing_body_exits_two_before_registering() {
    let mut args = draft_args();
    args.number = Some(1);
    args.file = "/no/such/body".to_string();

    let error = prepare_draft(&args).unwrap_err();

    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("--file not found"));
  }
}
