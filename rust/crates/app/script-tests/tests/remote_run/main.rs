//! The remote runner: `dev/remote-run/run.sh` reads the build host and its state base from `config.yaml` on each
//! run. It stops with exit 2 while the host is unset. It starts every run with a connectivity check whose Tailscale
//! login link exits 86 and whose other failures and time limit exit 87. The exit 86 stop text names the `/host-auth`
//! page. After the check, the command's exit code comes back unchanged, 255 included. Owner records identify the
//! caller host and local root; a sweep drops or removes only records for its own host. `remote-exec.pl` stops
//! processes with a cwd or executable path inside a tree before syncing or deleting it. For commands, it uses a short
//! TMPDIR on the
//! /dev/shm tmpfs, stops descendants on exit or signal, and detects a closed SSH output channel. The local work
//! guard's refusal names the same host and prints in full while the host is unset.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use charliebot_shell_harness::{self as harness, Env, Sandbox};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

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
  *--sweep-tree*)
    if [[ -n "${FAKE_STOP_FAIL:-}" ]]; then
      echo 'remote-exec: failed to stop a process in the build tree' >&2
      exit 23
    fi
    # A tree that vanishes after the sweep listed it: the state a concurrent run's removal leaves.
    if [[ -n "${FAKE_VANISH_TREE:-}" ]]; then
      rm -rf -- "$FAKE_VANISH_TREE"
    fi
    [[ -n "${FAKE_BUILD_HOME:-}" ]] || exit 0
    HOME="$FAKE_BUILD_HOME" eval "$last"
    exit $?
    ;;
  *--claim-tree*)
    [[ -n "${FAKE_BUILD_HOME:-}" ]] || exit 0
    HOME="$FAKE_BUILD_HOME" eval "$last"
    exit $?
    ;;
  *--stop-tree*)
    if [[ -n "${FAKE_STOP_FAIL:-}" ]]; then
      echo 'remote-exec: failed to stop a process in the build tree' >&2
      exit 23
    fi
    if [[ -n "${FAKE_STOP_SUCCESS:-}" ]]; then
      exit 0
    fi
    HOME="$FAKE_BUILD_HOME" eval "$last"
    ;;
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
      if [[ -n "${FAKE_VANISH_OWNER:-}" && "$last" == *"tree-owners/$FAKE_VANISH_OWNER"* ]]; then
        rm -rf -- "$FAKE_BUILD_HOME/scripts/remote-run/trees/$FAKE_VANISH_OWNER" \
          "$FAKE_BUILD_HOME/scripts/remote-run/tree-owners/$FAKE_VANISH_OWNER"
      fi
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
if [[ -n "${FAKE_UPLOAD_STATUS:-}" && "${*: -1}" == *"/trees/"* ]]; then
  exit "$FAKE_UPLOAD_STATUS"
fi
if [[ -n "${FAKE_BUILD_HOME:-}" ]]; then
  source="${@: -2:1}"
  destination="${@: -1}"
  if [[ "$source" == */remote-exec.pl ]]; then
    install -D -m 755 "$source" "$FAKE_BUILD_HOME/scripts/remote-run/${destination##*/}"
  fi
fi
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
  sandbox.stub("hostname", "printf 'host-a\\n'\n");
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
  assert!(run.stderr.contains("Do not retry the command"), "{}", run.describe());
}

fn host_auth_page_is_printed(run: &harness::Run) {
  assert!(run.stderr.contains(PAGE_URL), "{}", run.describe());
}

fn host_auth_page_is_not_printed(run: &harness::Run) {
  assert!(!run.stderr.contains("/host-auth"), "{}", run.describe());
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
  host_auth_page_is_printed(&run);
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
  host_auth_page_is_not_printed(&run);
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
  host_auth_page_is_not_printed(&run);
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
  std::fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).expect("make the remote base private");
  base
}

fn owner_record(host: &str, root: &Path) -> String {
  format!("host={host}\nroot={}\n", root.display())
}

fn fake_build_home_env(sandbox: &Sandbox) -> Env {
  let mut env = run_env(sandbox, CONFIG);
  env.insert("FAKE_BUILD_HOME".to_string(), sandbox.root().join("build-home").display().to_string());
  env
}

