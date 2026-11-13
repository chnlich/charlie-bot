//! The release entry point (scripts/release.sh), for real, against a stub gh and a sandboxed user manager.
//!
//! Every parametrized case is its own test. Each
//! test builds a throwaway git repository, release root and log root in a sandbox, so nothing touches
//! ~/charliebot-releases or the live home.

#[path = "../common/release_lock.rs"]
mod release_lock;
mod world;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox};
use release_lock::LockHolder;
use serde_json::json;
use world::World;

// ---- preflight refusals: exit 2 and nothing written ----------------------------------------------------------------

#[test]
#[ignore = "integration"]
fn test_preflight_refuses_an_argument_that_is_not_a_full_sha() {
  let world = World::new();

  let run = world.release("main");

  world.assert_refused_and_wrote_nothing(&run);
  assert!(run.stderr.contains("40-character"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn test_preflight_refuses_when_the_user_manager_does_not_answer_and_names_the_bus_address() {
  let world = World::new();
  world
    .sandbox
    .stub("systemctl", "printf '%s\\t%s\\n' \"$DBUS_SESSION_BUS_ADDRESS\" \"$*\" >> \"$UNIT_CALLS\"; exit 1\n");

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(
    run.stderr.contains(&format!("DBUS_SESSION_BUS_ADDRESS=unix:path={}/bus", world.runtime.display())),
    "{}",
    run.describe()
  );
  assert_eq!(
    recorded_calls(&world.sandbox.root().join("unit-calls.log")),
    format!("unix:path={}/bus\t--user show-environment\n", world.runtime.display()),
    "{}",
    run.describe()
  );
  assert!(!world.releases.join(&world.on_main).exists(), "{}", run.describe());
  assert!(!world.logs.exists(), "the refusal wrote {}", world.logs.display());
  assert!(!world.sandbox.root().join("user-units/charliebot-agents.slice").exists(), "the preflight installed a unit");
}

#[test]
#[ignore = "integration"]
fn test_preflight_refuses_a_commit_that_is_not_on_origin_main() {
  let world = World::new();

  let run = world.release(&world.off_main);

  world.assert_refused_and_wrote_nothing(&run);
  assert!(run.stderr.contains("is not on origin/main"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn test_preflight_refuses_a_commit_whose_release_exists() {
  let world = World::new();
  let existing = world.releases.join(&world.on_main);
  fs::create_dir(&existing).expect("the existing release");

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("already exists"), "{}", run.describe());
  assert_eq!(fs::read_dir(&existing).expect("the existing release").count(), 0);
  assert!(!world.logs.exists(), "the refusal wrote {}", world.logs.display());
}

#[test]
#[ignore = "integration"]
fn test_preflight_refuses_without_gh_authentication() {
  let world = World::new();
  let mut behavior = world.gh_defaults();
  behavior.auth = false;
  world.write_gh(&behavior);

  let run = world.release(&world.on_main);

  world.assert_refused_and_wrote_nothing(&run);
  assert!(run.stderr.contains("gh is not authenticated"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn test_preflight_refuses_a_commit_without_both_rust_jobs_green_no_run_for_the_commit() {
  preflight_refuses_a_commit_without_both_rust_jobs_green("no-run-for-the-commit");
}

#[test]
#[ignore = "integration"]
fn test_preflight_refuses_a_commit_without_both_rust_jobs_green_rust_test_failed() {
  preflight_refuses_a_commit_without_both_rust_jobs_green("rust-test-failed");
}

#[test]
#[ignore = "integration"]
fn test_preflight_refuses_a_commit_without_both_rust_jobs_green_run_failed() {
  preflight_refuses_a_commit_without_both_rust_jobs_green("run-failed");
}

/// The preflight refuses a commit with no successful push run that has both Rust jobs green: no run of the commit, a
/// run whose rust-test job failed, and a failed run.
fn preflight_refuses_a_commit_without_both_rust_jobs_green(case: &str) {
  let world = World::new();
  let mut behavior = world.gh_defaults();
  match case {
    "no-run-for-the-commit" => {
      behavior.runs = json!([{
        "databaseId": 7,
        "event": "push",
        "headSha": "0".repeat(40),
        "conclusion": "success",
      }])
      .to_string();
    }
    "rust-test-failed" => {
      behavior.jobs.insert(
        "7".to_string(),
        json!([
          {"name": "release-build", "conclusion": "success"},
          {"name": "rust-test", "conclusion": "failure"},
        ])
        .to_string(),
      );
    }
    "run-failed" => {
      behavior.runs = json!([{
        "databaseId": 7,
        "event": "push",
        "headSha": world.on_main,
        "conclusion": "failure",
      }])
      .to_string();
    }
    other => panic!("no case named {other}"),
  }
  world.write_gh(&behavior);

  let run = world.release(&world.on_main);

  world.assert_refused_and_wrote_nothing(&run);
  assert!(run.stderr.contains("CI has no successful push run"), "{}", run.describe());
}
// ---- the host release lock -----------------------------------------------------------------------------------------

#[test]
#[ignore = "integration"]
fn test_a_release_that_finds_the_host_lock_held_refuses_at_once_and_names_the_holder() {
  let world = World::new();
  let holder = LockHolder::start(&world.repo.join("scripts/release/lock.sh"), &world.env, "release.sh", &world.on_main);
  let started = Instant::now();

  let run = world.release(&world.on_main);

  let elapsed = started.elapsed();
  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(elapsed < Duration::from_secs(2), "the refusal took {elapsed:?}: {}", run.describe());
  assert!(run.stderr.contains("script=release.sh"), "{}", run.describe());
  assert!(run.stderr.contains(&format!("commit={}", world.on_main)), "{}", run.describe());
  assert!(run.stderr.contains(&format!("pid={}", holder.pid())), "{}", run.describe());
  assert!(run.stderr.contains("started="), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn test_a_holder_that_is_killed_leaves_the_lock_free_for_the_next_release() {
  let world = World::new();
  let mut holder =
    LockHolder::start(&world.repo.join("scripts/release/lock.sh"), &world.env, "release.sh", &world.on_main);
  holder.kill();
  assert!(holder.record().exists(), "the killed holder left no stale record");

  let run = world.release(&world.on_main);

  // The stale record does not block: the release runs every stage and installs.
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.releases.join(&world.on_main).join("release.log").exists(), "{}", run.describe());
}
// ---- a failed stage: exit 1, the stage named, nothing installed -----------------------------------------------------

#[test]
#[ignore = "integration"]
fn test_a_failed_stage_exits_1_names_the_stage_and_installs_nothing() {
  let world = World::new();
  // The units stage reloads the user manager; a refused reload fails it.
  world.sandbox.stub("systemctl", r#"case "$*" in *show-environment*) exit 0 ;; *) exit 1 ;; esac"#);

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("stage units failed"), "{}", run.describe());
  let log = world.stage_log("units.log");
  assert!(run.stderr.contains(&format!("stage units failed: {}", log.display())), "{}", run.describe());
  assert!(!world.releases.join(&world.on_main).exists(), "{}", run.describe());
  assert!(!world.has_temporary_release(), "{}", run.describe());
  assert_eq!(world.current_link(), PathBuf::from("prior"));
}

#[test]
#[ignore = "integration"]
fn test_a_failed_download_stage_exits_1_and_installs_nothing() {
  let world = World::new();
  let mut behavior = world.gh_defaults();
  behavior.download_ok = false;
  world.write_gh(&behavior);

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("stage download failed"), "{}", run.describe());
  assert!(!world.releases.join(&world.on_main).exists(), "{}", run.describe());
}

// ---- the downloaded artifact: a temporary directory the log directory never holds ------------------------------------

#[test]
#[ignore = "integration"]
fn test_a_passing_run_keeps_no_binary_in_the_log_directory_and_removes_the_artifact_directory() {
  let world = World::new();

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 0, "{}", run.describe());
  // The log directory keeps the stage logs and the artifact's checksums, and no binary.
  let names = world.log_names();
  for name in ["download.log", "units.log", "install.log", "sha256sums.txt"] {
    assert!(names.contains(&name.to_string()), "the log directory lacks {name}: {names:?}");
  }
  for name in ["charliebot", "charliebot-server", "artifact"] {
    assert!(!names.contains(&name.to_string()), "the log directory holds {name}: {names:?}");
  }
  let copied = fs::read_to_string(world.stage_log("sha256sums.txt")).expect("the copied checksums");
  assert_eq!(copied, fs::read_to_string(world.artifact.join("sha256sums.txt")).expect("the artifact checksums"));
  // The artifact temporary directory is gone.
  assert!(
    world.artifact_temp_entries().is_empty(),
    "the temporary directory stayed: {:?}",
    world.artifact_temp_entries()
  );
  // The release directory still holds both installed binaries.
  assert!(world.releases.join(&world.on_main).join("charliebot-server").is_file(), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn test_a_stage_that_fails_after_the_download_leaves_no_binary_and_no_artifact_directory() {
  let world = World::new();
  world.sandbox.stub("systemctl", r#"case "$*" in *show-environment*) exit 0 ;; *) exit 1 ;; esac"#);

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("stage units failed"), "{}", run.describe());
  let names = world.log_names();
  assert!(names.contains(&"sha256sums.txt".to_string()), "the download's checksums did not stay: {names:?}");
  assert!(names.contains(&"units.log".to_string()), "the failed stage's log did not stay: {names:?}");
  for name in ["charliebot", "charliebot-server", "artifact"] {
    assert!(!names.contains(&name.to_string()), "the log directory holds {name}: {names:?}");
  }
  assert!(!world.releases.join(&world.on_main).exists(), "{}", run.describe());
  assert!(
    world.artifact_temp_entries().is_empty(),
    "the temporary directory stayed: {:?}",
    world.artifact_temp_entries()
  );
}

#[test]
#[ignore = "integration"]
fn test_an_interrupt_during_a_stage_removes_the_artifact_directory() {
  let world = World::new();
  // The units stage's reload SIGTERMs the release script itself, as an interrupt does: the script must still remove the
  // artifact temporary directory. The stub's parent is the slice-unit install script, whose parent is the release.
  world.sandbox.stub(
    "systemctl",
    r#"case "$*" in *show-environment*) exit 0 ;; esac
ppid() { awk '/^PPid:/ { print $2 }' "/proc/$1/status"; }
kill -TERM "$(ppid "$PPID")"
exit 1
"#,
  );

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 143, "{}", run.describe());
  assert!(!world.releases.join(&world.on_main).exists(), "{}", run.describe());
  assert!(
    world.artifact_temp_entries().is_empty(),
    "the temporary directory stayed: {:?}",
    world.artifact_temp_entries()
  );
  let names = world.log_names();
  assert!(names.contains(&"units.log".to_string()), "the interrupted stage's log did not stay: {names:?}");
  for name in ["install.log", "charliebot", "charliebot-server", "artifact"] {
    assert!(!names.contains(&name.to_string()), "the log directory holds {name}: {names:?}");
  }
}

// ---- a passing run: both binaries installed, current left alone -----------------------------------------------------

#[test]
#[ignore = "integration"]
fn test_a_passing_run_installs_both_binaries_and_leaves_current() {
  let world = World::new();

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 0, "{}", run.describe());
  let release = world.releases.join(&world.on_main);
  let mut names: Vec<String> = fs::read_dir(&release)
    .expect("the release directory")
    .map(|entry| entry.expect("a release entry").file_name().into_string().expect("a name"))
    .collect();
  names.sort();
  assert_eq!(names, ["charliebot", "charliebot-server", "release.log", "sha256sums.txt"]);
  let cli_mode = fs::metadata(release.join("charliebot")).expect("the installed CLI").permissions().mode() & 0o777;
  assert_eq!(cli_mode, 0o755);
  let checksums = fs::read_to_string(release.join("sha256sums.txt")).expect("the installed checksums");
  assert!(checksums.contains("  charliebot\n"), "{checksums}");
  assert!(checksums.contains("  charliebot-server\n"), "{checksums}");
  let mut command = harness::command("sha256sum", &world.env);
  command.arg("-c").arg("sha256sums.txt").current_dir(&release);
  let check = harness::run(&mut command);
  assert_eq!(check.code, 0, "{}", check.describe());
  let release_log = fs::read_to_string(release.join("release.log")).expect("the release log");
  for stage in ["download", "units", "install"] {
    assert!(release_log.contains(&format!("stage={stage} verdict=pass")), "{release_log}");
  }
  assert!(!release_log.contains("stage=trial"), "the release ran a trial: {release_log}");
  assert!(!release_log.contains("stage=rehearsal"), "the release ran a rehearsal: {release_log}");
  assert_eq!(world.current_link(), PathBuf::from("prior"));
  assert!(run.stdout.contains(&format!("scripts/switch-server.sh rust {}", world.on_main)), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn test_a_release_installs_the_commits_slice_units_before_it_installs_the_release() {
  let world = World::new();

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 0, "{}", run.describe());
  let calls = recorded_calls(&world.sandbox.root().join("unit-calls.log"));
  assert_eq!(
    calls,
    format!(
      "unix:path={}/bus\t--user show-environment\nunix:path={}/bus\t--user daemon-reload\n",
      world.runtime.display(),
      world.runtime.display()
    ),
    "{}",
    run.describe()
  );
  let installed =
    fs::read_to_string(world.sandbox.root().join("user-units/charliebot-agents.slice")).expect("the installed unit");
  let committed = committed_text(&world.env, &world.repo, &world.on_main, "scripts/systemd/charliebot-agents.slice");
  assert_eq!(installed, committed);
  let release_log = fs::read_to_string(world.releases.join(&world.on_main).join("release.log")).expect("the log");
  let units_at = release_log.find("stage=units verdict=pass").expect("the units stage");
  let install_at = release_log.find("stage=install verdict=pass").expect("the install stage");
  assert!(units_at < install_at, "the install ran before the units: {release_log}");
}

// ---- the prune: the newest installs and the live version stay, every other version goes -----------------------

/// The install time of fixture version `index`, in Unix seconds. Index 1 is the newest install and index 10 the oldest,
/// so name order runs against install order.
fn installed_at(index: u64) -> u64 {
  1_700_000_000 + (11 - index) * 86_400
}

/// The commit name of fixture version `index`: a full sha, distinct for each index.
fn fixture_commit(index: u64) -> String {
  format!("{index:040x}")
}

/// Makes the ten fixture versions, each installed before the release under test.
fn ten_fixture_versions(world: &World) {
  for index in 1..=10 {
    world.version_dir(&fixture_commit(index), installed_at(index));
  }
}

/// The names under `releases` that are full commit shas: the version directories.
fn version_names(releases: &Path) -> Vec<String> {
  fs::read_dir(releases)
    .expect("the release root")
    .map(|entry| entry.expect("a release entry").file_name().into_string().expect("a name"))
    .filter(|name| name.len() == 40 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
    .collect()
}

#[test]
#[ignore = "integration"]
fn test_a_release_keeps_the_ten_newest_installs_deletes_the_oldest_and_logs_the_deletion() {
  let world = World::new();
  ten_fixture_versions(&world);
  // Entries that are not version directories: a file, and a leftover of an earlier install's temporary directory.
  fs::write(world.releases.join("notes.txt"), "not a version\n").expect("a non-version file");
  let leftover = world.releases.join(format!(".{}.4242", world.on_main));
  fs::create_dir(&leftover).expect("a leftover temporary directory");

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 0, "{}", run.describe());
  for index in 1..=9 {
    assert!(world.releases.join(fixture_commit(index)).is_dir(), "version {index} was deleted: {}", run.describe());
  }
  let oldest = world.releases.join(fixture_commit(10));
  assert!(!oldest.exists(), "the oldest install stayed: {}", run.describe());
  assert!(world.releases.join(&world.on_main).is_dir(), "{}", run.describe());
  assert!(world.releases.join("notes.txt").is_file(), "{}", run.describe());
  assert!(leftover.is_dir(), "{}", run.describe());
  assert_eq!(world.current_link(), PathBuf::from("prior"));
  let prune_log = fs::read_to_string(world.stage_log("prune.log")).expect("the prune log");
  assert!(prune_log.contains(&format!("deleted {}", oldest.display())), "{prune_log}");
  assert_eq!(prune_log.matches("deleted").count(), 1, "{prune_log}");
}

#[test]
#[ignore = "integration"]
fn test_a_failed_release_deletes_no_version_directory() {
  let world = World::new();
  ten_fixture_versions(&world);
  // The units stage fails before the install, so the prune never runs.
  world.sandbox.stub("systemctl", r#"case "$*" in *show-environment*) exit 0 ;; *) exit 1 ;; esac"#);

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("stage units failed"), "{}", run.describe());
  for index in 1..=10 {
    assert!(world.releases.join(fixture_commit(index)).is_dir(), "version {index} was deleted: {}", run.describe());
  }
  assert!(!world.log_names().contains(&"prune.log".to_string()), "the prune ran after a failed release");
}

#[test]
#[ignore = "integration"]
fn test_a_release_that_leaves_exactly_ten_versions_deletes_none() {
  let world = World::new();
  for index in 1..=9 {
    world.version_dir(&fixture_commit(index), installed_at(index));
  }

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(version_names(&world.releases).len(), 10, "{}", run.describe());
  let prune_log = fs::read_to_string(world.stage_log("prune.log")).expect("the prune log");
  assert!(prune_log.is_empty(), "{prune_log}");
}

#[test]
#[ignore = "integration"]
fn test_a_current_target_outside_the_ten_newest_installs_stays_and_eleven_remain() {
  let world = World::new();
  ten_fixture_versions(&world);
  // The oldest install is the live version, so it is outside the newest ten and the prune keeps it.
  world.point_current_at(&fixture_commit(10));

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(version_names(&world.releases).len(), 11, "{}", run.describe());
  assert!(world.releases.join(fixture_commit(10)).is_dir(), "the live version was deleted: {}", run.describe());
  assert_eq!(world.current_link(), PathBuf::from(fixture_commit(10)));
  let prune_log = fs::read_to_string(world.stage_log("prune.log")).expect("the prune log");
  assert!(prune_log.is_empty(), "{prune_log}");
}

#[test]
#[ignore = "integration"]
fn test_a_re_released_old_commit_counts_as_the_newest_install() {
  let world = World::new();
  ten_fixture_versions(&world);
  // The commit is dated 2001 and has no release directory, as after a prune deleted it. A prune that sorted by commit
  // date would delete it first; its install time is now, so it stays.
  let mut env = world.env.clone();
  env.insert("GIT_AUTHOR_DATE".to_string(), "2001-02-03T04:05:06Z".to_string());
  env.insert("GIT_COMMITTER_DATE".to_string(), "2001-02-03T04:05:06Z".to_string());
  git(&env, &world.repo, &["commit", "-q", "--allow-empty", "-m", "an old commit"]);
  let old = git(&env, &world.repo, &["rev-parse", "HEAD"]);
  git(&world.env, &world.repo, &["update-ref", "refs/remotes/origin/main", &old]);
  let mut behavior = world.gh_defaults();
  behavior.runs = json!([{"databaseId": 7, "event": "push", "headSha": old, "conclusion": "success"}]).to_string();
  world.write_gh(&behavior);

  let run = world.release(&old);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.releases.join(&old).is_dir(), "the re-released commit was deleted: {}", run.describe());
  assert!(!world.releases.join(fixture_commit(10)).exists(), "{}", run.describe());
  for index in 1..=9 {
    assert!(world.releases.join(fixture_commit(index)).is_dir(), "version {index} was deleted: {}", run.describe());
  }
}

/// The calls a stub recorded, one line per invocation; a stub that never ran left no file.
fn recorded_calls(calls: &Path) -> String {
  fs::read_to_string(calls).unwrap_or_default()
}

/// The text of one file as `commit` stores it.
fn committed_text(env: &Env, cwd: &Path, commit: &str, path: &str) -> String {
  let mut command = harness::command("git", env);
  command.arg("-C").arg(cwd).arg("show").arg(format!("{commit}:{path}"));
  let run = harness::run(&mut command);
  assert_eq!(run.code, 0, "{}", run.describe());
  run.stdout
}

// ---- the moved helpers' own checks ----------------------------------------------------------------------------------

/// A text log whose sweep removed two names, both of them sessions under the home.
const TEXT_LOG: &str = concat!(
  "2026-10-21 10:00:00.000Z [info     ] agents/transport/cgroup.rs:350 session_cgroup_enabled         ",
  "memory_max_mb=1024 swap_max_mb=0 tasks_max=256\n",
  "2026-10-21 10:00:01.000Z [info     ] agents/transport/cgroup.rs:361 session_cgroup_sweep_removed   count=2 ",
  "names='[\"charliebot-sess-aaaaaaaa\", \"charliebot-sess-bbbbbbbb\"]'\n",
);

/// A JSON log whose sweep removed one name under the home and one name from another home.
const JSON_LOG: &str = concat!(
  "{\"event\": \"session_cgroup_sweep_removed\", \"count\": 2, \"names\": [\"charliebot-sess-aaaaaaaa\", ",
  "\"charliebot-sess-cccccccc\"]}\n",
);

#[test]
#[ignore = "integration"]
fn test_sweep_check_reports_each_removed_name_against_the_home_text_own_names() {
  sweep_check_reports_each_removed_name_against_the_home(TEXT_LOG, 0);
}

#[test]
#[ignore = "integration"]
fn test_sweep_check_reports_each_removed_name_against_the_home_json_foreign_name() {
  sweep_check_reports_each_removed_name_against_the_home(JSON_LOG, 1);
}

/// The sweep check prints the removed names and the foreign ones: a removed name whose session directory is under the
/// home is the server's own, and one with no directory there is foreign, which exits 1.
fn sweep_check_reports_each_removed_name_against_the_home(log_text: &str, expected_rc: i32) {
  let sandbox = Sandbox::new();
  let sessions = sandbox.dir("sessions");
  fs::create_dir(sessions.join("aaaaaaaa-1111-4111-8111-111111111111")).expect("the first session");
  fs::create_dir(sessions.join("bbbbbbbb-2222-4222-8222-222222222222")).expect("the second session");
  let log = sandbox.root().join("server.log");
  fs::write(&log, log_text).expect("the server log");

  let mut command = harness::command(harness::built_binary("charliebot"), &sandbox.env());
  command.arg("release-helper").arg("sweep-check").arg(&log).arg(&sessions);
  let run = harness::run(&mut command);

  assert_eq!(run.code, expected_rc, "{}", run.describe());
  if expected_rc == 0 {
    assert!(run.stdout.contains("foreign=none"), "{}", run.describe());
  } else {
    assert!(run.stdout.contains("foreign=charliebot-sess-cccccccc"), "{}", run.describe());
  }
}

#[test]
#[ignore = "integration"]
fn test_release_helper_self_test_passes() {
  let sandbox = Sandbox::new();
  let mut command = harness::command(harness::built_binary("charliebot"), &sandbox.env());
  command.arg("release-helper").arg("self-test");

  let run = harness::run(&mut command);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(
    run.stdout.contains("PASS self-test delegation error reports the server's git timeout text"),
    "{}",
    run.describe()
  );
}
// ---- trial --serve: the seeded home, the tmux server and the tmux socket limit ----------------------------------------

/// The serve runs that pick a port of the trial range run one at a time: each one picks a free port, so two runs could
/// pick the same one.
static TRIAL_RUNS: Mutex<()> = Mutex::new(());

/// One serve trial's inputs: a throwaway repository whose origin/main names its commit, the server and the CLI beside
/// each other, a live home with one backend option, and a stub charlie-code. The trial's tree and evidence stay under
/// the sandbox.
struct ServeWorld {
  sandbox: Sandbox,
  repo: PathBuf,
  release: PathBuf,
  commit: String,
  env: Env,
}

impl ServeWorld {
  fn new() -> Self {
    let sandbox = Sandbox::new();
    let repo = sandbox.dir("repo");
    fs::create_dir_all(repo.join("scripts/release")).expect("the script directory");
    harness::copy_file(&harness::repo_root().join("scripts/release/trial.sh"), &repo.join("scripts/release/trial.sh"));
    fs::write(repo.join("README.md"), "serve trial\n").expect("the README");

    let mut env = sandbox.env();
    // The server refuses a repository root without web/templates or the agent gh wrapper, and the trial's archive is
    // this repository's commit.
    let mut copy = harness::command("cp", &env);
    copy.arg("-r").arg(harness::repo_root().join("web")).arg(&repo);
    let copied = harness::run(&mut copy);
    assert_eq!(copied.code, 0, "{}", copied.describe());
    fs::create_dir_all(repo.join("rust/crates/runtime/agents/gh_wrapper")).expect("the gh wrapper directory");
    harness::copy_file(
      &harness::repo_root().join("rust/crates/runtime/agents/gh_wrapper/gh"),
      &repo.join("rust/crates/runtime/agents/gh_wrapper/gh"),
    );
    for (name, value) in [
      ("GIT_AUTHOR_NAME", "serve-test"),
      ("GIT_AUTHOR_EMAIL", "serve-test@example.invalid"),
      ("GIT_COMMITTER_NAME", "serve-test"),
      ("GIT_COMMITTER_EMAIL", "serve-test@example.invalid"),
      ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ] {
      env.insert(name.to_string(), value.to_string());
    }
    git(&env, &repo, &["init", "-q", "-b", "main"]);
    git(&env, &repo, &["add", "."]);
    git(&env, &repo, &["commit", "-q", "-m", "the serve commit"]);
    let commit = git(&env, &repo, &["rev-parse", "HEAD"]);
    git(&env, &repo, &["update-ref", "refs/remotes/origin/main", &commit]);

    let release = sandbox.dir("release");
    symlink(harness::built_binary("charliebot-server"), release.join("charliebot-server")).expect("the server link");
    symlink(harness::built_binary("charliebot"), release.join("charliebot")).expect("the CLI link");
    let live_home = sandbox.dir("live-home");
    fs::write(
      live_home.join("config.yaml"),
      "server:\n  enable_local_work_guard: false\nbackends:\n  options:\n    - id: charlie-code-serve-test\n      label: Serve test\n      type: charlie-code\n      model: openai/test-model\n      api_base: http://127.0.0.1:9/v1\n      credential: serve-test\n",
    )
    .expect("the live config");
    fs::write(live_home.join("credentials.yaml"), "serve-test:\n  api_key: not-a-real-key\n").expect("the credentials");
    sandbox.stub("charlie-code", "exit 0\n");

    env.insert("CHARLIEBOT_HOME".to_string(), live_home.display().to_string());
    env.insert("CHARLIEBOT_RELEASE_RUN_DIR".to_string(), sandbox.dir("evidence").display().to_string());
    Self { sandbox, repo, release, commit, env }
  }

  /// Writes a synthetic home of `sessions` sessions with `mock-home generate`, under the sandbox, and returns it.
  fn seed_home(&self, name: &str, sessions: &str) -> PathBuf {
    let seed = self.sandbox.dir(name);
    let mut command = harness::command(mock_home(), &self.env);
    command.arg("generate").arg(&seed).args(["--sessions", sessions, "--seed", "3"]);
    let run = harness::run(&mut command);
    assert_eq!(run.code, 0, "{}", run.describe());
    seed
  }

  /// Runs one serve trial: the tree root and `seed` as --tree-root and --seed-home, then `command` once the server
  /// answers. The command reads the seed path from MOCK_HOME_SEED.
  fn serve(&self, tree_root: &Path, seed: &Path, command: &[&str]) -> Run {
    let _serial = TRIAL_RUNS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut trial = harness::command("bash", &self.env);
    trial
      .env("MOCK_HOME_SEED", seed)
      .env("MOCK_HOME_TOOL", mock_home())
      .arg(self.repo.join("scripts/release/trial.sh"))
      .args(["--serve", "--tree-root"])
      .arg(tree_root)
      .args(["--seed-home"])
      .arg(seed)
      .arg("--in-caller-scope")
      .arg(&self.commit)
      .arg(self.release.join("charliebot-server"))
      .arg("--")
      .args(command);
    harness::run(&mut trial)
  }
}

fn mock_home() -> PathBuf {
  harness::built_binary("mock-home")
}

/// The command that reads a seeded home back: `mock-home check` against the server that the trial started.
const CHECK_SERVED_HOME: &[&str] =
  &["sh", "-c", "\"$MOCK_HOME_TOOL\" check \"$MOCK_HOME_SEED\" \"$CHARLIEBOT_SERVE_URL\""];

#[test]
#[ignore = "integration"]
fn test_trial_serve_seeded_home_passes_the_mock_home_check() {
  let world = ServeWorld::new();
  let seed = world.seed_home("seed", "30");
  let tree_root = world.sandbox.dir("tree-root");

  let run = world.serve(&tree_root, &seed, CHECK_SERVED_HOME);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.contains("check passed: 30 sessions"), "{}", run.describe());
  assert_eq!(fs::read_dir(&tree_root).expect("the tree root").count(), 0, "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn test_trial_config_carries_the_live_weight_switches_and_an_absent_one_takes_its_default() {
  let world = ServeWorld::new();
  // The live config switches the cpu weight off and leaves the io weight absent.
  let live_config = PathBuf::from(world.env.get("CHARLIEBOT_HOME").expect("the live home")).join("config.yaml");
  let live = fs::read_to_string(&live_config).expect("the live config");
  let live = live.replacen("server:\n", "server:\n  enable_session_cpu_weight: false\n", 1);
  fs::write(&live_config, live).expect("the live config");
  let seed = world.seed_home("seed", "2");
  let tree_root = world.sandbox.dir("tree-root");
  let copied = world.sandbox.root().join("trial-config.yaml");
  let copied_text = copied.display().to_string();

  let run =
    world.serve(&tree_root, &seed, &["sh", "-c", "cp \"$CHARLIEBOT_HOME/config.yaml\" \"$1\"", "sh", &copied_text]);

  assert_eq!(run.code, 0, "{}", run.describe());
  let config = fs::read_to_string(&copied).expect("the trial config");
  assert!(config.contains("\n  enable_session_cpu_weight: false\n"), "{config}");
  // The server's default, the value the trial's server starts with.
  assert!(config.contains("\n  enable_session_io_weight: false\n"), "{config}");
}

#[test]
#[ignore = "integration"]
fn test_trial_serve_check_exits_1_and_names_a_session_whose_event_log_is_corrupted() {
  let world = ServeWorld::new();
  let seed = world.seed_home("seed", "30");
  let manifest: serde_json::Value =
    serde_json::from_slice(&fs::read(seed.join("manifest.json")).expect("the manifest")).expect("the manifest JSON");
  // The check always reads the largest session, so the corruption goes there.
  let largest = manifest["sessions"]
    .as_array()
    .expect("the manifest sessions")
    .iter()
    .max_by_key(|entry| entry["event_log_bytes"].as_u64().expect("a size"))
    .expect("a session");
  let session_id = largest["id"].as_str().expect("a session id").to_string();
  // A newest event that the manifest does not name: the served newest message then differs from the manifest.
  let log = seed.join(&session_id).join("data/chat_events.jsonl");
  let mut file = fs::OpenOptions::new().append(true).open(&log).expect("the event log");
  let foreign =
    json!({"type": "user", "content": "tampered after the manifest was written", "timestamp": "2026-10-01T00:00:00Z"});
  writeln!(file, "{foreign}").expect("the appended event");
  let tree_root = world.sandbox.dir("tree-root");

  let run = world.serve(&tree_root, &seed, CHECK_SERVED_HOME);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains(&format!("MISMATCH session {session_id}:")), "{}", run.describe());
  assert!(run.stderr.contains(&format!("Checkpoint {session_id}:")), "the manifest value: {}", run.describe());
  assert!(run.stderr.contains("tampered after the manifest was written"), "the served value: {}", run.describe());
  assert_eq!(fs::read_dir(&tree_root).expect("the tree root").count(), 0, "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn test_trial_serve_stops_the_tmux_server_it_started_and_removes_its_tree() {
  let world = ServeWorld::new();
  let seed = world.seed_home("seed", "1");
  let tree_root = world.sandbox.dir("tree-root");
  let pid_file = world.sandbox.root().join("tmux.pid");
  // The command inherits TMUX_TMPDIR from the trial, so this tmux server listens on the trial's own socket.
  let script = format!(
    "tmux -L charliebot new-session -d -s probe cat && tmux -L charliebot display-message -p '#{{pid}}' > '{}'",
    pid_file.display()
  );

  let run = world.serve(&tree_root, &seed, &["sh", "-c", script.as_str()]);

  assert_eq!(run.code, 0, "{}", run.describe());
  let tmux_pid: i32 =
    fs::read_to_string(&pid_file).expect("the tmux server pid").trim().parse().expect("a numeric tmux server pid");
  assert!(!process_is_running(tmux_pid), "the trial left tmux server {tmux_pid} running: {}", run.describe());
  assert_eq!(fs::read_dir(&tree_root).expect("the tree root").count(), 0, "{}", run.describe());
}

/// Whether process `pid` runs, as /proc reports it: a zombie has ended.
fn process_is_running(pid: i32) -> bool {
  let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
    return false;
  };
  let state = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().next()).unwrap_or("");
  state != "Z"
}

#[test]
#[ignore = "integration"]
fn test_trial_serve_refuses_a_tmux_socket_path_of_108_bytes_or_more() {
  let world = ServeWorld::new();
  let seed = world.seed_home("seed", "1");
  let tree_root = world.sandbox.dir(&format!("tree-root-{}", "long-directory-name-".repeat(3)));
  let uid = fs::metadata(&tree_root).expect("the tree root").uid();
  let socket = format!("{}/trial-{}/tmux/tmux-{uid}/charliebot", tree_root.display(), &world.commit[..12]);
  assert!(socket.len() >= 108, "the fixture socket path is too short: {socket}");

  let run = world.serve(&tree_root, &seed, &["true"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains(&socket), "{}", run.describe());
  assert_eq!(
    fs::read_dir(&tree_root).expect("the tree root").count(),
    0,
    "the refusal made a tree: {}",
    run.describe()
  );
}

/// A git command in `cwd` with the fixture's identity. A failing command fails the test.
fn git(env: &Env, cwd: &Path, args: &[&str]) -> String {
  let mut command = harness::command("git", env);
  command.arg("-C").arg(cwd).args(args);
  let run = harness::run(&mut command);
  assert_eq!(run.code, 0, "git {args:?} in {}: {}", cwd.display(), run.describe());
  run.stdout.trim().to_string()
}
