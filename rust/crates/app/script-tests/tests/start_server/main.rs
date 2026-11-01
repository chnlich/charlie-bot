use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// The hold module serves two test binaries; each uses part of it, so the rest is not dead code there.
#[path = "../common/hold.rs"]
#[allow(dead_code)]
mod hold;

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox, write_executable};
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use regex::Regex;
use serde_json::json;

const RELEASE_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const READY_LINE: &str = "fake_server_ready";
const MARKER_LINE: &str = "fake_server_shutdown_marker";
const EXIT_LINE: &str = "launcher: server exited rc=0";
const UNIT_FILES: [&str; 2] = ["charliebot.slice", "charliebot-agents.slice"];

/// How long a held test waits to be killed. The check kills it long before this.
const HOLD_WAIT: Duration = Duration::from_secs(60);

const FAKE_RELEASE_BINARY: &str = r#"#!/usr/bin/env bash
if [ "${1:-}" = "--check-config" ]; then
  if [ -f "$(dirname "$0")/check-config-fails" ]; then
    echo "config error: unknown key server.bogus" >&2
    exit 3
  fi
  exit 0
fi
here=$(dirname "$0")
printf '{"CHARLIEBOT_REPO_ROOT":"%s"}\n' "$CHARLIEBOT_REPO_ROOT" > "$here/run.json"
printf '%s\n' "fake_release_ready"
"#;

const SIGINT_RELEASE_BINARY: &str = r#"#!/usr/bin/env bash
if [ "${1:-}" = "--check-config" ]; then
  exit 0
fi
trap 'printf "%s\n" "fake_server_shutdown_marker"; exit 0' INT
printf '%s\n' "fake_server_ready"
while :; do
  sleep 3600
done
"#;

const FAKE_UV_RECORDING: &str = r#"printf 'called\n' > "$FAKE_RECORD"
printf '%s\n' "fake_server_ready"
"#;

#[test]
fn launcher_tee_outlives_sigint_and_records_the_exit_line() {
  let sandbox = Sandbox::new();
  sandbox.install_systemd_stubs();
  let releases = sandbox.dir("releases");
  install_release(&releases, SIGINT_RELEASE_BINARY);
  let (env, log_dir) = launcher_environment(&sandbox, &releases);
  let mut child = start_launcher(&env);

  wait_for_log(&mut child, &log_dir, READY_LINE, Duration::from_secs(8));
  child.signal(Signal::SIGINT);
  let deadline = Instant::now() + Duration::from_secs(8);
  loop {
    let log_text = read_log(&log_dir);
    if log_text.contains(EXIT_LINE) || child.try_wait().expect("check the launcher status").is_some() {
      break;
    }
    assert!(Instant::now() < deadline, "the launcher did not exit after SIGINT: {log_text:?}");
    thread::sleep(Duration::from_millis(50));
  }
  let log_text = read_log(&log_dir);
  let status = child.wait().expect("wait for the launcher");
  assert_eq!(status.code(), Some(0), "launcher status={status:?}, log tail: {:?}", tail(&log_text));
  assert!(log_text.contains(MARKER_LINE), "the shutdown marker did not reach the log: {log_text:?}");
  assert!(log_text.contains(EXIT_LINE), "the exit line did not reach the log: {log_text:?}");
  assert!(
    log_text.find(MARKER_LINE).expect("the shutdown marker") < log_text.find(EXIT_LINE).expect("the exit line"),
    "the exit line must follow the shutdown marker: {log_text:?}"
  );
  // The launcher's exit leaves the group to the guard's drop: closing the pipe's write end makes the watchdog kill
  // what is left of the group, itself included, and wait reaps it.
  let group_id = child.group_id();
  drop(child);
  assert!(
    hold::within(hold::END_WAIT, || hold::group_members(group_id).is_empty()),
    "the launcher group outlived its test: {:?}",
    hold::group_members(group_id)
  );
  assert!(hold::has_ended(group_id), "the watchdog still runs: {group_id}");
}

