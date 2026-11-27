//! The `PreToolUse` hook that refuses a Bash command which ends processes by name or pattern, for Claude Code
//! and codex.
//!
//! A name or pattern match reaches every process of the same user, so it can end other sessions' jobs and watch
//! triggers. The guard refuses `killall`, which has no scope. It refuses `pkill` unless a scope option limits the
//! match. It also refuses an unscoped `pgrep` in a command that runs `kill`, because that `kill` receives the
//! matches. The scope options are `-P`, `-g`, `-s` and `--cgroup`. A kill by PID or by process group passes, since
//! it names its target. A command that names one of the three commands and that the reader cannot parse is refused,
//! so a parse failure never lets such a command through.

use std::ffi::OsString;
use std::io::{Read, Write};

use charliebot_cli_common::exit::CliExit;

use super::bash_hook::run_bash_guard;
use super::shell_commands::{Bodies, Invocation, Killer, invocations};

/// The command names whose presence sends a command to the reader. A command with none of them passes unread.
const KILL_NAMES: [&str; 3] = ["pkill", "killall", "pgrep"];

/// The signal names that `pkill` and `pgrep` accept as an option, with or without the `SIG` prefix. Such an option
/// names the signal, so it is not a scope.
const SIGNAL_NAMES: &[&str] = &[
  "ABRT", "ALRM", "BUS", "CHLD", "CONT", "FPE", "HUP", "ILL", "INT", "IO", "IOT", "KILL", "PIPE", "POLL", "PROF",
  "PWR", "QUIT", "RTMAX", "RTMIN", "SEGV", "STKFLT", "STOP", "SYS", "TERM", "TRAP", "TSTP", "TTIN", "TTOU", "URG",
  "USR1", "USR2", "VTALRM", "WINCH", "XCPU", "XFSZ",
];

/// The short options of `pkill` and `pgrep` that take an argument. When one ends a cluster, the next word is its
/// argument; otherwise the rest of the cluster is that argument.
const ARGUMENT_OPTIONS: &[char] = &['d', 'F', 'G', 'O', 'q', 'r', 't', 'U', 'u'];

/// The long options of `pkill` and `pgrep` that take a separate argument word when written without `=`.
const LONG_ARGUMENT_OPTIONS: &[&str] = &[
  "delimiter",
  "euid",
  "group",
  "ns",
  "nslist",
  "older",
  "pidfile",
  "queue",
  "runstates",
  "signal",
  "terminal",
  "uid",
];

enum OptionKind {
  Scope,
  TakesNextArg,
  Other,
}

/// The refusal. It leads with the working alternative, the PID the caller recorded when it started the process.
const DENY_REASON: &str = "Kill the PID you recorded instead: start the process, keep `pid=$!`, and run `kill \"$pid\"`. To match by name, scope `pkill` or `pgrep` with `-P <parent pid>`, `-g <process group>`, `-s <session id>` or `--cgroup <path>`; `killall` has no scope. An unscoped match also ends other sessions' jobs and watch triggers.";

/// Read one Claude Code hook event and emit a deny decision when its Bash command ends processes by name or pattern.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  if !args.is_empty() {
    return Err(CliExit::usage("kill-guard takes no arguments"));
  }
  run(std::io::stdin().lock(), &mut std::io::stdout().lock())
}

fn run<R: Read, W: Write>(input: R, output: &mut W) -> Result<(), CliExit> {
  run_bash_guard(input, output, |_event, command| Ok(refuses(command).then_some(DENY_REASON)))
}

/// Whether a Bash command ends processes by name or pattern without a scope that limits them to the caller's processes.
/// A command that the reader cannot read is refused when it names one of the commands.
fn refuses(command: &str) -> bool {
  if !KILL_NAMES.iter().any(|name| command.contains(name)) {
    return false;
  }
  let Ok(found) = invocations(command, Bodies::Data) else {
    return true;
  };
  let mut pgrep_unscoped = false;
  let mut kill_seen = false;
  for invocation in &found {
    match invocation {
      Invocation::Dynamic | Invocation::Call { killer: Killer::Killall, .. } => return true,
      Invocation::Call { killer: Killer::Pkill, args, .. } => {
        if !scoped(args) {
          return true;
        }
      }
      Invocation::Call { killer: Killer::Pgrep, args, .. } => pgrep_unscoped |= !scoped(args),
      Invocation::Call { killer: Killer::Kill, .. } => kill_seen = true,
    }
  }
  pgrep_unscoped && kill_seen
}

/// Whether a `pkill` or `pgrep` selection carries a scope option before `--`.
fn scoped(args: &[String]) -> bool {
  let mut expect_arg = false;
  for arg in args {
    if expect_arg {
      expect_arg = false;
      continue;
    }
    if arg == "--" {
      break;
    }
    match classify_option(arg) {
      OptionKind::Scope => return true,
      OptionKind::TakesNextArg => expect_arg = true,
      OptionKind::Other => {}
    }
  }
  false
}

