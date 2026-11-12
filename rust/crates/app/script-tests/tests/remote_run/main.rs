//! The remote runner: `dev/remote-run/run.sh` reads the build host and its state base from `config.yaml` on each
//! run, and stops with exit 2 while the host is unset. It starts every run with a connectivity check whose Tailscale
//! login link exits 86 and whose other failures and time limit exit 87. The check's stop text names the `/host-auth`
//! page. After the check, the command's exit code comes back unchanged, 255 included. `dev/remote-run/remote-exec.pl`
//! runs a payload with a short TMPDIR on the /dev/shm tmpfs, passes the command's exit code through, and stops the
//! descendants the command leaves behind. The local work guard's refusal names the same host and prints in full while
//! the host is unset.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use charliebot_shell_harness::{self as harness, Env, Sandbox};

const HOST: &str = "build-host";
const PAGE_URL: &str = "https://charlie.example.test:18498/host-auth";
const SCRIPTS: &str = "dev/remote-run";

fn run_sh() -> PathBuf {
  harness::repo_root().join(format!("{SCRIPTS}/run.sh"))
}

fn remote_exec_pl() -> PathBuf {
  harness::repo_root().join(format!("{SCRIPTS}/remote-exec.pl"))
}

fn refuse_sh() -> PathBuf {
  harness::repo_root().join("dev/local-work-guard/refuse.sh")
}

/// The CharlieBot home's config that the runner reads on each run: the build host, and the address the stop text links.
const CONFIG: &str = "server:\n  public_url: https://charlie.example.test:18498\nremote_run:\n  host: build-host\n";

/// The fake ssh answers the runner's remote queries and plays the cases the connectivity check
/// must tell apart. Its behavior comes from the environment, one case per variable. With
/// `FAKE_BUILD_HOME` set, the sweep and marker commands run against a local stand-in for the build
/// host's `$HOME`. The sweep state therefore carries from one remote command to the next. The
/// preflight and the git-commit command return success without state.
const FAKE_SSH: &str = r##"
log="${FAKE_SSH_LOG:?}"
printf '%s\n' "$*" >>"$log"
last="${*: -1}"
case "$last" in
  *" exec "*) exit "${FAKE_REMOTE_STATUS:-0}" ;;
  true)
    if [[ -n "${FAKE_LINK:-}" ]]; then
      echo "# To authenticate, visit: https://login.tailscale.com/a/l0000000000000" >&2
      exec sleep "${FAKE_LINK_HOLD:-5}"
    fi
    [[ -n "${FAKE_CHECK_FAIL:-}" ]] && exit 7
    [[ -n "${FAKE_CHECK_HANG:-}" ]] && exec sleep 30
    ;;
  *update-ref*|*'missing build host tool'*) exit 0 ;;
  *)
    if [[ -n "${FAKE_BUILD_HOME:-}" ]]; then
      HOME="$FAKE_BUILD_HOME" eval "$last"
    elif [[ "$last" == *tree-owners* ]]; then
      printf '__NO_LOCAL_ROOT_RECORD__'
    fi
    ;;
esac
exit 0
"##;

const FAKE_RSYNC: &str = r##"
printf '%s\n' "rsync $*" >>"${FAKE_SSH_LOG:?}"
exit "${FAKE_RSYNC_STATUS:-0}"
"##;

/// A project directory for the runner to sync: no git, so the run takes the directory mode.
fn project(sandbox: &Sandbox) -> PathBuf {
  let project = sandbox.dir("project");
  std::fs::write(project.join("file.txt"), "content\n").expect("write the project file");
  project
}

/// The `charliebot` on PATH is the built CLI, which reads `config.yaml` from `CHARLIEBOT_HOME` on each call.
fn install_cli(sandbox: &Sandbox) {
  let cli = harness::built_binary("charliebot");
  sandbox.stub("charliebot", &format!("exec \"{}\" \"$@\"\n", cli.display()));
}

