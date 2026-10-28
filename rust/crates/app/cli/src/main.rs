//! The `charliebot` CLI dispatcher (`src/app/main.py`).
//!
//! The vocabulary is the 24 subcommands with callers. No argument, `-h` or `--help` prints the
//! usage block and exits 0; an unknown name exits 2; a name this port has not reached yet prints
//! the not-ported JSON error and exits 1 — it never calls the Python CLI.

mod commands;
mod host;
mod launcher;

use std::ffi::OsString;
use std::path::Path;
use std::process::exit;

use charliebot_agents::cli_host::LazyHost;
use charliebot_cli_common::json_out;

/// The subcommand vocabulary, sorted — `_RUNTIME_COMMANDS` plus every registered command with a
/// caller; the caller-less names the Python CLI lists are unknown names here.
const COMMANDS: [&str; 24] = [
  "artifact",
  "compaction-context",
  "config",
  "delegate",
  "discord",
  "feishu",
  "files",
  "gc-trash",
  "google",
  "improve",
  "improve-stop",
  "linear",
  "memory",
  "plan",
  "publish",
  "release-helper",
  "router",
  "schedule-trigger",
  "session",
  "slack",
  "switch-helper",
  "voice",
  "wait-guard",
  "web",
];

fn main() {
  let mut argv = std::env::args_os();
  let program = argv.next().map(|argv0| program_name(&argv0)).unwrap_or_else(|| "charliebot".to_string());
  let args: Vec<OsString> = argv.collect();

  if args.first().is_some_and(|arg| arg == charliebot_opencode::relay::RELAY_ARG) {
    match charliebot_opencode::relay::run_args(&args[1..]) {
      Ok(()) => exit(0),
      Err(error) => {
        eprintln!("OpenCode relay: {error:#}");
        exit(1);
      }
    }
  }

  if args.first().is_some_and(|arg| arg == charliebot_agents::transport::final_stdout::SUPERVISOR_ARG) {
    match charliebot_agents::transport::final_stdout::supervise(&args[1..]) {
      Ok(()) => exit(0),
      Err(error) => {
        eprintln!("final-stdout supervisor: {error:#}");
        exit(1);
      }
    }
  }

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

  let host = LazyHost::new(host::compose);
  let result = run_subcommand(&subcommand, &args[1..], &host);
  if let Err(failure) = result {
    eprint!("{}", json_out::render(&failure.body, None));
    exit(failure.code);
  }
}

fn run_subcommand(
  subcommand: &str,
  args: &[OsString],
  host: &LazyHost,
) -> Result<(), charliebot_cli_common::exit::CliExit> {
  match subcommand {
    "artifact" => charliebot_artifacts::cli::artifact::main(args, host),
    "compaction-context" => commands::compaction_context::main(args),
    "config" => commands::config::main(args),
    "delegate" => commands::delegate::main(args),
    "discord" => charliebot_discord::cli::main(args),
    "feishu" => commands::feishu::main(args),
    "files" => commands::files::main(args),
    "gc-trash" => commands::gc_trash::main(args, host),
    "google" => commands::google::main(args),
    "improve" => charliebot_improve::cli::improve_main(args),
    "improve-stop" => charliebot_improve::cli::stop_main(args),
    "linear" => commands::linear::main(args),
    "memory" => charliebot_memory::cli::main(args),
    "plan" => charliebot_artifacts::cli::plan::main(args, host),
    "publish" => charliebot_artifacts::cli::publish::main(args, host),
    "release-helper" => commands::release_helper::main(args),
    "router" => {
      commands::router::main(args);
      Ok(())
    }
    "schedule-trigger" => commands::schedule_trigger::main(args),
    "session" => commands::session::main(args),
    "slack" => charliebot_slack::cli::main(args),
    "switch-helper" => charliebot_switch_helper::main(args),
    "voice" => commands::voice::main(args, host),
    "wait-guard" => commands::wait_guard::main(args),
    "web" => charliebot_web_reply::cli::main(args, host),
    name => Err(charliebot_cli_common::exit::CliExit::error(format!("{name} is not ported to the Rust CLI yet"))),
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

#[cfg(test)]
mod tests {
  use std::ffi::OsString;
  use std::sync::Mutex;

  use super::*;

  static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

  struct EnvironmentReset {
    home: Option<OsString>,
    repo_root: Option<OsString>,
  }

  impl EnvironmentReset {
    fn set(home: &Path, repo_root: &Path) -> EnvironmentReset {
      let previous = EnvironmentReset {
        home: std::env::var_os(charliebot_core::home::CHARLIEBOT_HOME_ENV),
        repo_root: std::env::var_os(charliebot_core::repo_root::REPO_ROOT_ENV),
      };
      unsafe {
        std::env::set_var(charliebot_core::home::CHARLIEBOT_HOME_ENV, home);
        std::env::set_var(charliebot_core::repo_root::REPO_ROOT_ENV, repo_root);
      }
      previous
    }
  }

  impl Drop for EnvironmentReset {
    fn drop(&mut self) {
      unsafe {
        match &self.home {
          Some(value) => std::env::set_var(charliebot_core::home::CHARLIEBOT_HOME_ENV, value),
          None => std::env::remove_var(charliebot_core::home::CHARLIEBOT_HOME_ENV),
        }
        match &self.repo_root {
          Some(value) => std::env::set_var(charliebot_core::repo_root::REPO_ROOT_ENV, value),
          None => std::env::remove_var(charliebot_core::repo_root::REPO_ROOT_ENV),
        }
      }
    }
  }

  fn unexpected_host_composition() -> anyhow::Result<charliebot_agents::cli_host::CliHost> {
    panic!("compaction-context does not need the CLI host")
  }

  #[test]
  fn a_no_backend_verb_does_not_compose_the_cli_host() {
    let _lock = ENVIRONMENT_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let repo_root = tempfile::tempdir().unwrap();
    let session = home.path().join("sessions").join("synthetic-session").join("data");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(session.join("chat_events.jsonl"), "").unwrap();
    let _environment = EnvironmentReset::set(home.path(), repo_root.path());
    let host = LazyHost::new(unexpected_host_composition);
    let args = ["--session", "synthetic-session", "--slot", "1"].map(OsString::from);

    run_subcommand("compaction-context", &args, &host).unwrap();
  }
}