fn classify_option(text: &str) -> OptionKind {
  if let Some(long) = text.strip_prefix("--") {
    let (name, has_value) = long.split_once('=').map_or((long, false), |(name, _)| (name, true));
    if matches!(name, "cgroup" | "parent" | "pgroup" | "session") {
      return OptionKind::Scope;
    }
    if !has_value && LONG_ARGUMENT_OPTIONS.contains(&name) {
      return OptionKind::TakesNextArg;
    }
    return OptionKind::Other;
  }
  let Some(cluster) = text.strip_prefix('-') else {
    return OptionKind::Other;
  };
  if cluster.is_empty() || is_signal_name(cluster) {
    return OptionKind::Other;
  }
  let mut chars = cluster.chars();
  while let Some(letter) = chars.next() {
    match letter {
      'P' | 'g' | 's' => return OptionKind::Scope,
      letter if ARGUMENT_OPTIONS.contains(&letter) => {
        return if chars.as_str().is_empty() { OptionKind::TakesNextArg } else { OptionKind::Other };
      }
      _ => {}
    }
  }
  OptionKind::Other
}

fn is_signal_name(name: &str) -> bool {
  let upper = name.to_ascii_uppercase();
  SIGNAL_NAMES.contains(&upper.strip_prefix("SIG").unwrap_or(upper.as_str()))
}

#[cfg(test)]
mod tests {
  use std::io::Cursor;

  use serde_json::{Value, json};

  use super::*;

  fn run_input(input: &str) -> (Result<(), CliExit>, Vec<u8>) {
    let mut output = Vec::new();
    let result = run(Cursor::new(input.as_bytes()), &mut output);
    (result, output)
  }

  #[test]
  fn refuses_name_based_kills_without_a_scope() {
    for command in [
      "pkill -f x",
      "pkill -x flock",
      "killall node",
      "pgrep -f x | xargs kill",
      "kill $(pgrep -f x)",
      "pkill -f 'sleep -s 60'",
      "case $x in a) pkill -f x;; esac",
      "pkill -f -- -s",
      "pkill -u -s x",
      "pkill --signal -P x",
      "pkill -STOP -f x",
      "killall -g node",
      "x=$(( 1 << 2 ))\npkill -f foo",
      "echo $(( $(pkill -f foo) + 1 ))",
      "echo $((pkill -f foo); (echo bar))",
    ] {
      assert!(refuses(command), "{command}");
    }
  }

  #[test]
  fn passes_kills_by_pid_and_scoped_matches() {
    for command in [
      "kill 12345",
      "kill -- -4321",
      "pkill -P $$ -f x",
      r#"pkill -P"$$" -f x"#,
      "pkill -g 4321 x",
      "pkill --cgroup /x y",
      r#"pkill --cgroup="/x" y"#,
      r#"pkill --cgroup="$CG" y"#,
      "pgrep -P $$ -f x | xargs kill",
      "pkill -STOP -P 5 x",
      "pkill -u me -s 7 x",
      "sleep 5 & kill $!",
    ] {
      assert!(!refuses(command), "{command}");
    }
  }

  #[test]
  fn passes_names_that_only_appear_as_text() {
    for command in [
      r#"echo "pkill -f x""#,
      "grep -rn 'killall' docs",
      r#"echo "pgrep -f x | xargs kill""#,
      "git commit -m 'stop pkill use'",
      "cat <<'EOF'\npkill -f x\nEOF",
      "command -v pkill",
      "command -V killall",
      "x=$(( 1 << 2 ))\necho pkill",
    ] {
      assert!(!refuses(command), "{command}");
    }
  }

  #[test]
  fn refuses_the_same_kills_behind_a_wrapper_a_shell_or_eval() {
    for command in [
      "timeout 5 pkill -f x",
      "sudo -u root pkill -f x",
      "bash -c 'pkill -f x'",
      r#"eval "killall node""#,
      "echo x; (pkill -f y)",
      "if pgrep -f x; then kill $p; fi",
      "x=pkill; $x -f y",
      "cat <<EOF\n$(pkill -f x)\nEOF",
      "pkill -f 'unterminated",
    ] {
      assert!(refuses(command), "{command}");
    }
  }

  #[test]
  fn a_redirection_does_not_hide_the_command() {
    assert!(refuses("2>/dev/null pkill -f x"));
    assert!(!refuses("pkill -P $$ -f x 2>/dev/null"));
  }

  #[test]
  fn hook_refuses_a_pkill_with_a_deny_decision() {
    let (result, output) = run_input(r#"{"tool_name":"Bash","tool_input":{"command":"pkill -f x"}}"#);
    result.unwrap();
    let actual: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(actual["hookSpecificOutput"]["permissionDecision"], json!("deny"));
    assert_eq!(actual["hookSpecificOutput"]["hookEventName"], json!("PreToolUse"));
  }

  #[test]
  fn hook_passes_a_scoped_match_and_other_tools() {
    let (result, output) = run_input(r#"{"tool_name":"Bash","tool_input":{"command":"pkill -P $$ -f x"}}"#);
    result.unwrap();
    assert!(output.is_empty());
    let (result, output) = run_input(r#"{"tool_name":"Read","tool_input":{}}"#);
    result.unwrap();
    assert!(output.is_empty());
  }
}
