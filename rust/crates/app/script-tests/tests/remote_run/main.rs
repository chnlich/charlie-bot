//! The remote runner: `dev/remote-run/run.sh` reads the build host and its state base from `config.yaml` on each
//! run. It removes a tree once its last use is 3 days old by default; `REMOTE_RUN_TREE_RETENTION_DAYS` sets the
//! age, and `run.sh --dry-run` lists selections without deleting them. It stops with exit 2 while the host is
//! unset. It starts every run with a connectivity check; a Tailscale login link exits 86, other failures and the
//! time limit exit 87. The exit 86 stop text names the `/host-auth` page. After the check, the command's exit code
//! comes back unchanged, 255 included. Owner records identify the caller host and local root. A tree whose current
//! record names a gone root is removed at once; every other tree, including one with a path-only legacy record or
//! no record, follows the age rule. The age is the last-use record, or the content age of a tree that has none.
//! Every removal checks the tree lock, the cwd, exe, open fds and command lines of live processes, and Git dirt when
//! the tree has `.git`. A pre-sync replacement checks the tree lock and process references. Neither stops a process. For
//! commands, the runner uses a short TMPDIR on the `/dev/shm` tmpfs, stops descendants on exit or signal, and
//! detects a closed SSH output channel. The local work guard's refusal names the same host and prints in full
//! while the host is unset.

use std::fs;
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

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
  *--check-tree*)
    if [[ -n "${FAKE_SYNC_CHECK_FAIL:-}" ]]; then
      echo 'remote-exec: cannot check the build tree before replacing it' >&2
      exit 23
    fi
    if [[ -n "${FAKE_SYNC_CHECK_SUCCESS:-}" ]]; then
      echo ready
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

fn set_tree_old(tree: &Path) {
  fn collect(path: &Path, paths: &mut Vec<PathBuf>) {
    paths.push(path.to_path_buf());
    if path.is_dir() {
      for entry in fs::read_dir(path).expect("read tree entry") {
        collect(&entry.expect("read tree entry").path(), paths);
      }
    }
  }

  let mut paths = Vec::new();
  collect(tree, &mut paths);
  for path in paths.into_iter().rev() {
    fs::File::open(&path)
      .expect("open tree entry to set its age")
      .set_modified(SystemTime::UNIX_EPOCH)
      .expect("set old tree entry mtime");
  }
}

/// A Git repository in `tree` with one commit of its files. Auto maintenance is off. Otherwise `git commit` starts
/// `git maintenance run --auto`, and a Git that has `maintenance.autoDetach` (default true) runs it in a detached
/// process. That process removes `.git/objects/maintenance.lock` after the commit returns, which sets the mtime of
/// `.git/objects` to the present. A removal after `set_tree_old` makes the sweep judge the tree young.
fn make_git_repo(tree: &Path) {
  for args in [
    vec!["init", "-q"],
    vec!["config", "user.name", "remote-run-test"],
    vec!["config", "user.email", "test@example.invalid"],
    vec!["config", "maintenance.auto", "false"],
  ] {
    let result = Command::new("git").current_dir(tree).args(args).output().expect("run git setup");
    assert!(result.status.success(), "git setup failed: {}", String::from_utf8_lossy(&result.stderr));
  }
  let add = Command::new("git").current_dir(tree).args(["add", "-A"]).output().expect("stage tree files");
  assert!(add.status.success(), "git add failed: {}", String::from_utf8_lossy(&add.stderr));
  let commit =
    Command::new("git").current_dir(tree).args(["commit", "-q", "-m", "initial"]).output().expect("commit tree files");
  assert!(commit.status.success(), "git commit failed: {}", String::from_utf8_lossy(&commit.stderr));
}

fn install_sweep_tree(sandbox: &Sandbox, name: &str, record: &str) -> (PathBuf, Env) {
  let base = sandbox.root().join("scripts/remote-run");
  let tree = base.join("trees").join(name);
  fs::create_dir_all(&tree).expect("create tree for sweep test");
  fs::create_dir_all(base.join("tree-owners")).expect("create owner records for sweep test");
  fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).expect("make sweep test base private");
  fs::write(tree.join("content.txt"), "tree contents\n").expect("write sweep test content");
  fs::write(base.join("tree-owners").join(name), record).expect("write sweep test owner record");
  (tree, remote_exec_env(sandbox))
}

fn sweep_tree(env: &Env, tree: &Path, root_state: &str, retention_days: &str, dry_run: bool) -> harness::Run {
  let mut command = Command::new("perl");
  command
    .arg(remote_exec_pl())
    .arg("--sweep-tree")
    .arg("host-a")
    .arg(tree)
    .arg(root_state)
    .arg(retention_days)
    .arg(if dry_run { "1" } else { "0" });
  command.env_clear();
  command.envs(env);
  harness::run(&mut command)
}

fn check_tree_before_sync(env: &Env, tree: &Path, encoded_root: &str) -> harness::Run {
  let mut command = Command::new("perl");
  command.arg(remote_exec_pl()).arg("--check-tree").arg("host-a").arg(tree).arg(encoded_root);
  command.env_clear();
  command.envs(env);
  harness::run(&mut command)
}