/// The launcher's whole group ends with its test process, which is killed here with SIGKILL: no guard runs, and the
/// watchdog kills the group when the pipe's write end closes.
#[test]
fn the_launcher_group_ends_with_a_killed_test_process() {
  let mut held_test = hold::HeldTest::start("hold_the_launcher_group_for_the_kill_check");
  let text = held_test.recorded();
  let field = |name: &str| {
    text
      .lines()
      .find_map(|line| line.strip_prefix(name))
      .unwrap_or_else(|| panic!("the marker has no {name} line: {text:?}"))
      .to_string()
  };
  let group_id: u32 = field("group=").parse().expect("a group id in the marker");
  let server_current = field("server=");
  let server_release = field("release=");
  let root = PathBuf::from(field("root="));
  assert!(!hold::group_members(group_id).is_empty(), "no process in group {group_id} before the kill");
  assert!(!hold::processes_with_arg(&server_current).is_empty(), "the release server does not run before the kill");
  held_test.kill_and_reap();

  let server_gone =
    || hold::processes_with_arg(&server_current).is_empty() && hold::processes_with_arg(&server_release).is_empty();
  assert!(
    hold::within(hold::END_WAIT, || hold::group_members(group_id).is_empty() && server_gone()),
    "group {group_id} outlived the killed test: members {:?}, servers {:?} {:?}",
    hold::group_members(group_id),
    hold::processes_with_arg(&server_current),
    hold::processes_with_arg(&server_release)
  );
  assert!(hold::has_ended(group_id), "the watchdog still runs: {group_id}");
  remove_killed_test_root(&root);
}

/// Started by `the_launcher_group_ends_with_a_killed_test_process` in a second copy of this binary. Without
/// `LEAK_CHECK_HOLD` it does nothing, so an ordinary run of this file passes it.
#[test]
fn hold_the_launcher_group_for_the_kill_check() {
  let Some(marker) = hold::marker() else { return };
  let sandbox = Sandbox::new();
  sandbox.install_systemd_stubs();
  let releases = sandbox.dir("releases");
  let release = install_release(&releases, SIGINT_RELEASE_BINARY);
  let (env, log_dir) = launcher_environment(&sandbox, &releases);
  let mut group = start_launcher(&env);
  wait_for_log(&mut group, &log_dir, READY_LINE, Duration::from_secs(8));
  let text = format!(
    "group={}\nserver={}\nrelease={}\nroot={}\n",
    group.group_id(),
    releases.join("current/charliebot-server").display(),
    release.join("charliebot-server").display(),
    sandbox.root().display()
  );
  hold::write_marker(&marker, &text);
  thread::sleep(HOLD_WAIT);
}

/// A test process killed with SIGKILL never drops its sandbox, so its root stays on disk; the check removes it.
fn remove_killed_test_root(root: &Path) {
  if let Err(error) = fs::remove_dir_all(root) {
    eprintln!("cannot remove the killed test's root {}: {error}", root.display());
  }
}

#[test]
fn launcher_without_current_refuses_to_start_and_names_the_missing_release() {
  let sandbox = Sandbox::new();
  sandbox.install_systemd_stubs();
  let releases = sandbox.dir("releases");
  let (run, log_dir) = run_launcher(&sandbox, &releases);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains(&releases.join("current").display().to_string()));
  assert!(!log_dir.exists());
  assert!(!sandbox.root().join("uv-record.json").exists());
}

#[test]
fn launcher_with_current_runs_the_release_binary_with_the_repo_root() {
  let sandbox = Sandbox::new();
  sandbox.install_systemd_stubs();
  let releases = sandbox.dir("releases");
  let release = install_release(&releases, FAKE_RELEASE_BINARY);
  let (run, log_dir) = run_launcher(&sandbox, &releases);

  let record_path = release.join("run.json");
  assert!(record_path.exists(), "{}", run.stderr);
  assert_eq!(run.code, 0, "{}", run.stderr);
  assert_server_scope(&run, &sandbox);
  let record: serde_json::Value =
    serde_json::from_slice(&fs::read(&record_path).expect("read the release environment record"))
      .expect("the release environment record is JSON");
  assert_eq!(record, json!({"CHARLIEBOT_REPO_ROOT": harness::repo_root().display().to_string()}));
  assert!(!sandbox.root().join("uv-record.json").exists(), "no uv command runs for a current release");
  let log_text = read_log(&log_dir);
  let log_lines = log_text.lines().collect::<Vec<_>>();
  let first_line = log_lines.first().expect("the launcher line");
  assert!(first_line.contains("rust") && first_line.contains(RELEASE_SHA));
  assert!(log_lines.contains(&"fake_release_ready"));
}

