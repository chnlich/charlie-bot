//! The `charliebot` CLI dispatcher (`src/app/main.py`).
//!
//! The vocabulary is the 14 subcommands with callers. No argument, `-h` or `--help` prints the
//! usage block and exits 0; an unknown name exits 2; a name this port has not reached yet prints
//! the not-ported JSON error and exits 1 — it never calls the Python CLI.

mod commands;
mod launcher;

use std::ffi::OsString;
use std::path::Path;
use std::process::exit;

use charliebot_cli_common::json_out;

/// The subcommand vocabulary, sorted — `_RUNTIME_COMMANDS` plus every registered command with a
/// caller; the six caller-less names the Python CLI lists are unknown names here.
const COMMANDS: [&str; 14] = [
  "artifact",
  "compaction-context",
  "delegate",
  "discord",
  "gc-trash",
  "improve",
  "improve-stop",
  "memory",
  "plan",
  "publish",
  "schedule-trigger",
  "session",
  "slack",
  "web",
];

fn main() {
  let mut argv = std::env::args_os();
  let program = argv.next().map(|argv0| program_name(&argv0)).unwrap_or_else(|| "charliebot".to_string());
  let args: Vec<OsString> = argv.collect();

  if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
    print_help(&program);
    exit(0);
  }

  let subcommand = args[0].to_string_lossy().into_owned();
  if !COMMANDS.contains(&subcommand.as_str()) {
    eprintln!(
      "{program}: unknown subcommand {}. Available subcommands: {}",
      charliebot_core::pyfmt::str_repr(&subcommand),
      COMMANDS.join(", ")
    );
    exit(2);
  }

  let result = match subcommand.as_str() {
    "artifact" => charliebot_artifacts::cli::artifact::main(&args[1..]),
    "compaction-context" => commands::compaction_context::main(&args[1..]),
    "delegate" => commands::delegate::main(&args[1..]),
    "gc-trash" => commands::gc_trash::main(&args[1..]),
    "memory" => charliebot_memory::cli::main(&args[1..]),
    "plan" => charliebot_artifacts::cli::plan::main(&args[1..]),
    "schedule-trigger" => commands::schedule_trigger::main(&args[1..]),
    "session" => commands::session::main(&args[1..]),
    name => Err(charliebot_cli_common::exit::CliExit::error(format!("{name} is not ported to the Rust CLI yet"))),
  };
  if let Err(failure) = result {
    eprint!("{}", json_out::render(&failure.body, None));
    exit(failure.code);
  }
}

fn program_name(argv0: &OsString) -> String {
  Path::new(argv0).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

fn print_help(program: &str) {
  println!("usage: {program} <subcommand> [args...]");
  println!();
  println!("Available subcommands:");
  for command in COMMANDS {
    println!("  {command}");
  }
}
