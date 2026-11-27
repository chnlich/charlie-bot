//! The foreground command cap's three subcommands: `foreground-cap`, the `PreToolUse` hook that rewrites a shell command;
//! `foreground-call`, the call entry that runs the rewritten command under the session cgroup; and `foreground-guard`,
//! the guard unit that ends a call at its deadline. The mechanism lives in `charliebot_agents::foreground`.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::exit;

use charliebot_agents::foreground::call::{CallSpec, run_call};
use charliebot_agents::foreground::guard::{CgroupGuardHost, run_guard};
use charliebot_agents::foreground::hook::answer;
use charliebot_agents::foreground::{FOREGROUND_DEADLINE_SECS, FOREGROUND_STOP_SECS, SESSION_CGROUP_ENV, decode_hex};
use clap::Parser;

/// The exit status that tells a `PreToolUse` hook to block the call, with its stderr as the reason.
const HOOK_BLOCK_EXIT: i32 = 2;

#[derive(Parser)]
#[command(name = "foreground-call", about = "Run one agent shell command under the foreground deadline")]
struct CallCli {
  /// The shell command, hex-encoded as the `foreground-cap` hook writes it
  #[arg(long)]
  cmd_hex: String,
  /// Seconds until the command is ended; the code constant unless a test injects a shorter one
  #[arg(long, hide = true, default_value_t = FOREGROUND_DEADLINE_SECS)]
  deadline_secs: u64,
  /// Seconds from SIGTERM to the kill; the code constant unless a test injects a shorter one
  #[arg(long, hide = true, default_value_t = FOREGROUND_STOP_SECS)]
  stop_secs: u64,
}

#[derive(Parser)]
#[command(name = "foreground-guard", about = "Hold one foreground call's deadline")]
struct GuardCli {
  /// The call cgroup directory
  #[arg(long)]
  cgroup: PathBuf,
  /// Seconds until SIGTERM
  #[arg(long)]
  deadline_secs: u64,
  /// Seconds from SIGTERM to `cgroup.kill`
  #[arg(long)]
  stop_secs: u64,
}

/// `foreground-cap`: read one hook event on stdin and write its answer. A hook error blocks the call with exit 2, so a
/// failed rewrite never runs the command without its deadline.
pub fn cap_main() {
  let mut input = String::new();
  if let Err(error) = std::io::stdin().read_to_string(&mut input) {
    block(&format!("foreground-cap: could not read hook input: {error}"));
  }
  let event: serde_json::Value = match serde_json::from_str(&input) {
    Ok(event) => event,
    Err(error) => block(&format!("foreground-cap: invalid hook JSON: {error}")),
  };
  let session_cgroup_set = std::env::var_os(SESSION_CGROUP_ENV).is_some_and(|value| !value.is_empty());
  match answer(&event, session_cgroup_set) {
    Ok(Some(answer)) => {
      let mut stdout = std::io::stdout().lock();
      let written = serde_json::to_writer(&mut stdout, &answer)
        .map_err(std::io::Error::other)
        .and_then(|()| stdout.write_all(b"\n"));
      if let Err(error) = written {
        block(&format!("foreground-cap: could not write the hook answer: {error}"));
      }
    }
    Ok(None) => {}
    Err(error) => block(&format!("foreground-cap: {error}")),
  }
}

/// `foreground-call`: run the command under the session cgroup and exit with the status the agent sees.
pub fn call_main(args: &[OsString]) {
  let cli = parse::<CallCli>("foreground-call", args);
  let command = match decode_hex(&cli.cmd_hex) {
    Ok(command) => command,
    Err(error) => fail(&format!("foreground-call: {error}")),
  };
  let session_cgroup = match std::env::var_os(SESSION_CGROUP_ENV).filter(|value| !value.is_empty()) {
    Some(path) => PathBuf::from(path),
    None => {
      fail(&format!("foreground-call: {SESSION_CGROUP_ENV} is unset, so the command cannot run under a deadline"))
    }
  };
  let guard_exe = match std::env::current_exe() {
    Ok(path) => path,
    Err(error) => fail(&format!("foreground-call: cannot locate the charliebot executable: {error}")),
  };
  let spec =
    CallSpec { session_cgroup, command, deadline_secs: cli.deadline_secs, stop_secs: cli.stop_secs, guard_exe };
  match run_call(&spec) {
    Ok(code) => exit(code),
    Err(error) => fail(&format!("foreground-call: {error}")),
  }
}

/// `foreground-guard`: hold one call's deadline. Run by the guard unit that `foreground-call` starts.
pub fn guard_main(args: &[OsString]) {
  let cli = parse::<GuardCli>("foreground-guard", args);
  let mut host = CgroupGuardHost::new(&cli.cgroup);
  let deadline = std::time::Duration::from_secs(cli.deadline_secs);
  let stop = std::time::Duration::from_secs(cli.stop_secs);
  if let Err(error) = run_guard(&mut host, deadline, stop) {
    fail(&format!("foreground-guard: {error}"));
  }
}

fn parse<T: Parser>(name: &str, args: &[OsString]) -> T {
  let argv = std::iter::once(OsString::from(format!("charliebot {name}"))).chain(args.iter().cloned());
  match T::try_parse_from(argv) {
    Ok(cli) => cli,
    Err(error) => error.exit(),
  }
}

fn fail(message: &str) -> ! {
  eprintln!("{message}");
  exit(1)
}

fn block(message: &str) -> ! {
  eprintln!("{message}");
  exit(HOOK_BLOCK_EXIT)
}
