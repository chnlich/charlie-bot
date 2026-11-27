use std::ffi::OsString;

use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::exit::CliExit;
use clap::{Parser, Subcommand};

use super::add::{self, AddArgs};
use super::digest;
use super::lint::{self, LintArgs};
use super::proposal;
use super::query::{self, QueryArgs};

#[derive(Parser)]
#[command(name = "memory", about = "Labeled-entry memory store: query, add captures, and lint")]
struct MemoryCli {
  #[command(subcommand)]
  command: MemoryCommand,
}

#[derive(Subcommand)]
enum MemoryCommand {
  /// Print entries' full text, or their index lines with `--index`.
  ///
  /// `--topic <topic> --index` prints the index of that topic. `--index` alone prints the index of every topic.
  Query(QueryArgs),
  /// Stage a free-form capture.
  Add(AddArgs),
  /// Validate the store.
  Lint(LintArgs),
  /// Print the cross-session user-message digest for the memory curator.
  UserDigest,
}

/// The memory CLI entry point, which parses the arguments and runs one local store verb.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  if args.first().and_then(|arg| arg.to_str()) == Some("proposal") {
    return proposal::main(&args[1..]);
  }
  let cli: MemoryCli = cli_args::parse("charliebot memory", args);
  match cli.command {
    MemoryCommand::Query(args) => query::query(&args),
    MemoryCommand::Add(args) => add::add(&args),
    MemoryCommand::Lint(args) => lint::lint(&args),
    MemoryCommand::UserDigest => digest::run(),
  }
}