/// The runner's run from `project`, with the fake ssh evaluating its state commands against the stand-in
/// build home.
fn run_with_fake_build_home(sandbox: &Sandbox, project: &Path) -> harness::Run {
  let env = fake_build_home_env(sandbox);
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
  std::fs::write(base.join("tree-owners/project"), owner_record("host-a", &gone_owner))
    .expect("the stale owner record");

  let run = run_with_fake_build_home(&sandbox, &project);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(
    run.stderr.contains("sweep: removed build host tree project because its recorded local directory is gone"),
    "{}",
    run.describe()
  );
  assert!(!base.join("trees/project/leftover.txt").exists(), "the stale tree survived the sweep: {}", run.describe());
  let record = fs::read_to_string(base.join("tree-owners/project")).expect("the new owner record");
  assert_eq!(record, owner_record("host-a", &project), "the run did not record the current owner");
}

/// A caller on host A cannot infer that a host B path is gone by checking the same string locally.
#[test]
#[ignore = "integration"]
fn a_sweep_keeps_a_tree_owned_by_another_caller_host() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let remote_tree = base.join("trees/host-b-tree");
  std::fs::create_dir_all(&remote_tree).expect("the host B tree");
  let host_b_root = sandbox.root().join("host-b/missing-root");
  let host_b_record = owner_record("host-b", &host_b_root);
  std::fs::write(base.join("tree-owners/host-b-tree"), &host_b_record).expect("the host B owner record");

  let run = run_with_fake_build_home(&sandbox, &project);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(remote_tree.exists(), "the host B tree was swept: {}", run.describe());
  assert_eq!(fs::read_to_string(base.join("tree-owners/host-b-tree")).expect("the host B owner record"), host_b_record);
  assert!(run.stderr.contains("owned by caller host host-b"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_sweep_keeps_a_legacy_owner_record_that_does_not_name_a_host() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let remote_tree = base.join("trees/legacy-tree");
  std::fs::create_dir_all(&remote_tree).expect("the legacy tree");
  let legacy_record = sandbox.root().join("missing-legacy-root");
  std::fs::write(base.join("tree-owners/legacy-tree"), legacy_record.display().to_string())
    .expect("the legacy owner record");

  let run = run_with_fake_build_home(&sandbox, &project);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(remote_tree.exists(), "the legacy tree was swept: {}", run.describe());
  assert_eq!(
    fs::read_to_string(base.join("tree-owners/legacy-tree")).expect("the legacy owner record"),
    legacy_record.display().to_string()
  );
  assert!(run.stderr.contains("legacy owner record has no caller host"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_sweep_stops_and_reports_processes_in_a_stale_tree_before_removing_it() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let remote_tree = base.join("trees/orphan-tree");
  std::fs::create_dir_all(&remote_tree).expect("the stale tree");
  let gone_owner = sandbox.root().join("gone-owner");
  std::fs::write(base.join("tree-owners/orphan-tree"), owner_record("host-a", &gone_owner))
    .expect("the stale owner record");
  let mut process =
    Command::new("sleep").arg("30").current_dir(&remote_tree).spawn().expect("start the process in the stale tree");
  let pid = process.id();

  let run = run_with_fake_build_home(&sandbox, &project);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains(&format!("stopped process {pid} in tree")), "{}", run.describe());
  assert!(!remote_tree.exists(), "the stale tree survived the sweep: {}", run.describe());
  assert!(!process.wait().expect("wait for the stopped process").success());
}

#[test]
#[ignore = "integration"]
fn a_failed_tree_stop_keeps_the_tree_and_fails_the_sweep() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let remote_tree = base.join("trees/orphan-tree");
  std::fs::create_dir_all(&remote_tree).expect("the stale tree");
  let gone_owner = sandbox.root().join("gone-owner");
  let record = owner_record("host-a", &gone_owner);
  std::fs::write(base.join("tree-owners/orphan-tree"), &record).expect("the stale owner record");
  let mut env = fake_build_home_env(&sandbox);
  env.insert("FAKE_STOP_FAIL".to_string(), "1".to_string());

  let run = run_runner(&env, &project, &["echo", "remote"]);

  assert_eq!(run.code, 23, "{}", run.describe());
  assert!(run.stderr.contains("failed to stop a process in the build tree"), "{}", run.describe());
  assert!(remote_tree.exists(), "the tree was removed after its stop failed: {}", run.describe());
  assert_eq!(fs::read_to_string(base.join("tree-owners/orphan-tree")).expect("the stale owner record"), record);
}

/// A record whose tree and local root are both gone is dropped, the drop is named, and the run succeeds.
#[test]
#[ignore = "integration"]
fn a_sweep_drops_a_record_whose_tree_and_local_root_are_gone() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let gone_owner = sandbox.root().join("gone-owner/src");
  std::fs::write(base.join("tree-owners/stale-tree"), owner_record("host-a", &gone_owner))
    .expect("the stale owner record");

  let run = run_with_fake_build_home(&sandbox, &project);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("dropped the owner record of build host tree stale-tree"), "{}", run.describe());
  assert!(!base.join("tree-owners/stale-tree").exists(), "the stale record survived the sweep: {}", run.describe());
  assert!(!base.join("trees/stale-tree").exists(), "the sweep created the stale tree: {}", run.describe());
}

