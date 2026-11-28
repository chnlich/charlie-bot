//! The `charliebot` CLI dispatcher.
//!
//! The vocabulary is every subcommand with a caller. No argument, `-h` or `--help` prints the
//! usage block and exits 0; an unknown name exits 2; a vocabulary name with no dispatch arm prints
//! the not-ported JSON error and exits 1.

mod commands;
mod host;
mod launcher;
mod session_cgroup;

use std::ffi::OsString;
use std::path::Path;
use std::process::exit;

use charliebot_agents::cli_host::LazyHost;
use charliebot_cli_common::json_out;

/// The subcommand vocabulary, sorted: every registered command with a
/// caller. A caller-less name is an unknown name here.
const COMMANDS: [&str; 40] = [
  "artifact",
  "backend",
  "backup",
  "compaction-context",
  "config",
  "delegate",
  "discord",
  "disk-sweep",
  "feishu",
  "files",
  "foreground-call",
  "foreground-cap",
  "foreground-guard",
  "gc-trash",
  "github",
  "google",
  "improve",
  "kill-check",
  "kill-guard",
  "land",
  "linear",
  "memory",
  "plan",
  "preflight-charlie-code",
  "publish",
  "read-budget",
  "release-helper",
  "router",
  "scan-guard",
  "screenshots",
  "session",
  "setup",
  "slack",
  charliebot_switch_helper::SUBCOMMAND,
  "taste",
  "triage",
  "usage",
  "voice",
  "wait-guard",
  "web",
];

/// Commands that backend hooks and the transient guard call, rather than users.
const INTERNAL_COMMANDS: [&str; 3] = ["foreground-call", "foreground-cap", "foreground-guard"];

fn public_commands() -> impl Iterator<Item = &'static str> {
  COMMANDS.iter().copied().filter(|command| !INTERNAL_COMMANDS.contains(command))
}

fn main() {
  let mut argv = std::env::args_os();
  let program = argv.next().map(|argv0| program_name(&argv0)).unwrap_or_else(|| "charliebot".to_string());
  let args: Vec<OsString> = argv.collect();

  if let Some(code) = charliebot_packages::helper_entry::run(&args) {
    exit(code.into());
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
    let available = public_commands().collect::<Vec<_>>().join(", ");
    eprintln!(
      "{program}: unknown subcommand {}. Available subcommands: {}",
      charliebot_core::text_repr::str_repr(&subcommand),
      available
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
    "backend" => commands::backend::main(args, host),
    "backup" => commands::backup::main(args, host),
    "compaction-context" => commands::compaction_context::main(args),
    "config" => commands::config::main(args),
    "delegate" => commands::delegate::main(args),
    "discord" => charliebot_discord::cli::main(args),
    "disk-sweep" => commands::disk_sweep::main(args, host),
    "feishu" => charliebot_feishu::cli::main(args),
    "files" => commands::files::main(args),
    "foreground-call" => {
      commands::foreground::call_main(args);
      Ok(())
    }
    "foreground-cap" => {
      commands::foreground::cap_main();
      Ok(())
    }
    "foreground-guard" => {
      commands::foreground::guard_main(args);
      Ok(())
    }
    "gc-trash" => commands::gc_trash::main(args, host),
    "github" => charliebot_github::cli::main(args),
    "google" => charliebot_google::cli::main(args),
    "improve" => charliebot_improve::cli::improve_main(args),
    "kill-check" => commands::kill_check::main(args),
    "kill-guard" => commands::kill_guard::main(args),
    "land" => commands::land::main(args),
    "linear" => commands::linear::main(args),
    "memory" => charliebot_memory::cli::main(args),
    "plan" => charliebot_artifacts::cli::plan::main(args, host),
    "preflight-charlie-code" => commands::preflight_charlie_code::main(args),
    "publish" => charliebot_artifacts::cli::publish::main(args, host),
    "read-budget" => commands::read_budget::main(args),
    "release-helper" => commands::release_helper::main(args),
    "router" => {
      commands::router::main(args);
      Ok(())
    }
    "scan-guard" => commands::scan_guard::main(args),
    "screenshots" => commands::screenshots::main(args),
    "session" => commands::session::main(args),
    "setup" => commands::setup::main(args),
    "slack" => charliebot_slack::cli::main(args),
    charliebot_switch_helper::SUBCOMMAND => charliebot_switch_helper::main(args),
    "taste" => {
      // The plan registry is composed only when `candidates` reads a session's notes.
      let read_plans = |session_dir: &Path| -> anyhow::Result<Option<Vec<serde_json::Value>>> {
        host.get()?.registry.plan_registry_reader()?(session_dir)
      };
      charliebot_taste::cli::main(args, read_plans)
    }
    "triage" => commands::triage::main(args, host),
    "usage" => commands::usage::main(args),
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
  for command in public_commands() {
    println!("  {command}");
  }
}

#[cfg(test)]
mod tests {
  use std::ffi::OsString;
  use std::sync::Mutex;

  use super::*;

  #[test]
  fn foreground_commands_are_registered_but_not_advertised_as_user_commands() {
    for command in INTERNAL_COMMANDS {
      assert!(COMMANDS.contains(&command));
      assert!(!public_commands().any(|public| public == command));
    }
  }

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
