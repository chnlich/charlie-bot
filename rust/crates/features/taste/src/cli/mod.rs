//! The `taste` CLI verbs: add, mark, query, index and candidates.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::{config, home};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::candidates::{self, CandidateSource};
use crate::store::{self, MarkStatus, Row};

#[derive(Parser)]
#[command(name = "taste", about = "The user's choice precedents: add, mark, query, index and candidates")]
struct TasteCli {
  #[command(subcommand)]
  command: TasteCommand,
}

#[derive(Subcommand)]
enum TasteCommand {
  /// Validate a row file, assign its id, and append it.
  Add(AddArgs),
  /// Cover a row with a memory entry or retract it.
  Mark(MarkArgs),
  /// Print a domain's active rows.
  Query(QueryArgs),
  /// Print the segment that every manager and worker run receives.
  Index,
  /// Print the choice candidates that the extractor reads.
  Candidates(CandidatesArgs),
}

#[derive(Args)]
struct AddArgs {
  /// The row's JSON file.
  #[arg(long)]
  file: PathBuf,
}

#[derive(Args)]
struct MarkArgs {
  /// The row id to mark.
  id: String,
  /// The memory entry that now covers the row.
  #[arg(long)]
  covered_by: Option<String>,
  /// Withdraw a row whose source the extractor read wrong.
  #[arg(long)]
  retracted: bool,
  /// Why the row is withdrawn; required with --retracted.
  #[arg(long)]
  reason: Option<String>,
}

#[derive(Args)]
struct QueryArgs {
  /// The domain to print, such as work/code-review.
  #[arg(long)]
  domain: String,
  /// Print the rows that were active at this RFC 3339 moment instead of today's active rows.
  #[arg(long)]
  as_of: Option<String>,
  /// Print each row as one JSON object per line.
  #[arg(long)]
  json: bool,
}

#[derive(Args)]
struct CandidatesArgs {
  /// Only candidates at or after this RFC 3339 moment.
  #[arg(long)]
  since: String,
  /// Comma-separated sources: comments, notes, downvote-replies, chat.
  #[arg(long)]
  sources: Option<String>,
}

/// Parse the taste arguments and run one verb. `read_plans` reads one session's plan registry; only `candidates`
/// calls it.
pub fn main<F>(args: &[OsString], read_plans: F) -> Result<(), CliExit>
where
  F: Fn(&Path) -> anyhow::Result<Option<Vec<Value>>>,
{
  let command = with_infer_long_args(TasteCli::command());
  let argv = std::iter::once(OsString::from("charliebot taste")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = TasteCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    TasteCommand::Add(args) => add(&args),
    TasteCommand::Mark(args) => mark(&args),
    TasteCommand::Query(args) => query(&args),
    TasteCommand::Index => index(),
    TasteCommand::Candidates(args) => candidates(&args, &read_plans),
  }
}

fn store_dir() -> Result<PathBuf, CliExit> {
  let home_dir = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  Ok(home_dir.join("taste"))
}

fn add(args: &AddArgs) -> Result<(), CliExit> {
  let text =
    std::fs::read_to_string(&args.file).map_err(|error| CliExit::error(format!("{}: {error}", args.file.display())))?;
  let value: Value = serde_json::from_str(&text)
    .map_err(|error| CliExit::error(format!("{}: invalid JSON: {error}", args.file.display())))?;
  let id = store::add_row(&store_dir()?, &value).map_err(|error| CliExit::error(format!("{error:#}")))?;
  println!("{id}");
  Ok(())
}

fn mark(args: &MarkArgs) -> Result<(), CliExit> {
  let status = match (&args.covered_by, args.retracted, &args.reason) {
    (Some(entry), false, None) if !entry.trim().is_empty() => MarkStatus::Covered(entry.clone()),
    (Some(_), false, Some(_)) => return Err(CliExit::error("--reason belongs to --retracted, not --covered-by")),
    (Some(_), true, _) => return Err(CliExit::error("give exactly one of --covered-by or --retracted")),
    (None, true, Some(reason)) if !reason.trim().is_empty() => MarkStatus::Retracted(reason.clone()),
    (None, true, _) => return Err(CliExit::error("--retracted needs --reason")),
    (None, false, _) => return Err(CliExit::error("give exactly one of --covered-by or --retracted")),
    (Some(_), false, None) => return Err(CliExit::error("--covered-by needs a non-empty entry")),
  };
  store::add_mark(&store_dir()?, &args.id, status).map_err(|error| CliExit::error(format!("{error:#}")))
}

fn query(args: &QueryArgs) -> Result<(), CliExit> {
  let store = store::load_store(&store_dir()?).map_err(|error| CliExit::error(format!("{error:#}")))?;
  let rows = match &args.as_of {
    Some(moment) => {
      let moment = parse_moment(moment, "--as-of")?;
      store.rows_as_of(&args.domain, moment)
    }
    None => store.query_domain(&args.domain).map_err(CliExit::error)?,
  };
  for row in rows {
    if args.json {
      println!("{}", serde_json::to_string(&store::row_value(row)).map_err(|error| CliExit::error(error.to_string()))?);
    } else {
      println!("{}", compact_row(row));
    }
  }
  Ok(())
}

fn index() -> Result<(), CliExit> {
  let text = store::load_store(&store_dir()?).map_err(|error| CliExit::error(format!("{error:#}")))?.index_text();
  // Exactly the injected segment: no trailing newline the contribution does not carry.
  print!("{text}");
  Ok(())
}

fn candidates<F>(args: &CandidatesArgs, read_plans: &F) -> Result<(), CliExit>
where
  F: Fn(&Path) -> anyhow::Result<Option<Vec<Value>>>,
{
  let since = parse_moment(&args.since, "--since")?;
  let sources = match &args.sources {
    None => CandidateSource::all(),
    Some(list) => {
      let mut sources = BTreeSet::new();
      for value in list.split(',') {
        let source = CandidateSource::parse(value)
          .ok_or_else(|| CliExit::error(format!("--sources: unknown source {:?}", value.trim())))?;
        sources.insert(source);
      }
      sources
    }
  };
  let home_dir = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let sessions_root = config::sessions_dir_in(&home_dir);
  let candidates = candidates::collect(&sessions_root, since, &sources, read_plans)
    .map_err(|error| CliExit::error(format!("{error:#}")))?;
  for candidate in candidates {
    println!("{}", serde_json::to_string(&candidate.to_value()).map_err(|error| CliExit::error(error.to_string()))?);
  }
  Ok(())
}

fn parse_moment(value: &str, flag: &str) -> Result<OffsetDateTime, CliExit> {
  OffsetDateTime::parse(value, &Rfc3339)
    .map_err(|error| CliExit::error(format!("{flag}: not an RFC 3339 timestamp: {error}")))
}

/// One row's compact line: id, date, situation, options, chosen, reason and link.
fn compact_row(row: &Row) -> String {
  let options = if row.options.is_empty() { "(none)".to_string() } else { row.options.join(" | ") };
  format!(
    "{} · {} · situation: {} · options: {} · chosen: {} · reason: {} · link: {}",
    row.id, row.decided_at, row.situation, options, row.chosen, row.reason, row.source.url
  )
}