/// A record whose tree is missing while its local root exists is dropped, and the sync of that root claims the name
/// again, so the tree and its record come back together.
#[test]
#[ignore = "integration"]
fn a_missing_tree_with_a_live_local_root_is_recreated_by_the_next_sync() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  std::fs::write(base.join("tree-owners/project"), owner_record("host-a", &project)).expect("the owner record");

  let run = run_with_fake_build_home(&sandbox, &project);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("dropped the owner record of build host tree project"), "{}", run.describe());
  assert!(base.join("trees/project").is_dir(), "the sync did not recreate the tree: {}", run.describe());
  let record = fs::read_to_string(base.join("tree-owners/project")).expect("the owner record");
  assert_eq!(record, owner_record("host-a", &project), "the sync did not reclaim the name: {}", run.describe());
}

/// A record this caller host cannot prove it owns stays as it is, even when its tree is missing: another host's record,
/// and a path-only legacy record.
#[test]
#[ignore = "integration"]
fn a_sweep_keeps_records_this_host_cannot_prove_it_owns_when_their_tree_is_missing() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let host_b_record = owner_record("host-b", &sandbox.root().join("host-b/missing-root"));
  std::fs::write(base.join("tree-owners/host-b-stale"), &host_b_record).expect("the host B owner record");
  let legacy_record = sandbox.root().join("missing-legacy-root").display().to_string();
  std::fs::write(base.join("tree-owners/legacy-stale"), &legacy_record).expect("the legacy owner record");

  let run = run_with_fake_build_home(&sandbox, &project);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(fs::read_to_string(base.join("tree-owners/host-b-stale")).expect("the host B record"), host_b_record);
  assert_eq!(fs::read_to_string(base.join("tree-owners/legacy-stale")).expect("the legacy record"), legacy_record);
}

/// A tree that a concurrent run removes after this sweep listed it: the sweep drops the record it finds, and the run
/// succeeds. The fake removes the tree between the listing and the sweep's call, the interleaving that failed the run.
#[test]
#[ignore = "integration"]
fn a_tree_removed_after_the_listing_drops_its_record_and_the_run_succeeds() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let vanishing = base.join("trees/vanishing-tree");
  std::fs::create_dir_all(&vanishing).expect("the vanishing tree");
  let gone_owner = sandbox.root().join("gone-owner");
  std::fs::write(base.join("tree-owners/vanishing-tree"), owner_record("host-a", &gone_owner))
    .expect("the stale owner record");
  let mut env = fake_build_home_env(&sandbox);
  env.insert("FAKE_VANISH_TREE".to_string(), vanishing.display().to_string());

  let run = run_runner(&env, &project, &["echo", "remote"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("dropped the owner record of build host tree vanishing-tree"), "{}", run.describe());
  assert!(!base.join("tree-owners/vanishing-tree").exists(), "{}", run.describe());
}