#[test]
fn launcher_with_current_stops_when_check_config_fails_and_names_the_rollback() {
  let sandbox = Sandbox::new();
  sandbox.install_systemd_stubs();
  let releases = sandbox.dir("releases");
  let release = install_release(&releases, FAKE_RELEASE_BINARY);
  fs::write(release.join("check-config-fails"), "").expect("write the failing config marker");
  let (run, _) = run_launcher(&sandbox, &releases);

  assert_eq!(run.code, 3, "{}", run.describe());
  assert!(run.stderr.contains("config error"));
  assert!(run.stdout.contains("scripts/switch-server.sh rust"));
  assert!(!release.join("run.json").exists());
  assert!(!sandbox.root().join("uv-record.json").exists());
}

#[test]
fn preflight_accepts_limits_that_match_the_unit_files() {
  let sandbox = Sandbox::new();
  let launcher = stash_launcher(&sandbox);
  sandbox.install_systemd_stubs();
  let run = run_preflight(&launcher, &sandbox.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.is_empty());
}

#[test]
fn preflight_rejects_a_nonzero_swap_limit() {
  let sandbox = Sandbox::new();
  let launcher = stash_launcher(&sandbox);
  let unit_file = launcher.parent().expect("the scripts directory").join("systemd/charliebot-agents.slice");
  replace_text(&unit_file, "MemorySwapMax=0", "MemorySwapMax=1G");
  sandbox.install_systemd_stubs();
  let mut env = sandbox.env();
  env.insert("SYSTEMCTL_MEMORY_SWAP_MAX".to_string(), 1024_u64.pow(3).to_string());
  let run = run_preflight(&launcher, &env);

  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("MemorySwapMax='1G', expected 0"));
}

#[test]
fn preflight_reads_a_unit_file_whose_last_line_has_no_newline() {
  let sandbox = Sandbox::new();
  let launcher = stash_launcher(&sandbox);
  let unit_file = launcher.parent().expect("the scripts directory").join("systemd/charliebot-agents.slice");
  fs::write(&unit_file, "[Slice]\nMemoryMax=80%\nMemorySwapMax=0").expect("write the unterminated unit file");
  sandbox.install_systemd_stubs();
  let run = run_preflight(&launcher, &sandbox.env());

  assert_eq!(run.code, 0, "{}", run.describe());
}

#[test]
fn percentage_limit_rounds_down_to_whole_pages() {
  let sandbox = Sandbox::new();
  let script = harness::repo_root().join("scripts/start-server.sh");
  let mut command = harness::command("bash", &sandbox.env());
  command.args(["-c", "source \"$1\"; _percent_bytes 80 64428228", "bash"]).arg(script);
  let run = harness::run(&mut command);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(run.stdout, "52779601920");
}

#[test]
fn preflight_rejects_a_slice_limit_that_differs_from_its_unit_file() {
  let sandbox = Sandbox::new();
  let launcher = stash_launcher(&sandbox);
  let unit_file = launcher.parent().expect("the scripts directory").join("systemd/charliebot-agents.slice");
  replace_text(&unit_file, "MemoryMax=80%", "MemoryMax=40%");
  sandbox.install_systemd_stubs();
  let run = run_preflight(&launcher, &sandbox.env());

  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("charliebot-agents.slice"));
  assert!(run.stderr.contains("MemoryMax"));
  assert!(run.stderr.contains(&harness::memtotal_percent_bytes(80).to_string()));
  assert!(run.stderr.contains(&harness::memtotal_percent_bytes(40).to_string()));
}

