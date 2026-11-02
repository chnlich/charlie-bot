//! The remote runner: `tools/remote-run/run.sh` starts every run with a connectivity check whose
//! Tailscale login link exits 86, whose other failures and its time limit exit 87, and whose stop
//! text names the `/host-auth` page; after the check, the command's exit code comes back
//! unchanged, 255 included. `tools/remote-run/remote-exec.pl` runs a payload with a short TMPDIR
//! under /tmp, passes the command's exit code through, and stops the descendants the command
//! leaves behind.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use charliebot_shell_harness::{self as harness, Env, Sandbox};

const HOST: &str = "build-host";
const PAGE_URL: &str = "https://charlie.example.test:18498/host-auth";
const SCRIPTS: &str = "tools/remote-run";

fn run_sh() -> PathBuf {
  harness::repo_root().join(format!("{SCRIPTS}/run.sh"))
}

fn remote_exec_pl() -> PathBuf {
  harness::repo_root().join(format!("{SCRIPTS}/remote-exec.pl"))
}

/// The fake ssh answers the runner's remote queries and plays the cases the connectivity check
/// must tell apart. Its behavior comes from the environment, one case per variable.
const FAKE_SSH: &str = r#"
log="${FAKE_SSH_LOG:?}"
printf '%s\n' "$*" >>"$log"
last="${*: -1}"
case "$last" in
  *tree-owners*) printf '__NO_LOCAL_ROOT_RECORD__' ;;
  exec\ *) exit "${FAKE_REMOTE_STATUS:-0}" ;;
  true)
    if [[ -n "${FAKE_LINK:-}" ]]; then
      echo "# To authenticate, visit: https://login.tailscale.com/a/l0000000000000" >&2
      exec sleep "${FAKE_LINK_HOLD:-5}"
    fi
    [[ -n "${FAKE_CHECK_FAIL:-}" ]] && exit 7
    [[ -n "${FAKE_CHECK_HANG:-}" ]] && exec sleep 30
    ;;
esac
exit 0
"#;

const FAKE_RSYNC: &str = r#"
printf '%s\n' "rsync $*" >>"${FAKE_SSH_LOG:?}"
exit "${FAKE_RSYNC_STATUS:-0}"
"#;

const FAKE_CHARLIEBOT: &str = r#"
if [[ "${FAKE_URL_UNSET:-0}" == 1 ]]; then
  echo 'error: config key server.public_url is unset' >&2
  exit 1
fi
printf 'https://charlie.example.test:18498\n'
"#;

/// A project directory for the runner to sync: no git, so the run takes the directory mode.
fn project(sandbox: &Sandbox) -> PathBuf {
  let project = sandbox.dir("project");
  std::fs::write(project.join("file.txt"), "content\n").expect("write the project file");
  project
}

fn run_env(sandbox: &Sandbox) -> Env {
  let mut env = sandbox.env();
  env.insert("REMOTE_RUN_HOST".to_string(), HOST.to_string());
  env.insert("FAKE_SSH_LOG".to_string(), sandbox.root().join("ssh.log").display().to_string());
  env
}

fn run_runner(env: &Env, project: &Path, arguments: &[&str]) -> harness::Run {
  let mut command = Command::new("bash");
  command.arg(run_sh()).args(arguments);
  command.env_clear();
  command.envs(env);
  command.current_dir(project);
  harness::run(&mut command)
}

fn stop_text_is_printed(run: &harness::Run) {
  assert!(run.stderr.contains("End this run and report the blocker"), "{}", run.describe());
  assert!(run.stderr.contains(PAGE_URL), "{}", run.describe());
}

#[test]
fn a_login_link_in_the_check_gives_86_and_the_stop_text() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("charliebot", FAKE_CHARLIEBOT);
  let project = project(&sandbox);
  let mut env = run_env(&sandbox, &project);
  env.insert("FAKE_LINK".to_string(), "1".to_string());
  let run = run_runner(&env, &project, &["echo", "remote"]);
  assert_eq!(run.code, 86, "{}", run.describe());
  stop_text_is_printed(&run);
  assert!(run.stderr.contains("Tailscale needs an SSH approval"), "{}", run.describe());
}

#[test]
fn a_check_failure_gives_87_and_the_stop_text() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("charliebot", FAKE_CHARLIEBOT);
  let project = project(&sandbox);
  let mut env = run_env(&sandbox, &project);
  env.insert("FAKE_CHECK_FAIL".to_string(), "1".to_string());
  let run = run_runner(&env, &project, &["echo", "remote"]);
  assert_eq!(run.code, 87, "{}", run.describe());
  stop_text_is_printed(&run);
}

#[test]
fn a_check_past_its_limit_gives_87() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("charliebot", FAKE_CHARLIEBOT);
  let project = project(&sandbox);
  let mut env = run_env(&sandbox, &project);
  env.insert("FAKE_CHECK_HANG".to_string(), "1".to_string());
  env.insert("REMOTE_RUN_CONNECT_TIMEOUT".to_string(), "2".to_string());
  let run = run_runner(&env, &project, &["echo", "remote"]);
  assert_eq!(run.code, 87, "{}", run.describe());
  stop_text_is_printed(&run);
}