/// Another sweep can remove both entries after the listing; the current sync then reclaims the now-empty name.
#[test]
#[ignore = "integration"]
fn a_tree_and_record_removed_after_the_listing_allow_the_current_sync_to_reclaim_the_name() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  std::fs::create_dir_all(base.join("trees/project")).expect("the stale tree");
  let gone_owner = sandbox.root().join("gone-owner");
  std::fs::write(base.join("tree-owners/project"), owner_record("host-a", &gone_owner))
    .expect("the stale owner record");
  let mut env = fake_build_home_env(&sandbox);
  env.insert("FAKE_VANISH_OWNER".to_string(), "project".to_string());

  let run = run_runner(&env, &project, &["echo", "remote"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(base.join("trees/project").is_dir(), "the run did not recreate the tree: {}", run.describe());
  let record = fs::read_to_string(base.join("tree-owners/project")).expect("the reclaimed owner record");
  assert_eq!(record, owner_record("host-a", &project), "the run did not reclaim the name: {}", run.describe());
}

/// A record entry that is not a regular file fails the sweep: a dangling symlink is never skipped as absent.
#[test]
#[ignore = "integration"]
fn a_dangling_owner_record_entry_fails_the_sweep() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  std::os::unix::fs::symlink(sandbox.root().join("nowhere"), base.join("tree-owners/odd-tree"))
    .expect("the dangling owner record");

  let run = run_with_fake_build_home(&sandbox, &project);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("odd-tree"), "{}", run.describe());
}

/// A symlink to a readable owner record is still an invalid record entry, even for a live tree and local root.
#[test]
#[ignore = "integration"]
fn a_symlink_to_a_regular_owner_record_fails_the_sweep() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  std::fs::create_dir_all(base.join("trees/linked-tree")).expect("the linked tree");
  let source = base.join("owner-record-source");
  std::fs::write(&source, owner_record("host-a", &project)).expect("the owner record target");
  std::os::unix::fs::symlink(&source, base.join("tree-owners/linked-tree")).expect("the symlinked owner record");

  let run = run_with_fake_build_home(&sandbox, &project);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("not a regular file"), "{}", run.describe());
}

/// A claim whose tree cannot be created leaves no owner record: the record is written first and removed again when the
/// tree's creation fails.
#[test]
#[ignore = "integration"]
fn a_claim_whose_tree_cannot_be_created_leaves_no_owner_record() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  std::fs::set_permissions(base.join("trees"), fs::Permissions::from_mode(0o500)).expect("make the trees read-only");

  let run = run_with_fake_build_home(&sandbox, &project);
  std::fs::set_permissions(base.join("trees"), fs::Permissions::from_mode(0o700)).expect("restore the trees directory");

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(!base.join("tree-owners/project").exists(), "a failed claim left its owner record: {}", run.describe());
  assert!(!base.join("trees/project").exists(), "a failed claim created its tree: {}", run.describe());
}

/// An upload that fails after its claim keeps the claimed tree and its owner record together.
#[test]
#[ignore = "integration"]
fn a_failed_upload_keeps_the_claimed_tree_with_its_owner_record() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let mut env = fake_build_home_env(&sandbox);
  env.insert("FAKE_UPLOAD_STATUS".to_string(), "23".to_string());

  let run = run_runner(&env, &project, &["echo", "remote"]);

  assert_eq!(run.code, 23, "{}", run.describe());
  assert!(base.join("trees/project").is_dir(), "the claimed tree is missing: {}", run.describe());
  let record = fs::read_to_string(base.join("tree-owners/project")).expect("the owner record");
  assert_eq!(record, owner_record("host-a", &project), "{}", run.describe());
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
  std::fs::write(base.join("tree-owners/project"), owner_record("host-a", &live_owner)).expect("the live owner record");

  let run = run_with_fake_build_home(&sandbox, &project);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("belongs to local directory"), "{}", run.describe());
  assert!(!run.stderr.contains("sweep: removed"), "the live tree was swept: {}", run.describe());
  assert!(base.join("trees/project").exists(), "the live tree was removed: {}", run.describe());
}