#[test]
fn preflight_rejects_a_missing_required_unit_limit() {
  let sandbox = Sandbox::new();
  let launcher = stash_launcher(&sandbox);
  let unit_file = launcher.parent().expect("the scripts directory").join("systemd/charliebot-agents.slice");
  replace_text(&unit_file, "MemoryMax=80%\n", "");
  sandbox.install_systemd_stubs();
  let mut env = sandbox.env();
  env.insert("SYSTEMCTL_MEMORY_MAX".to_string(), "infinity".to_string());
  let run = run_preflight(&launcher, &env);

  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("charliebot-agents.slice unit file has no finite MemoryMax"));
}

#[test]
fn preflight_rejects_an_inactive_slice() {
  let sandbox = Sandbox::new();
  let launcher = stash_launcher(&sandbox);
  sandbox.install_systemd_stubs();
  let mut env = sandbox.env();
  env.insert("SYSTEMCTL_ACTIVE_STATE".to_string(), "inactive".to_string());
  let run = run_preflight(&launcher, &env);

  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("charliebot-agents.slice"));
  assert!(run.stderr.contains("inactive"));
}

#[test]
fn launcher_preflight_checks_slices_and_records_a_trial_scope() {
  let sandbox = Sandbox::new();
  let launcher = stash_launcher(&sandbox);
  sandbox.install_systemd_stubs();
  let log_dir = sandbox.root().join("logs");
  let run = run_launcher_preflight(&launcher, &log_dir, &sandbox.env());

  assert_eq!(run.code, 0, "{}", run.describe());
  let records = sandbox.scope_records();
  assert_eq!(records.len(), 1);
  let record = &records[0];
  let unit_name = &record.unit_name;
  assert!(Regex::new(r"^charliebot-preflight-\d{8}_\d{6}-\d+$").expect("the unit pattern").is_match(unit_name));
  let expected_arguments = vec![
    "--user".to_string(),
    "--scope".to_string(),
    format!("--unit={unit_name}"),
    "--slice=charliebot.slice".to_string(),
    "-p".to_string(),
    "MemorySwapMax=0".to_string(),
    "--".to_string(),
    "true".to_string(),
  ];
  assert_eq!(record.arguments, expected_arguments);
  assert!(!log_dir.exists());
}

#[test]
fn launcher_preflight_exits_1_when_systemd_refuses_the_trial_scope() {
  let sandbox = Sandbox::new();
  let launcher = stash_launcher(&sandbox);
  sandbox.install_systemd_stubs();
  let log_dir = sandbox.root().join("logs");
  let mut env = sandbox.env();
  env.insert("SYSTEMD_RUN_REFUSE_ALL".to_string(), "1".to_string());
  let run = run_launcher_preflight(&launcher, &log_dir, &env);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("start refused by test"));
  assert!(sandbox.scope_records().is_empty());
  assert!(!log_dir.exists());
}

#[test]
fn launcher_preflight_exits_1_when_a_slice_limit_differs() {
  let sandbox = Sandbox::new();
  let launcher = stash_launcher(&sandbox);
  sandbox.install_systemd_stubs();
  let mut env = sandbox.env();
  env.insert("SYSTEMCTL_MEMORY_MAX".to_string(), (harness::memtotal_percent_bytes(80) + 1).to_string());
  let log_dir = sandbox.root().join("logs");
  let run = run_launcher_preflight(&launcher, &log_dir, &env);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("charliebot-agents.slice MemoryMax"));
  assert!(sandbox.scope_records().is_empty());
  assert!(!log_dir.exists());
}

#[test]
fn launcher_rejects_an_unknown_argument_before_writing_a_server_log() {
  let sandbox = Sandbox::new();
  let launcher = harness::repo_root().join("scripts/start-server.sh");
  let log_dir = sandbox.root().join("logs");
  let mut env = sandbox.env();
  env.insert("CHARLIEBOT_LOG_DIR".to_string(), log_dir.display().to_string());
  let mut command = harness::command("bash", &env);
  command.arg(launcher).arg("--unknown");
  let run = harness::run(&mut command);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("usage: start-server.sh [--preflight]"));
  assert!(!log_dir.exists());
}

#[test]
fn shell_entry_points_parse_start_server() {
  assert_shell_entry_point_parses("start-server.sh");
}