#[test]
fn a_missing_public_url_names_the_key() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("charliebot", FAKE_CHARLIEBOT);
  let project = project(&sandbox);
  let mut env = run_env(&sandbox, &project);
  env.insert("FAKE_LINK".to_string(), "1".to_string());
  env.insert("FAKE_URL_UNSET".to_string(), "1".to_string());
  let run = run_runner(&env, &project, &["echo", "remote"]);
  assert_eq!(run.code, 86, "{}", run.describe());
  assert!(run.stderr.contains("server.public_url"), "{}", run.describe());
}

#[test]
fn a_passing_check_leaves_a_remote_255_unchanged() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  let project = project(&sandbox);
  let mut env = run_env(&sandbox, &project);
  env.insert("FAKE_REMOTE_STATUS".to_string(), "255".to_string());
  let run = run_runner(&env, &project, &["echo", "remote"]);
  assert_eq!(run.code, 255, "{}", run.describe());
  let log = std::fs::read_to_string(sandbox.root().join("ssh.log")).expect("the ssh log");
  assert!(log.contains("remote-exec.pl"), "the remote command never ran: {log}");
}

/// The payload run.sh sends: NUL-separated tree, directory, Python version, and command words,
/// base64-encoded through the system base64 tool, as run.sh builds it.
fn payload(sandbox: &Sandbox, relative: &str, python: &str, argv: &[&str]) -> String {
  let tree = "~/scripts/remote-run/trees/proj".to_string();
  let mut fields: Vec<String> = vec![tree, relative.to_string(), python.to_string()];
  fields.extend(argv.iter().map(|word| word.to_string()));
  let script = fields
    .iter()
    .map(|field| format!("'{field}'"))
    .collect::<Vec<_>>()
    .join(" ");
  let mut command = Command::new("bash");
  command.arg("-c").arg(format!("printf '%s\\0' {script} | base64 -w0"));
  let output = command.output().expect("base64 runs");
  String::from_utf8(output.stdout).expect("base64 is text").trim().to_string()
}

/// The build host state remote-exec.pl needs: the base with its build slots, plus the two tools a
/// cargo command requires before a slot is taken.
fn install_remote_state(sandbox: &Sandbox) {
  let base = sandbox.root().join("scripts/remote-run");
  std::fs::create_dir_all(base.join("slots")).expect("the slots directory");
  for index in 1..=4 {
    std::fs::write(base.join(format!("slots/slot-{index}.lock")), "").expect("a slot lock");
  }
  std::fs::create_dir_all(base.join("cargo/bin")).expect("the cargo directory");
  harness::write_executable(&base.join("cargo/bin/cargo"), "#!/usr/bin/env bash\nexit 0\n");
  std::fs::create_dir_all(base.join("bin")).expect("the bin directory");
  harness::write_executable(&base.join("bin/sccache"), "#!/usr/bin/env bash\nexit 0\n");
  let tree = sandbox.root().join("scripts/remote-run/trees/proj");
  std::fs::create_dir_all(&tree).expect("the synced tree");
}

fn remote_exec_env(sandbox: &Sandbox) -> Env {
  let mut env = sandbox.env();
  env.insert("HOME".to_string(), sandbox.root().display().to_string());
  env
}

fn run_remote_exec(env: &Env, payload: &str) -> harness::Run {
  let mut command = Command::new("perl");
  command.arg(remote_exec_pl()).arg(payload);
  command.env_clear();
  command.envs(env);
  harness::run(&mut command)
}

#[test]
fn a_command_gets_a_short_tmpdir_and_its_own_exit_code() {
  let sandbox = Sandbox::new();
  install_remote_state(&sandbox);
  let env = remote_exec_env(&sandbox);
  let payload =
    payload(&sandbox, ".", "", &["sh", "-c", "printf '%s' \"$TMPDIR\"; exit 42"]);
  let run = run_remote_exec(&env, &payload);
  assert_eq!(run.code, 42, "{}", run.describe());
  assert!(run.stdout.starts_with("/tmp/rr-"), "{}", run.describe());
  assert!(run.stdout.len() <= 25, "{}", run.describe());
}

#[test]
fn a_command_leaving_a_daemon_does_not_hold_the_runner() {
  let sandbox = Sandbox::new();
  install_remote_state(&sandbox);
  let env = remote_exec_env(&sandbox);
  let payload = payload(&sandbox, ".", "",
    &["sh", "-c", "setsid sh -c 'sleep 30' >/dev/null 2>&1 <&- & sleep 0.2"]);
  let started = Instant::now();
  let run = run_remote_exec(&env, &payload);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(started.elapsed().as_secs() < 5, "the daemon held the runner: {}", run.describe());
  assert!(run.stderr.contains("command left child processes"), "{}", run.describe());
}

#[test]
fn a_cargo_command_without_slots_is_refused() {
  let sandbox = Sandbox::new();
  install_remote_state(&sandbox);
  std::fs::remove_dir_all(sandbox.root().join("scripts/remote-run/slots"))
    .expect("remove the slots");
  let env = remote_exec_env(&sandbox);
  let payload = payload(&sandbox, ".", "", &["cargo", "test"]);
  let run = run_remote_exec(&env, &payload);
  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("build slot is missing"), "{}", run.describe());
}