/// The environment of one run: a CharlieBot home whose `config.yaml` is `config`, and the fake ssh's log.
fn run_env(sandbox: &Sandbox, config: &str) -> Env {
  let home = sandbox.dir("home");
  fs::write(home.join("config.yaml"), config).expect("write the config");
  let mut env = sandbox.env();
  env.insert("CHARLIEBOT_HOME".to_string(), home.display().to_string());
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

fn ssh_log(sandbox: &Sandbox) -> String {
  fs::read_to_string(sandbox.root().join("ssh.log")).unwrap_or_default()
}

#[test]
#[ignore = "integration"]
fn a_login_link_in_the_check_gives_86_and_the_stop_text() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let mut env = run_env(&sandbox, CONFIG);
  env.insert("FAKE_LINK".to_string(), "1".to_string());
  let run = run_runner(&env, &project, &["echo", "remote"]);
  assert_eq!(run.code, 86, "{}", run.describe());
  stop_text_is_printed(&run);
  assert!(run.stderr.contains("Tailscale needs an SSH approval"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_check_failure_gives_87_and_the_stop_text() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let mut env = run_env(&sandbox, CONFIG);
  env.insert("FAKE_CHECK_FAIL".to_string(), "1".to_string());
  let run = run_runner(&env, &project, &["echo", "remote"]);
  assert_eq!(run.code, 87, "{}", run.describe());
  stop_text_is_printed(&run);
}

#[test]
#[ignore = "integration"]
fn a_check_past_its_limit_gives_87() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let mut env = run_env(&sandbox, CONFIG);
  env.insert("FAKE_CHECK_HANG".to_string(), "1".to_string());
  env.insert("REMOTE_RUN_CONNECT_TIMEOUT".to_string(), "2".to_string());
  let run = run_runner(&env, &project, &["echo", "remote"]);
  assert_eq!(run.code, 87, "{}", run.describe());
  stop_text_is_printed(&run);
}

#[test]
#[ignore = "integration"]
fn a_missing_public_url_names_the_key() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let mut env = run_env(&sandbox, "remote_run:\n  host: build-host\n");
  env.insert("FAKE_LINK".to_string(), "1".to_string());
  let run = run_runner(&env, &project, &["echo", "remote"]);
  assert_eq!(run.code, 86, "{}", run.describe());
  assert!(run.stderr.contains("server.public_url"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn an_unset_host_stops_before_any_ssh_call_and_names_the_key() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let mut env = run_env(&sandbox, "server:\n  public_url: https://charlie.example.test:18498\n");
  // The environment no longer names the host, so an exported value does not stand in for the config.
  env.insert("REMOTE_RUN_HOST".to_string(), "env-host".to_string());
  let run = run_runner(&env, &project, &["echo", "remote"]);
  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("remote_run.host"), "{}", run.describe());
  assert!(!ssh_log(&sandbox).contains("env-host"), "the run reached ssh: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_host_written_after_the_start_reaches_the_next_run() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let env = run_env(&sandbox, "server:\n  public_url: https://charlie.example.test:18498\n");
  let unset = run_runner(&env, &project, &["echo", "remote"]);
  assert_eq!(unset.code, 2, "{}", unset.describe());

  // The host is written to config.yaml after the first run. The next run reads the file, with no server restart.
  fs::write(sandbox.root().join("home/config.yaml"), CONFIG).expect("write the config");
  let run = run_runner(&env, &project, &["echo", "remote"]);
  assert_ne!(run.code, 2, "{}", run.describe());
  assert!(ssh_log(&sandbox).contains(HOST), "the next run did not reach the written host: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_passing_check_leaves_a_remote_255_unchanged() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let mut env = run_env(&sandbox, CONFIG);
  env.insert("FAKE_REMOTE_STATUS".to_string(), "255".to_string());
  let run = run_runner(&env, &project, &["echo", "remote"]);
  assert_eq!(run.code, 255, "{}", run.describe());
  let log = ssh_log(&sandbox);
  assert!(
    log.lines().any(|line| line.contains(" exec ") && line.contains("/remote-exec-")),
    "the remote command never ran this checkout's remote-exec copy: {log}"
  );
  assert!(log.lines().any(|line| line.contains(HOST) && line.ends_with(" true")), "the check named no host: {log}");
}

/// The build-host base the fake ssh evaluates the sweep and marker commands against: the tree
/// directory and the owner-record directory the runner reads and removes.
fn install_fake_build_home(sandbox: &Sandbox) -> PathBuf {
  let base = sandbox.root().join("build-home/scripts/remote-run");
  std::fs::create_dir_all(base.join("trees")).expect("the tree directory");
  std::fs::create_dir_all(base.join("tree-owners")).expect("the owner records");
  base
}

/// Runs the runner from `project` with the fake ssh evaluating its state commands against the
/// stand-in build home.
fn run_with_fake_build_home(sandbox: &Sandbox, project: &Path) -> harness::Run {
  let mut env = run_env(sandbox, CONFIG);
  env.insert("FAKE_BUILD_HOME".to_string(), sandbox.root().join("build-home").display().to_string());
  run_runner(&env, project, &["echo", "remote"])
}

/// The 08:15Z install case: the build host holds a tree named like the sync root, recorded to a
/// local directory an earlier run's temp directory removed. The run sweeps the tree, then syncs.
#[test]
#[ignore = "integration"]
fn a_same_name_tree_whose_recorded_directory_is_gone_is_removed_and_the_run_succeeds() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  std::fs::create_dir_all(base.join("trees/project")).expect("the stale tree");
  std::fs::write(base.join("trees/project/leftover.txt"), "stale\n").expect("the stale tree file");
  let gone_owner = sandbox.root().join("gone-owner/src");
  std::fs::write(base.join("tree-owners/project"), gone_owner.display().to_string()).expect("the stale owner record");

  let run = run_with_fake_build_home(&sandbox, &project);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(
    run.stderr.contains("sweep: removed build host tree project because its recorded local directory is gone"),
    "{}",
    run.describe()
  );
  assert!(!base.join("trees/project").exists(), "the stale tree survived the sweep: {}", run.describe());
  let record = fs::read_to_string(base.join("tree-owners/project")).expect("the new owner record");
  assert_eq!(record, project.display().to_string(), "the run did not record the current owner");
}

/// Two live local directories must never share one build-host tree: the same-name refusal holds
/// while the recorded owner directory exists.
#[test]
#[ignore = "integration"]
fn a_same_name_tree_whose_recorded_directory_exists_keeps_the_refusal() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  std::fs::create_dir_all(base.join("trees/project")).expect("the remote tree");
  let live_owner = sandbox.dir("other-root");
  std::fs::write(base.join("tree-owners/project"), live_owner.display().to_string()).expect("the live owner record");

  let run = run_with_fake_build_home(&sandbox, &project);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("belongs to local directory"), "{}", run.describe());
  assert!(!run.stderr.contains("sweep: removed"), "the live tree was swept: {}", run.describe());
  assert!(base.join("trees/project").exists(), "the live tree was removed: {}", run.describe());
}