#[test]
fn shell_entry_points_parse_setup() {
  assert_shell_entry_point_parses("setup.sh");
}

#[test]
fn shell_entry_points_parse_trial() {
  assert_shell_entry_point_parses("release/trial.sh");
}

fn assert_shell_entry_point_parses(script: &str) {
  let sandbox = Sandbox::new();
  let path = harness::repo_root().join("scripts").join(script);
  let mut command = harness::command("bash", &sandbox.env());
  command.arg("-n").arg(path);
  let run = harness::run(&mut command);

  assert_eq!(run.code, 0, "{}", run.describe());
}

fn stash_launcher(sandbox: &Sandbox) -> PathBuf {
  let scripts = sandbox.dir("scripts");
  let systemd = sandbox.dir("scripts/systemd");
  let launcher = scripts.join("start-server.sh");
  harness::copy_file(&harness::repo_root().join("scripts/start-server.sh"), &launcher);
  for unit in UNIT_FILES {
    harness::copy_file(&harness::repo_root().join("scripts/systemd").join(unit), &systemd.join(unit));
  }
  launcher
}

fn run_preflight(launcher: &Path, env: &Env) -> Run {
  let mut command = harness::command("bash", env);
  command.args(["-c", "source \"$1\"; preflight_slices", "bash"]).arg(launcher);
  harness::run(&mut command)
}

fn run_launcher_preflight(launcher: &Path, log_dir: &Path, env: &Env) -> Run {
  let mut env = env.clone();
  env.insert("CHARLIEBOT_LOG_DIR".to_string(), log_dir.display().to_string());
  let mut command = harness::command("bash", &env);
  command.arg(launcher).arg("--preflight");
  harness::run(&mut command)
}

fn launcher_environment(sandbox: &Sandbox, releases: &Path) -> (Env, PathBuf) {
  let log_dir = sandbox.root().join("logs");
  let uv_record = sandbox.root().join("uv-record.json");
  let mut env = sandbox.env();
  env.insert("CHARLIEBOT_LOG_DIR".to_string(), log_dir.display().to_string());
  env.insert("CHARLIEBOT_RELEASES_DIR".to_string(), releases.display().to_string());
  env.insert("FAKE_RECORD".to_string(), uv_record.display().to_string());
  (env, log_dir)
}

fn run_launcher(sandbox: &Sandbox, releases: &Path) -> (Run, PathBuf) {
  sandbox.stub("uv", FAKE_UV_RECORDING);
  let (env, log_dir) = launcher_environment(sandbox, releases);
  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("scripts/start-server.sh"));
  (harness::run(&mut command), log_dir)
}

fn install_release(releases: &Path, binary: &str) -> PathBuf {
  let release = releases.join(RELEASE_SHA);
  fs::create_dir_all(&release).expect("create the release directory");
  write_executable(&release.join("charliebot-server"), binary);
  std::os::unix::fs::symlink(RELEASE_SHA, releases.join("current")).expect("write the current release link");
  release
}

fn assert_server_scope(run: &Run, sandbox: &Sandbox) {
  let records = sandbox.scope_records();
  assert_eq!(records.len(), 1);
  let record = &records[0];
  let unit_name = &record.unit_name;
  assert!(Regex::new(r"^charliebot-server-\d{8}_\d{6}-\d+$").expect("the unit pattern").is_match(unit_name));
  let expected_scope_line = format!("  scope  -> {unit_name}");
  assert!(run.stdout.lines().any(|line| line == expected_scope_line));
  assert!(record.arguments.contains(&format!("--unit={unit_name}")));
  assert!(record.arguments.contains(&"--slice=charliebot.slice".to_string()));
  let property_index = record.arguments.iter().position(|argument| argument == "-p").expect("the scope property");
  assert_eq!(record.arguments[property_index + 1], "MemorySwapMax=0");
}

fn read_log(log_dir: &Path) -> String {
  let latest = log_dir.join("server-latest.log");
  if !latest.exists() {
    return String::new();
  }
  fs::read_to_string(&latest).unwrap_or_else(|error| panic!("read {}: {error}", latest.display()))
}