/// An upload that replaces a tree's files stops existing in-tree processes first.
#[test]
#[ignore = "integration"]
fn a_sync_stops_the_existing_tree_before_uploading_replacements() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  std::fs::create_dir_all(base.join("trees/project")).expect("the existing remote tree");
  std::fs::write(base.join("tree-owners/project"), owner_record("host-a", &project)).expect("the current owner record");
  let mut env = fake_build_home_env(&sandbox);
  env.insert("FAKE_STOP_SUCCESS".to_string(), "1".to_string());

  let run = run_runner(&env, &project, &["echo", "remote"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  let log = ssh_log(&sandbox);
  let stop_index = log.lines().position(|line| line.contains("--stop-tree")).expect("tree stop call");
  let upload_index =
    log.lines().position(|line| line.starts_with("rsync ") && line.contains("trees/project/")).expect("tree upload");
  assert!(stop_index < upload_index, "the upload ran before the stop: {log}");
}

#[test]
#[ignore = "integration"]
fn a_failed_sync_tree_stop_keeps_the_tree_and_skips_upload() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let remote_tree = base.join("trees/project");
  std::fs::create_dir_all(&remote_tree).expect("the existing remote tree");
  std::fs::write(base.join("tree-owners/project"), owner_record("host-a", &project)).expect("the current owner record");
  let mut env = fake_build_home_env(&sandbox);
  env.insert("FAKE_STOP_FAIL".to_string(), "1".to_string());

  let run = run_runner(&env, &project, &["echo", "remote"]);

  assert_eq!(run.code, 23, "{}", run.describe());
  assert!(run.stderr.contains("failed to stop processes in build host tree project"), "{}", run.describe());
  assert!(remote_tree.exists(), "the tree was removed after its stop failed: {}", run.describe());
  assert!(!ssh_log(&sandbox).lines().any(|line| line.starts_with("rsync ") && line.contains("trees/project/")));
}

/// The local work guard's refusal for `tool`, with the environment `env`.
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

fn spawn_remote_exec(env: &Env, payload: &str) -> std::process::Child {
  let mut command = Command::new("perl");
  command.arg(remote_exec_pl()).arg(payload);
  command.env_clear();
  command.envs(env);
  command.stdout(Stdio::piped()).stderr(Stdio::piped());
  command.spawn().expect("remote-exec starts")
}

fn wait_for_pid_file(path: &Path) -> u32 {
  let deadline = Instant::now() + Duration::from_secs(5);
  while !path.exists() {
    assert!(Instant::now() < deadline, "the command did not write its child pid");
    thread::sleep(Duration::from_millis(10));
  }
  fs::read_to_string(path).expect("read the child pid").trim().parse().expect("the child pid is numeric")
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
fn a_signal_to_remote_exec_stops_the_command_tree() {
  let sandbox = Sandbox::new();
  install_remote_state(&sandbox);
  let env = remote_exec_env(&sandbox);
  let pid_file = sandbox.root().join("command-child.pid");
  let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
  let encoded = payload(".", "", &["sh", "-c", &command]);
  let mut remote_exec = spawn_remote_exec(&env, &encoded);
  let child_pid = wait_for_pid_file(&pid_file);

  signal::kill(Pid::from_raw(remote_exec.id() as i32), Signal::SIGTERM).expect("signal remote-exec");
  let status = remote_exec.wait().expect("wait for remote-exec");
  let mut stderr = String::new();
  remote_exec.stderr.take().expect("remote-exec stderr").read_to_string(&mut stderr).expect("read stderr");

  assert_eq!(status.code(), Some(143), "{stderr}");
  assert!(!Path::new(&format!("/proc/{child_pid}")).exists(), "the command child survived: {stderr}");
}

#[test]
#[ignore = "integration"]
fn a_closed_ssh_output_channel_stops_the_command_tree() {
  let sandbox = Sandbox::new();
  install_remote_state(&sandbox);
  let env = remote_exec_env(&sandbox);
  let pid_file = sandbox.root().join("command-child.pid");
  let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
  let encoded = payload(".", "", &["sh", "-c", &command]);
  let mut remote_exec = spawn_remote_exec(&env, &encoded);
  let child_pid = wait_for_pid_file(&pid_file);
  drop(remote_exec.stdout.take());

  let status = remote_exec.wait().expect("wait for remote-exec after SSH loss");
  let mut stderr = String::new();
  remote_exec.stderr.take().expect("remote-exec stderr").read_to_string(&mut stderr).expect("read stderr");

  assert_eq!(status.code(), Some(143), "{stderr}");
  assert!(!Path::new(&format!("/proc/{child_pid}")).exists(), "the command child survived SSH loss: {stderr}");
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