/// Holds the activity lock of `tree`, as a running sync or run does, until the returned file is dropped.
fn hold_tree_activity_lock(tree: &Path) -> fs::File {
  let name = tree.file_name().expect("the tree has a name").to_string_lossy().into_owned();
  let lock_dir = tree.parent().unwrap().parent().unwrap().join("tree-locks");
  fs::create_dir_all(&lock_dir).expect("create tree lock directory");
  let lock = fs::OpenOptions::new()
    .read(true)
    .write(true)
    .create(true)
    .truncate(false)
    .open(lock_dir.join(format!("{name}.lock")))
    .expect("open tree activity lock");
  assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0, "lock tree activity");
  lock
}

/// Writes a tree's last-use record, the epoch second the runner last synced or ran in it.
fn write_last_use(sandbox: &Sandbox, name: &str, epoch: u64) {
  let dir = sandbox.root().join("scripts/remote-run/tree-last-use");
  fs::create_dir_all(&dir).expect("create the last-use directory");
  fs::write(dir.join(name), format!("{epoch}\n")).expect("write the last-use record");
}

fn now_epoch() -> u64 {
  SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("the clock is after 1970").as_secs()
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

/// A build host holds a tree named like the sync root, and its owner record names a local directory that no longer
/// exists. The run sweeps that tree, then syncs.
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
fn an_aged_legacy_owner_record_uses_the_age_selection() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let remote_tree = base.join("trees/legacy-tree");
  std::fs::create_dir_all(&remote_tree).expect("the legacy tree");
  let legacy_record = sandbox.dir("legacy-root");
  std::fs::write(base.join("tree-owners/legacy-tree"), format!("{}\n", legacy_record.display()))
    .expect("the legacy owner record");
  set_tree_old(&remote_tree);
  let mut env = fake_build_home_env(&sandbox);
  env.insert("REMOTE_RUN_TREE_RETENTION_DAYS".to_string(), "1".to_string());

  let run = run_runner(&env, &project, &["echo", "remote"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(
    run.stderr.contains("sweep: removed build host tree legacy-tree because it was last used more than 1 days ago"),
    "{}",
    run.describe()
  );
  assert!(!remote_tree.exists(), "the aged legacy tree survived the sweep: {}", run.describe());
  assert!(
    !base.join("tree-owners/legacy-tree").exists(),
    "the legacy owner record survived the sweep: {}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn the_runner_dry_run_reports_aged_trees_without_deleting_them() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let remote_tree = base.join("trees/dry-run-legacy");
  fs::create_dir_all(&remote_tree).expect("create legacy dry-run tree");
  fs::write(remote_tree.join("old.txt"), "old\n").expect("write old tree content");
  set_tree_old(&remote_tree);
  let local_root = sandbox.dir("legacy-root");
  let legacy_record = format!("{}\n", local_root.display());
  fs::write(base.join("tree-owners/dry-run-legacy"), &legacy_record).expect("write legacy owner record");
  let mut env = fake_build_home_env(&sandbox);
  env.insert("REMOTE_RUN_TREE_RETENTION_DAYS".to_string(), "1".to_string());

  let run = run_runner(&env, &project, &["--dry-run"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.contains("dry-run: selected build host tree dry-run-legacy"), "{}", run.describe());
  assert!(run.stdout.contains("allocated bytes"), "the selection omitted allocated size: {}", run.describe());
  assert!(run.stdout.contains("dry-run: selected 1 trees,"), "{}", run.describe());
  assert!(remote_tree.exists(), "the runner dry run removed the tree: {}", run.describe());
  assert_eq!(fs::read_to_string(base.join("tree-owners/dry-run-legacy")).unwrap(), legacy_record);
  assert!(
    !ssh_log(&sandbox).lines().any(|line| line.starts_with("rsync ") && line.contains("trees/dry-run-legacy/")),
    "the dry run uploaded to a tree: {}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn the_runner_reports_and_keeps_a_dirty_old_tree() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let remote_tree = base.join("trees/dirty-old");
  fs::create_dir_all(&remote_tree).expect("create old dirty tree");
  fs::write(remote_tree.join("content.txt"), "initial\n").expect("write Git content");
  make_git_repo(&remote_tree);
  fs::write(remote_tree.join("content.txt"), "uncommitted\n").expect("make remote tree dirty");
  set_tree_old(&remote_tree);
  let local_root = sandbox.dir("dirty-owner");
  fs::write(base.join("tree-owners/dirty-old"), owner_record("host-a", &local_root))
    .expect("write dirty tree owner record");
  let mut env = fake_build_home_env(&sandbox);
  env.insert("REMOTE_RUN_TREE_RETENTION_DAYS".to_string(), "1".to_string());

  let run = run_runner(&env, &project, &["echo", "remote"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(
    run.stderr.contains("sweep: kept build host tree dirty-old because it has uncommitted changes"),
    "{}",
    run.describe()
  );
  assert!(remote_tree.exists(), "the dirty tree was removed: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn an_old_clean_tree_is_removed_and_a_recent_tree_is_kept() {
  let sandbox = Sandbox::new();
  let root = sandbox.dir("owner-root");
  let old_record = owner_record("host-a", &root);
  let (old_tree, env) = install_sweep_tree(&sandbox, "old-tree", &old_record);
  set_tree_old(&old_tree);
  let old_run = sweep_tree(&env, &old_tree, "live", "1", false);
  assert_eq!(old_run.code, 0, "{}", old_run.describe());
  assert!(old_run.stdout.starts_with("removed-age:"), "{}", old_run.describe());
  assert!(!old_tree.exists(), "the old clean tree survived: {}", old_run.describe());
  assert!(!old_tree.parent().unwrap().parent().unwrap().join("tree-owners/old-tree").exists());

  let (recent_tree, recent_env) = install_sweep_tree(&sandbox, "recent-tree", &old_record);
  let recent_run = sweep_tree(&recent_env, &recent_tree, "live", "1", false);
  assert_eq!(recent_run.code, 0, "{}", recent_run.describe());
  assert_eq!(recent_run.stdout.trim(), "kept-young", "{}", recent_run.describe());
  assert!(recent_tree.exists(), "the recent tree was removed: {}", recent_run.describe());
}

#[test]
#[ignore = "integration"]
fn a_dirty_old_tree_is_reported_and_kept() {
  let sandbox = Sandbox::new();
  let root = sandbox.dir("owner-root");
  let (tree, env) = install_sweep_tree(&sandbox, "dirty-tree", &owner_record("host-a", &root));
  make_git_repo(&tree);
  fs::write(tree.join("content.txt"), "uncommitted change\n").expect("make the tree dirty");
  set_tree_old(&tree);

  let run = sweep_tree(&env, &tree, "live", "1", false);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(run.stdout.trim(), "kept-dirty", "{}", run.describe());
  assert!(tree.exists(), "the dirty tree was removed: {}", run.describe());
  assert!(tree.parent().unwrap().parent().unwrap().join("tree-owners/dirty-tree").exists());
}

#[test]
#[ignore = "integration"]
fn a_dry_run_reports_an_old_tree_without_deleting_it_or_its_record() {
  let sandbox = Sandbox::new();
  let root = sandbox.dir("owner-root");
  let (tree, env) = install_sweep_tree(&sandbox, "dry-tree", &owner_record("host-a", &root));
  set_tree_old(&tree);

  let run = sweep_tree(&env, &tree, "live", "1", true);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.starts_with("would-remove-age:"), "{}", run.describe());
  assert!(tree.exists(), "the dry run removed the tree: {}", run.describe());
  assert!(tree.parent().unwrap().parent().unwrap().join("tree-owners/dry-tree").exists());
}

#[test]
#[ignore = "integration"]
fn a_held_tree_activity_lock_keeps_an_old_tree() {
  let sandbox = Sandbox::new();
  let root = sandbox.dir("owner-root");
  let (tree, env) = install_sweep_tree(&sandbox, "locked-tree", &owner_record("host-a", &root));
  set_tree_old(&tree);
  let lock = hold_tree_activity_lock(&tree);

  let run = sweep_tree(&env, &tree, "live", "1", false);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(run.stdout.trim(), "kept-locked", "{}", run.describe());
  assert!(tree.exists(), "the tree with a held activity lock was removed: {}", run.describe());
  assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0, "unlock tree activity");
}

#[test]
#[ignore = "integration"]
fn a_pre_sync_check_keeps_a_tree_with_a_held_activity_lock() {
  let sandbox = Sandbox::new();
  let (tree, env) = install_sweep_tree(&sandbox, "sync-locked", &owner_record("host-a", Path::new("/")));
  let _lock = hold_tree_activity_lock(&tree);

  let run = check_tree_before_sync(&env, &tree, "Lw==");

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(run.stdout.trim(), "kept-locked", "{}", run.describe());
  assert!(tree.exists(), "the pre-sync check removed the tree: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pre_sync_check_keeps_a_tree_used_by_a_live_process() {
  let sandbox = Sandbox::new();
  let (tree, env) = install_sweep_tree(&sandbox, "sync-busy", &owner_record("host-a", Path::new("/")));
  let mut process = Command::new("sleep").arg("30").current_dir(&tree).spawn().expect("start tree process");
  let pid = process.id();

  let run = check_tree_before_sync(&env, &tree, "Lw==");
  let remains_alive = process.try_wait().expect("check the tree process").is_none();
  process.kill().expect("stop the test process");
  process.wait().expect("wait for the test process");

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(run.stdout.trim(), format!("kept-in-use:{pid}:cwd"), "{}", run.describe());
  assert!(remains_alive, "the pre-sync check stopped the process: {}", run.describe());
  assert!(tree.exists(), "the pre-sync check removed the tree: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn an_open_file_descriptor_keeps_an_old_tree() {
  let sandbox = Sandbox::new();
  let root = sandbox.dir("owner-root");
  let (tree, env) = install_sweep_tree(&sandbox, "fd-tree", &owner_record("host-a", &root));
  set_tree_old(&tree);
  let file = fs::File::open(tree.join("content.txt")).expect("open a tree file");

  let run = sweep_tree(&env, &tree, "live", "1", false);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.contains(":fd/"), "{}", run.describe());
  assert!(tree.exists(), "the tree with an open fd was removed: {}", run.describe());
  drop(file);
}

#[test]
#[ignore = "integration"]
fn a_process_working_directory_keeps_an_old_tree() {
  let sandbox = Sandbox::new();
  let root = sandbox.dir("owner-root");
  let (tree, env) = install_sweep_tree(&sandbox, "cwd-tree", &owner_record("host-a", &root));
  set_tree_old(&tree);
  let mut process = Command::new("sleep").arg("30").current_dir(&tree).spawn().expect("start tree process");
  let pid = process.id();

  let run = sweep_tree(&env, &tree, "live", "1", false);
  let remains_alive = process.try_wait().expect("check the tree process").is_none();
  process.kill().expect("stop the tree process after the sweep");
  process.wait().expect("wait for the tree process");

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.contains(&format!("kept-in-use:{pid}:cwd")), "{}", run.describe());
  assert!(remains_alive, "the sweep stopped the process: {}", run.describe());
  assert!(tree.exists(), "the tree with a live cwd was removed: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_process_executable_inside_an_old_tree_keeps_it() {
  let sandbox = Sandbox::new();
  let root = sandbox.dir("owner-root");
  let (tree, env) = install_sweep_tree(&sandbox, "exe-tree", &owner_record("host-a", &root));
  let executable = tree.join("sleeper");
  fs::copy("/bin/sleep", &executable).expect("copy a process executable into the tree");
  set_tree_old(&tree);
  let mut process = Command::new("/bin/bash")
    .arg("-c")
    .arg("exec -a sleeper \"$1\" 30")
    .arg("bash")
    .arg(&executable)
    .arg0("sleeper")
    .current_dir(sandbox.root())
    .spawn()
    .expect("start process from the tree executable");
  let pid = process.id();
  let deadline = Instant::now() + Duration::from_secs(5);
  while fs::read_link(format!("/proc/{pid}/exe")).ok().as_deref() != Some(executable.as_path()) {
    assert!(Instant::now() < deadline, "the process did not start from {}", executable.display());
    thread::sleep(Duration::from_millis(5));
  }

  let run = sweep_tree(&env, &tree, "live", "1", false);
  let remains_alive = process.try_wait().expect("check the tree process").is_none();
  process.kill().expect("stop the tree process after the sweep");
  process.wait().expect("wait for the tree process");

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.contains(&format!("kept-in-use:{pid}:exe")), "{}", run.describe());
  assert!(remains_alive, "the sweep stopped the process: {}", run.describe());
  assert!(tree.exists(), "the tree with a live executable was removed: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_process_command_line_naming_an_old_tree_keeps_it() {
  let sandbox = Sandbox::new();
  let root = sandbox.dir("owner-root");
  let (tree, env) = install_sweep_tree(&sandbox, "cmdline-tree", &owner_record("host-a", &root));
  set_tree_old(&tree);
  let mut process = Command::new("perl")
    .arg("-e")
    .arg("sleep 30")
    .arg(&tree)
    .current_dir(sandbox.root())
    .spawn()
    .expect("start a process whose command line names the tree");
  let pid = process.id();

  let run = sweep_tree(&env, &tree, "live", "1", false);
  let remains_alive = process.try_wait().expect("check the tree process").is_none();
  process.kill().expect("stop the tree process after the sweep");
  process.wait().expect("wait for the tree process");

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.contains(&format!("kept-in-use:{pid}:cmdline")), "{}", run.describe());
  assert!(remains_alive, "the sweep stopped the process: {}", run.describe());
  assert!(tree.exists(), "the tree named by a live command line was removed: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_running_remote_driver_holds_its_tree_lock_and_names_its_tree() {
  let sandbox = Sandbox::new();
  install_remote_state(&sandbox);
  let env = remote_exec_env(&sandbox);
  let tree = sandbox.root().join("scripts/remote-run/trees/proj");
  let owner_root = sandbox.dir("owner-root");
  fs::create_dir_all(sandbox.root().join("scripts/remote-run/tree-owners")).expect("the owner-record directory");
  fs::write(sandbox.root().join("scripts/remote-run/tree-owners/proj"), owner_record("host-a", &owner_root))
    .expect("write the driver owner record");
  fs::write(tree.join("content.txt"), "tree\n").expect("write tree content");
  set_tree_old(&tree);
  let pid_file = sandbox.root().join("driver-child.pid");
  let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
  let encoded = payload(".", "", &["sh", "-c", &command]);
  let mut driver = spawn_remote_exec(&env, &encoded);
  let child_pid = wait_for_pid_file(&pid_file);
  let driver_cmdline = fs::read(format!("/proc/{}/cmdline", driver.id())).expect("read driver command line");
  assert!(
    driver_cmdline
      .windows(tree.as_os_str().as_encoded_bytes().len())
      .any(|part| part == tree.as_os_str().as_encoded_bytes())
  );

  let run = sweep_tree(&env, &tree, "live", "1", false);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(run.stdout.trim(), "kept-locked", "{}", run.describe());
  assert!(tree.exists(), "the active driver's tree was removed: {}", run.describe());
  signal::kill(Pid::from_raw(driver.id() as i32), Signal::SIGTERM).expect("stop remote driver");
  let status = driver.wait().expect("wait for remote driver");
  assert_eq!(status.code(), Some(143));
  assert!(!Path::new(&format!("/proc/{child_pid}")).exists(), "the driver's child survived");
}

/// A gone-root sweep keeps a tree whose driver is running: the driver holds the tree's activity lock, and the sweep
/// neither stops the driver nor its child nor removes the tree.
#[test]
#[ignore = "integration"]
fn a_gone_root_sweep_keeps_a_running_driver_and_its_tree() {
  let sandbox = Sandbox::new();
  install_remote_state(&sandbox);
  let env = remote_exec_env(&sandbox);
  let tree = sandbox.root().join("scripts/remote-run/trees/proj");
  let gone_root = sandbox.root().join("gone-owner-root");
  fs::create_dir_all(sandbox.root().join("scripts/remote-run/tree-owners")).expect("the owner-record directory");
  fs::write(sandbox.root().join("scripts/remote-run/tree-owners/proj"), owner_record("host-a", &gone_root))
    .expect("write the driver owner record");
  fs::write(tree.join("content.txt"), "tree\n").expect("write tree content");
  let pid_file = sandbox.root().join("driver-child.pid");
  let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
  let encoded = payload(".", "", &["sh", "-c", &command]);
  let mut driver = spawn_remote_exec(&env, &encoded);
  let child_pid = wait_for_pid_file(&pid_file);

  let run = sweep_tree(&env, &tree, "gone", "1", false);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(run.stdout.trim(), "kept-locked", "{}", run.describe());
  assert!(tree.exists(), "the gone-root tree with a running driver was removed: {}", run.describe());
  assert!(driver.try_wait().expect("check the driver").is_none(), "the sweep stopped the driver");
  assert!(Path::new(&format!("/proc/{child_pid}")).exists(), "the sweep stopped the driver's child");
  signal::kill(Pid::from_raw(driver.id() as i32), Signal::SIGTERM).expect("stop remote driver");
  driver.wait().expect("wait for remote driver");
}

/// A gone root removes nothing while the tree's activity lock is held. The process running in the tree keeps running.
#[test]
#[ignore = "integration"]
fn a_gone_root_tree_with_a_held_lock_keeps_the_tree_and_its_process() {
  let sandbox = Sandbox::new();
  let gone_root = sandbox.root().join("gone-owner");
  let (tree, env) = install_sweep_tree(&sandbox, "gone-locked", &owner_record("host-a", &gone_root));
  let _lock = hold_tree_activity_lock(&tree);
  let mut process = Command::new("sleep").arg("30").current_dir(&tree).spawn().expect("start the process in the tree");

  let run = sweep_tree(&env, &tree, "gone", "3", false);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(run.stdout.trim(), "kept-locked", "{}", run.describe());
  assert!(tree.exists(), "the gone-root tree with a held lock was removed: {}", run.describe());
  assert!(
    process.try_wait().expect("check the process").is_none(),
    "the sweep stopped the process: {}",
    run.describe()
  );
  process.kill().expect("stop the test process");
  process.wait().expect("wait for the test process");
}

/// A gone root removes nothing while a process runs in the tree. The sweep names the process and neither stops it nor
/// removes the tree.
#[test]
#[ignore = "integration"]
fn a_gone_root_tree_with_a_live_process_is_kept_and_the_process_survives() {
  let sandbox = Sandbox::new();
  let gone_root = sandbox.root().join("gone-owner");
  let (tree, env) = install_sweep_tree(&sandbox, "gone-busy", &owner_record("host-a", &gone_root));
  let mut process = Command::new("sleep").arg("30").current_dir(&tree).spawn().expect("start the process in the tree");
  let pid = process.id();

  let run = sweep_tree(&env, &tree, "gone", "3", false);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(run.stdout.trim(), format!("kept-in-use:{pid}:cwd"), "{}", run.describe());
  assert!(tree.exists(), "the gone-root tree with a live process was removed: {}", run.describe());
  assert!(
    process.try_wait().expect("check the process").is_none(),
    "the sweep stopped the process: {}",
    run.describe()
  );
  process.kill().expect("stop the test process");
  process.wait().expect("wait for the test process");
}

/// A path-only legacy record proves no owner, so its gone root never removes the tree: the tree follows the age rule.
#[test]
#[ignore = "integration"]
fn a_legacy_record_whose_root_is_gone_takes_only_the_age_rule() {
  let sandbox = Sandbox::new();
  let legacy = format!("{}\n", sandbox.root().join("gone-legacy-root").display());
  let (tree, env) = install_sweep_tree(&sandbox, "legacy-gone", &legacy);

  let young = sweep_tree(&env, &tree, "gone", "3", false);

  assert_eq!(young.code, 0, "{}", young.describe());
  assert_eq!(young.stdout.trim(), "kept-young", "{}", young.describe());
  assert!(tree.exists(), "the legacy tree with a gone root was removed: {}", young.describe());

  write_last_use(&sandbox, "legacy-gone", 0);
  let aged = sweep_tree(&env, &tree, "gone", "3", false);

  assert_eq!(aged.code, 0, "{}", aged.describe());
  assert!(aged.stdout.starts_with("removed-age:"), "{}", aged.describe());
  assert!(!tree.exists(), "the aged legacy tree survived: {}", aged.describe());
}

/// A tree without an owner record has no record to drop, and the age rule selects it like any other.
#[test]
#[ignore = "integration"]
fn a_tree_without_an_owner_record_is_removed_by_the_age_rule() {
  let sandbox = Sandbox::new();
  let (tree, env) = install_sweep_tree(&sandbox, "recordless", &owner_record("host-a", &sandbox.dir("owner-root")));
  fs::remove_file(sandbox.root().join("scripts/remote-run/tree-owners/recordless")).expect("remove the owner record");
  set_tree_old(&tree);

  let dry = sweep_tree(&env, &tree, "live", "1", true);

  assert_eq!(dry.code, 0, "{}", dry.describe());
  assert!(dry.stdout.starts_with("would-remove-age:"), "{}", dry.describe());
  assert!(tree.exists(), "the dry run removed the tree: {}", dry.describe());

  let run = sweep_tree(&env, &tree, "live", "1", false);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.starts_with("removed-age:"), "{}", run.describe());
  assert!(!tree.exists(), "the aged tree without a record survived: {}", run.describe());
}

/// The last-use record sets a tree's age, whatever its content's modification time says.
#[test]
#[ignore = "integration"]
fn a_last_use_record_sets_the_age_instead_of_the_content_mtime() {
  let sandbox = Sandbox::new();
  let record = owner_record("host-a", &sandbox.dir("owner-root"));
  let (used_recently, env) = install_sweep_tree(&sandbox, "used-recently", &record);
  set_tree_old(&used_recently);
  write_last_use(&sandbox, "used-recently", now_epoch());
  let (used_long_ago, _) = install_sweep_tree(&sandbox, "used-long-ago", &record);
  write_last_use(&sandbox, "used-long-ago", 0);

  let young = sweep_tree(&env, &used_recently, "live", "1", false);

  assert_eq!(young.code, 0, "{}", young.describe());
  assert_eq!(young.stdout.trim(), "kept-young", "{}", young.describe());
  assert!(used_recently.exists(), "the recently used tree was removed: {}", young.describe());

  let aged = sweep_tree(&env, &used_long_ago, "live", "1", false);

  assert_eq!(aged.code, 0, "{}", aged.describe());
  assert!(aged.stdout.starts_with("removed-age:"), "{}", aged.describe());
  assert!(!used_long_ago.exists(), "the long-unused tree survived: {}", aged.describe());
}

/// The dry run and the real sweep give each tree the same outcome, and the dry run deletes nothing. Each kind of tree
/// the sweep handles appears once.
#[test]
#[ignore = "integration"]
fn a_dry_run_and_a_real_sweep_select_the_same_trees() {
  let sandbox = Sandbox::new();
  let gone_root = sandbox.root().join("gone-owner");
  let live_root = sandbox.dir("owner-root");
  let legacy = format!("{}\n", sandbox.root().join("gone-legacy-root").display());

  let (gone_clean, env) = install_sweep_tree(&sandbox, "gone-clean", &owner_record("host-a", &gone_root));
  let (gone_locked, _) = install_sweep_tree(&sandbox, "gone-locked", &owner_record("host-a", &gone_root));
  let _lock = hold_tree_activity_lock(&gone_locked);
  let (aged_clean, _) = install_sweep_tree(&sandbox, "aged-clean", &owner_record("host-a", &live_root));
  write_last_use(&sandbox, "aged-clean", 0);
  let (young, _) = install_sweep_tree(&sandbox, "young", &owner_record("host-a", &live_root));
  write_last_use(&sandbox, "young", now_epoch());
  let (aged_dirty, _) = install_sweep_tree(&sandbox, "aged-dirty", &owner_record("host-a", &live_root));
  make_git_repo(&aged_dirty);
  fs::write(aged_dirty.join("content.txt"), "uncommitted change\n").expect("make the tree dirty");
  write_last_use(&sandbox, "aged-dirty", 0);
  let (legacy_aged, _) = install_sweep_tree(&sandbox, "legacy-aged", &legacy);
  set_tree_old(&legacy_aged);
  let (recordless, _) = install_sweep_tree(&sandbox, "recordless", &owner_record("host-a", &live_root));
  fs::remove_file(sandbox.root().join("scripts/remote-run/tree-owners/recordless")).expect("remove the owner record");
  set_tree_old(&recordless);

  // Each case: the tree, its root state, and its dry-run outcome.
  let cases = [
    (&gone_clean, "gone"),
    (&gone_locked, "gone"),
    (&aged_clean, "live"),
    (&young, "live"),
    (&aged_dirty, "live"),
    (&legacy_aged, "live"),
    (&recordless, "live"),
  ];
  let dry_outcomes: Vec<String> = cases
    .iter()
    .map(|(tree, root_state)| {
      let run = sweep_tree(&env, tree, root_state, "1", true);
      assert_eq!(run.code, 0, "{}", run.describe());
      run.stdout.trim().to_string()
    })
    .collect();
  for (tree, _) in &cases {
    assert!(tree.exists(), "the dry run removed {}", tree.display());
  }

  let real_outcomes: Vec<String> = cases
    .iter()
    .map(|(tree, root_state)| {
      let run = sweep_tree(&env, tree, root_state, "1", false);
      assert_eq!(run.code, 0, "{}", run.describe());
      run.stdout.trim().to_string()
    })
    .collect();
  let expected = [
    "would-remove-gone:",
    "kept-locked",
    "would-remove-age:",
    "kept-young",
    "kept-dirty",
    "would-remove-age:",
    "would-remove-age:",
  ];
  for (index, dry) in dry_outcomes.iter().enumerate() {
    let real = &real_outcomes[index];
    assert!(dry.starts_with(expected[index]), "case {index} dry-run outcome {dry}");
    if dry.starts_with("would-remove-") {
      // A gone root's real removal reports no byte count, so the two runs agree on the outcome's kind.
      let dry_kind = dry.split(':').next().unwrap_or_default().replace("would-remove-", "removed-");
      let real_kind = real.split(':').next().unwrap_or_default();
      assert_eq!(real_kind, dry_kind, "case {index}: the real sweep differs from the dry run");
    } else {
      assert_eq!(real, dry, "case {index}: the real sweep differs from the dry run");
    }
  }
}

/// A sync that finds its own tree without a record reclaims it: the sweep removes the aged tree, and the claim writes
/// the owner record and the last-use record.
#[test]
#[ignore = "integration"]
fn a_record_less_aged_tree_of_the_current_name_is_removed_and_the_run_succeeds() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let remote_tree = base.join("trees/project");
  std::fs::create_dir_all(&remote_tree).expect("the record-less tree");
  std::fs::write(remote_tree.join("leftover.txt"), "stale\n").expect("the record-less tree file");
  set_tree_old(&remote_tree);
  let mut env = fake_build_home_env(&sandbox);
  env.insert("REMOTE_RUN_TREE_RETENTION_DAYS".to_string(), "1".to_string());

  let run = run_runner(&env, &project, &["echo", "remote"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(
    run.stderr.contains("sweep: removed build host tree project because it was last used more than 1 days ago"),
    "{}",
    run.describe()
  );
  assert!(!base.join("trees/project/leftover.txt").exists(), "the record-less tree survived: {}", run.describe());
  let record = fs::read_to_string(base.join("tree-owners/project")).expect("the reclaimed owner record");
  assert_eq!(record, owner_record("host-a", &project), "{}", run.describe());
  let last_use = fs::read_to_string(base.join("tree-last-use/project")).expect("the last-use record");
  assert!(last_use.trim().parse::<u64>().expect("the last-use epoch") + 60 >= now_epoch(), "{}", run.describe());
}

/// A tree whose last-use record is young gets no sweep call from the sync: the record alone keeps it. An aged tree of
/// the same run still goes to the build host and is removed.
#[test]
#[ignore = "integration"]
fn a_young_last_use_record_keeps_its_tree_without_a_build_host_sweep() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  let young_root = sandbox.dir("young-owner");
  let aged_root = sandbox.dir("aged-owner");
  for (name, root) in [("young-tree", &young_root), ("aged-tree", &aged_root)] {
    fs::create_dir_all(base.join("trees").join(name)).expect("the tree");
    fs::write(base.join("tree-owners").join(name), owner_record("host-a", root)).expect("the owner record");
  }
  fs::create_dir_all(base.join("tree-last-use")).expect("the last-use directory");
  fs::write(base.join("tree-last-use/young-tree"), format!("{}\n", now_epoch())).expect("the young record");
  fs::write(base.join("tree-last-use/aged-tree"), "0\n").expect("the aged record");
  let mut env = fake_build_home_env(&sandbox);
  env.insert("REMOTE_RUN_TREE_RETENTION_DAYS".to_string(), "1".to_string());

  let run = run_runner(&env, &project, &["echo", "remote"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  let log = ssh_log(&sandbox);
  assert!(
    !log.lines().any(|line| line.contains("--sweep-tree") && line.contains("young-tree")),
    "the young tree had a build host sweep call: {log}"
  );
  assert!(
    log.lines().any(|line| line.contains("--sweep-tree") && line.contains("aged-tree")),
    "the aged tree had no build host sweep call: {log}"
  );
  assert!(!base.join("trees/aged-tree").exists(), "the aged tree survived: {}", run.describe());
  assert!(base.join("trees/young-tree").is_dir(), "the young tree was removed: {}", run.describe());
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

/// A current-format record for another host stays untouched. A path-only legacy record with no tree is dropped.
#[test]
#[ignore = "integration"]
fn a_sweep_keeps_foreign_records_and_drops_missing_legacy_trees() {
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
  assert!(
    !base.join("tree-owners/legacy-stale").exists(),
    "the missing legacy tree's record survived: {}",
    run.describe()
  );
  assert!(run.stderr.contains("dropped the owner record of build host tree legacy-stale"), "{}", run.describe());
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
  assert!(
    ssh_log(&sandbox)
      .contains("--rsync-path=flock -n -x -- \"$HOME/scripts/remote-run/tree-locks/project.lock\" rsync"),
    "the upload did not use the claimed tree's activity lock: {}",
    run.describe()
  );
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

/// An upload checks that no process or run holds the tree first.
#[test]
#[ignore = "integration"]
fn a_sync_checks_the_existing_tree_before_uploading_replacements() {
  let sandbox = Sandbox::new();
  sandbox.stub("ssh", FAKE_SSH);
  sandbox.stub("rsync", FAKE_RSYNC);
  install_cli(&sandbox);
  let project = project(&sandbox);
  let base = install_fake_build_home(&sandbox);
  std::fs::create_dir_all(base.join("trees/project")).expect("the existing remote tree");
  std::fs::write(base.join("tree-owners/project"), owner_record("host-a", &project)).expect("the current owner record");
  let mut env = fake_build_home_env(&sandbox);
  env.insert("FAKE_SYNC_CHECK_SUCCESS".to_string(), "1".to_string());

  let run = run_runner(&env, &project, &["echo", "remote"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  let log = ssh_log(&sandbox);
  let check_index = log.lines().position(|line| line.contains("--check-tree")).expect("tree check call");
  let upload_index =
    log.lines().position(|line| line.starts_with("rsync ") && line.contains("trees/project/")).expect("tree upload");
  assert!(check_index < upload_index, "the upload ran before the tree check: {log}");
  assert!(!log.contains("--stop-tree"), "the sync still invokes a process-stopping mode: {log}");
}

#[test]
#[ignore = "integration"]
fn a_failed_sync_tree_check_keeps_the_tree_and_skips_upload() {
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
  env.insert("FAKE_SYNC_CHECK_FAIL".to_string(), "1".to_string());

  let run = run_runner(&env, &project, &["echo", "remote"]);

  assert_eq!(run.code, 23, "{}", run.describe());
  assert!(run.stderr.contains("failed to check build host tree project before replacing it"), "{}", run.describe());
  assert!(remote_tree.exists(), "the tree was removed after its pre-sync check failed: {}", run.describe());
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
  std::fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).expect("make remote state private");
}

fn remote_exec_env(sandbox: &Sandbox) -> Env {
  let mut env = sandbox.env();
  env.insert("REMOTE_RUN_REMOTE_BASE".to_string(), "scripts/remote-run".to_string());
  env.insert("HOME".to_string(), sandbox.root().display().to_string());
  env
}

fn run_remote_exec(env: &Env, payload: &str) -> harness::Run {
  let tree = PathBuf::from(&env["HOME"]).join("scripts/remote-run/trees/proj");
  let mut command = Command::new("perl");
  command.arg(remote_exec_pl()).arg("--run-tree").arg(tree).arg(payload);
  command.env_clear();
  command.envs(env);
  harness::run(&mut command)
}

fn spawn_remote_exec(env: &Env, payload: &str) -> std::process::Child {
  let tree = PathBuf::from(&env["HOME"]).join("scripts/remote-run/trees/proj");
  let mut command = Command::new("perl");
  command.arg(remote_exec_pl()).arg("--run-tree").arg(tree).arg(payload);
  command.env_clear();
  command.envs(env);
  command.stdout(Stdio::piped()).stderr(Stdio::piped());
  command.spawn().expect("remote-exec starts")
}

/// The child pid the command writes to `path`. The shell creates the file before `echo` writes the pid, so the pid
/// counts as written only once the content ends with the newline that `echo` writes after it.
fn wait_for_pid_file(path: &Path) -> u32 {
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    let content = match fs::read_to_string(path) {
      Ok(content) => content,
      Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
      Err(error) => panic!("read {}: {error}", path.display()),
    };
    if let Some(line) = content.strip_suffix('\n') {
      return line.parse().unwrap_or_else(|error| {
        panic!("{} does not hold a numeric pid: {error}; content: {content:?}", path.display())
      });
    }
    assert!(
      Instant::now() < deadline,
      "{} did not hold a complete child pid line within 5 s; content: {content:?}",
      path.display()
    );
    thread::sleep(Duration::from_millis(10));
  }
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