fn wait_for_log(child: &mut ProcessGroup, log_dir: &Path, needle: &str, timeout: Duration) {
  let deadline = Instant::now() + timeout;
  loop {
    let log_text = read_log(log_dir);
    if log_text.contains(needle) {
      return;
    }
    if let Some(status) = child.try_wait().expect("check the launcher status") {
      panic!("launcher exited before its server was ready: status={status:?}; log={log_text:?}");
    }
    assert!(Instant::now() < deadline, "the ready line never reached the log: {log_text:?}");
    thread::sleep(Duration::from_millis(50));
  }
}

fn tail(text: &str) -> &str {
  let start = text.len().saturating_sub(400);
  &text[start..]
}

fn replace_text(path: &Path, old: &str, new: &str) {
  let contents = fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
  assert!(contents.contains(old), "{} does not contain {old:?}", path.display());
  fs::write(path, contents.replace(old, new)).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

/// Starts the launcher in a process group of its own, under a watchdog that reads a pipe. The watchdog kills the
/// group when the pipe's write end closes, which happens when this process ends however it ends, and it is the only
/// cleanup when this process is killed and no guard runs. The launcher gets SIGINT's default action, because the
/// test sends SIGINT to the whole group.
fn start_launcher(env: &Env) -> ProcessGroup {
  // The watchdog's `$$` is its pid, and as group leader its group id. It ignores the group's SIGINT. `kill -s` is the
  // spelling dash, the /bin/sh of Debian and Ubuntu, accepts with a group id.
  const WATCHDOG: &str = "trap '' INT; read _; kill -s KILL -- -$$";
  // Both ends are close-on-exec and the write end is kept here alone, so no child holds it past exec: the read ends
  // exactly when this process's write end drops.
  let (reader, writer) = std::io::pipe().expect("the watchdog's pipe");
  let mut watchdog = Command::new("/bin/sh");
  watchdog
    .args(["-c", WATCHDOG])
    .stdin(Stdio::from(reader))
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .process_group(0);
  // Not harness::spawn: a parent-death signal would end the watchdog with this process, before it kills the group.
  let watchdog = watchdog.spawn().expect("start the watchdog");
  let mut command = harness::command("bash", env);
  command
    .arg(harness::repo_root().join("scripts/start-server.sh"))
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .process_group(watchdog.id() as i32);
  unsafe {
    command.pre_exec(|| {
      if libc::signal(libc::SIGINT, libc::SIG_DFL) == libc::SIG_ERR {
        return Err(std::io::Error::last_os_error());
      }
      Ok(())
    });
  }
  let launcher = harness::spawn(&mut command);
  ProcessGroup { launcher, watchdog, pipe: Some(writer) }
}

/// The launcher's process group, whose id is the watchdog's pid.
struct ProcessGroup {
  launcher: Child,
  watchdog: Child,
  /// The pipe's write end, held only here: dropping it ends the watchdog's read, and with it the group.
  pipe: Option<std::io::PipeWriter>,
}

impl ProcessGroup {
  fn signal(&self, signal: Signal) {
    killpg(Pid::from_raw(self.group_id() as i32), signal).expect("signal the launcher process group");
  }

  fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
    self.launcher.try_wait()
  }

  fn wait(&mut self) -> std::io::Result<ExitStatus> {
    self.launcher.wait()
  }

  /// The group's id, which is the watchdog's pid.
  fn group_id(&self) -> u32 {
    self.watchdog.id()
  }
}

impl Drop for ProcessGroup {
  fn drop(&mut self) {
    // A launcher that still runs is killed directly. One that has exited leaves the group to the watchdog: the
    // write end closes below, the watchdog reads its end, kills the group, itself included, and is reaped here.
    if self.launcher.try_wait().expect("check the launcher before cleanup").is_none() {
      match killpg(Pid::from_raw(self.group_id() as i32), Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(error) => panic!("kill the launcher process group: {error}"),
      }
      self.launcher.wait().expect("reap the launcher process");
    }
    self.pipe.take();
    self.watchdog.wait().expect("reap the watchdog");
  }
}