/// Runs the local work guard's refusal for `tool` with the environment `env`.
fn refuse(env: &Env, tool: &str) -> harness::Run {
  let mut command = Command::new("bash");
  command.arg(refuse_sh()).arg(tool);
  command.env_clear();
  command.envs(env);
  harness::run(&mut command)
}

#[test]
#[ignore = "integration"]
fn the_refusal_names_the_build_host_from_config() {
  let sandbox = Sandbox::new();
  install_cli(&sandbox);
  let env = run_env(&sandbox, CONFIG);
  let run = refuse(&env, "cargo");
  assert_eq!(run.code, 97, "{}", run.describe());
  assert!(run.stderr.contains("refused: cargo"), "{}", run.describe());
  assert!(run.stderr.contains(HOST), "{}", run.describe());
  assert!(run.stderr.contains("dev/remote-run/run.sh"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn the_refusal_prints_in_full_and_names_the_key_while_the_host_is_unset() {
  let sandbox = Sandbox::new();
  install_cli(&sandbox);
  let env = run_env(&sandbox, "server:\n  public_url: https://charlie.example.test:18498\n");
  let run = refuse(&env, "cargo");
  assert_eq!(run.code, 97, "{}", run.describe());
  assert!(run.stderr.contains("refused: cargo"), "{}", run.describe());
  assert!(run.stderr.contains("remote_run.host"), "{}", run.describe());
  assert!(run.stderr.contains("dev/remote-run/run.sh"), "{}", run.describe());
  assert_eq!(run.stderr.lines().count(), 4, "the refusal is not the four-line text: {}", run.describe());
}

/// The payload run.sh sends: NUL-separated tree, directory, Python version, and command words,
/// base64-encoded through the system base64 tool, as run.sh builds it.
fn payload(relative: &str, python: &str, argv: &[&str]) -> String {
  let tree = "~/scripts/remote-run/trees/proj".to_string();
  let mut fields: Vec<String> = vec![tree, relative.to_string(), python.to_string()];
  fields.extend(argv.iter().map(|word| word.to_string()));
  let script = fields.iter().map(|field| format!("'{}'", field.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ");
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
  env.insert("REMOTE_RUN_REMOTE_BASE".to_string(), "scripts/remote-run".to_string());
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
#[ignore = "integration"]
fn a_command_gets_a_short_tmpdir_and_its_own_exit_code() {
  let sandbox = Sandbox::new();
  install_remote_state(&sandbox);
  let env = remote_exec_env(&sandbox);
  let payload = payload(".", "", &["sh", "-c", "printf '%s' \"$TMPDIR\"; exit 42"]);
  let run = run_remote_exec(&env, &payload);
  assert_eq!(run.code, 42, "{}", run.describe());
  assert!(run.stdout.starts_with("/dev/shm/r"), "{}", run.describe());
  assert!(run.stdout.len() <= 25, "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_command_leaving_a_daemon_does_not_hold_the_runner() {
  let sandbox = Sandbox::new();
  install_remote_state(&sandbox);
  let env = remote_exec_env(&sandbox);
  let payload = payload(".", "", &["sh", "-c", "setsid sh -c 'sleep 30' >/dev/null 2>&1 <&- & sleep 0.2"]);
  let started = Instant::now();
  let run = run_remote_exec(&env, &payload);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(started.elapsed().as_secs() < 5, "the daemon held the runner: {}", run.describe());
  assert!(run.stderr.contains("command left child processes"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_cargo_command_without_slots_is_refused() {
  let sandbox = Sandbox::new();
  install_remote_state(&sandbox);
  std::fs::remove_dir_all(sandbox.root().join("scripts/remote-run/slots")).expect("remove the slots");
  let env = remote_exec_env(&sandbox);
  let payload = payload(".", "", &["cargo", "test"]);
  let run = run_remote_exec(&env, &payload);
  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("build slot is missing"), "{}", run.describe());
}
