//! The release entry point (scripts/release.sh), for real, against a stub gh and a sandboxed user manager.
//!
//! Every parametrized case is its own test. Each
//! test builds a throwaway git repository, release root and log root in a sandbox, so nothing touches
//! ~/charliebot-releases or the live home.

#[path = "../common/release_lock.rs"]
mod release_lock;
mod world;

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox};
use release_lock::LockHolder;
use serde_json::json;
use tempfile::TempDir;
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

/// The preflight refuses a commit with no successful push run that has both Rust jobs green. The cases are no run of
/// the commit, a run whose rust-test job failed, and a failed run.
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

/// The stub gh refuses the display name CI, which two workflows share, so this release passes only when the lookup
/// names the workflow by its file, ci.yml.
#[test]
#[ignore = "integration"]
fn test_a_release_finds_the_ci_run_by_its_workflow_file_when_two_workflows_share_the_name_ci() {
  let world = World::new();

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(world.releases.join(&world.on_main).is_dir(), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn test_preflight_stops_with_gh_error_when_gh_fails_reading_the_ci_runs() {
  let world = World::new();
  let gh_error = "HTTP 502: Bad Gateway (https://api.github.com/repos/example/charlie-bot/actions/runs)";
  let mut behavior = world.gh_defaults();
  behavior.runs_error = Some(gh_error.to_string());
  world.write_gh(&behavior);

  let run = world.release(&world.on_main);

  world.assert_refused_and_wrote_nothing(&run);
  assert!(run.stderr.contains(gh_error), "{}", run.describe());
  assert!(!run.stderr.contains("CI has no successful push run"), "{}", run.describe());
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
  // The units stage reloads the user manager. A refused reload fails it.
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

/// The ten fixture versions, each installed before the release under test.
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
  // date would delete it first. Its install time is now, so it stays.
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

/// The calls a stub recorded, one line per invocation. A stub that never ran left no file.
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

/// The sweep check prints the removed names and the foreign ones. A removed name whose session directory is under the
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
// ---- trial --serve: the seeded home, the tmux server and the socket path limit ----------------------------------------

/// The serve runs that pick a port of the trial range run one at a time. Each one picks a free port, so two runs could
/// pick the same one.
static TRIAL_RUNS: Mutex<()> = Mutex::new(());

/// The name prefix of a serve trial's tree root: six random characters follow it.
const TREE_ROOT_PREFIX: &str = "tr";

/// The access key the serve world's live home and its live server share, so the trial's `backend resolve` is admitted.
const LIVE_ACCESS_KEY: &str = "serve-test-live-access-key";

/// The live server of a serve world: `charliebot-server` on the live home, which the trial's probe read asks through
/// `charliebot backend resolve`. It is stopped when the world drops.
struct LiveServer {
  child: Child,
}

impl LiveServer {
  /// Starts the server on `home` and returns once its log holds `server_ready`.
  fn start(sandbox: &Sandbox, env: &Env, home: &Path, repo: &Path) -> Self {
    let log = sandbox.dir("live-logs").join("server.log");
    let log_file = fs::File::create(&log).expect("the live server log");
    let mut command = harness::command(harness::built_binary("charliebot-server"), env);
    command
      .current_dir(repo)
      .env("CHARLIEBOT_HOME", home)
      .env("CHARLIEBOT_REPO_ROOT", repo)
      .stdout(Stdio::from(log_file.try_clone().expect("the live server log handle")))
      .stderr(Stdio::from(log_file));
    let child = command.spawn().expect("the live server starts");
    let mut server = Self { child };
    server.wait_until_ready(&log);
    server
  }

  fn wait_until_ready(&mut self, log: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
      let text = fs::read_to_string(log).unwrap_or_default();
      if text.split_whitespace().any(|word| word == "server_ready") {
        return;
      }
      if let Some(status) = self.child.try_wait().expect("the live server's status") {
        panic!("the live server exited before it was ready: {status}\n{text}");
      }
      assert!(Instant::now() < deadline, "the live server logged no server_ready: {text}");
      thread::sleep(Duration::from_millis(100));
    }
  }
}

impl Drop for LiveServer {
  fn drop(&mut self) {
    if let Err(error) = self.child.kill() {
      eprintln!("stopping the live server: {error}");
    }
    if let Err(error) = self.child.wait() {
      eprintln!("waiting for the live server: {error}");
    }
  }
}

/// A port that is free on 127.0.0.1 at the moment of the call.
fn free_port() -> u16 {
  TcpListener::bind("127.0.0.1:0").expect("a free port").local_addr().expect("the port's address").port()
}

/// The number of ports in a serve world's trial range, the size of the trial's default range.
const TRIAL_PORT_SPAN: u16 = 20;

/// One serve trial's inputs: a throwaway repository whose origin/main names its commit, the server and the CLI beside
/// each other, and a live home with its live server. The live home's backends name a probe member and the two Claude
/// options the trial copies models from. A stub charlie-code completes the inputs. The trial's evidence stays under the
/// sandbox; its tree root is `tree_root`, a short directory beside the sandbox.
struct ServeWorld {
  sandbox: Sandbox,
  repo: PathBuf,
  release: PathBuf,
  commit: String,
  env: Env,
  trees: TempDir,
  _live: LiveServer,
}

impl ServeWorld {
  fn new() -> Self {
    let sandbox = Sandbox::new();
    // The tree root sits directly under the temporary directory, not under the sandbox. A --tree-root trial's Chrome
    // socket path is the root plus 68 bytes, and the sandbox's own name would exceed the limit under the runner's
    // TMPDIR. The socket budget is checked by the test_trial_preflight_* tests.
    let trees = tempfile::Builder::new().prefix(TREE_ROOT_PREFIX).tempdir().expect("the tree root");
    let repo = sandbox.dir("repo");
    fs::create_dir_all(repo.join("scripts/release")).expect("the script directory");
    harness::copy_file(&harness::repo_root().join("scripts/release/trial.sh"), &repo.join("scripts/release/trial.sh"));
    fs::write(repo.join("README.md"), "serve trial\n").expect("the README");

    let mut env = sandbox.env();
    // The server requires the checkout's web assets, cron defaults and prompts, the agent gh wrapper and the agent
    // ssh guard. The trial's archive is this repository's commit.
    let source_repo = harness::repo_root();
    let mut copy = harness::command("cp", &env);
    copy.arg("-r").arg(source_repo.join("web")).arg(source_repo.join("configs")).arg(&repo);
    let copied = harness::run(&mut copy);
    assert_eq!(copied.code, 0, "{}", copied.describe());
    fs::create_dir_all(repo.join("prompts")).expect("the prompt directory");
    let mut copy = harness::command("cp", &env);
    copy.arg("-r").arg(source_repo.join("prompts/cron")).arg(repo.join("prompts"));
    let copied = harness::run(&mut copy);
    assert_eq!(copied.code, 0, "{}", copied.describe());
    fs::create_dir_all(repo.join("rust/crates/runtime/agents/gh_wrapper")).expect("the gh wrapper directory");
    harness::copy_file(
      &source_repo.join("rust/crates/runtime/agents/gh_wrapper/gh"),
      &repo.join("rust/crates/runtime/agents/gh_wrapper/gh"),
    );
    fs::create_dir_all(repo.join("rust/crates/runtime/agents/ssh_guard")).expect("the ssh guard directory");
    harness::copy_file(
      &source_repo.join("rust/crates/runtime/agents/ssh_guard/ssh"),
      &repo.join("rust/crates/runtime/agents/ssh_guard/ssh"),
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
    let live_port = free_port();
    fs::write(
      live_home.join("config.yaml"),
      format!(
        concat!(
          "server:\n  host: 127.0.0.1\n  port: {port}\n  session_memory_max_mb: 0\n  min_free_disk_gib: 0\n",
          "  enable_local_work_guard: false\n",
          "backends:\n  options:\n",
          "    - id: charlie-code-serve-test\n      label: Serve test\n      type: charlie-code\n",
          "      model: openai/test-model\n      api_base: http://127.0.0.1:9/v1\n      credential: serve-test\n",
          // The default memory-curator seed names claude-opus and claude-fable, so the live server refuses to start
          // without them.
          "    - id: claude-opus\n      label: Opus\n      type: cc-claude\n      model: claude-opus-5-5\n",
          "      effort: xhigh\n",
          "    - id: claude-fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n",
          "      effort: xhigh\n",
          "  calls:\n    new_session: charlie-code-serve-test\n    probe: charlie-code-serve-test\n",
        ),
        port = live_port,
      ),
    )
    .expect("the live config");
    fs::write(
      live_home.join("credentials.yaml"),
      format!("serve-test:\n  api_key: not-a-real-key\ncharliebot:\n  access_key: {LIVE_ACCESS_KEY}\n"),
    )
    .expect("the credentials");
    sandbox.stub("charlie-code", "exit 0\n");

    let mut disable_tasks = harness::command(harness::built_binary("charliebot"), &env);
    disable_tasks
      .current_dir(&repo)
      .env("CHARLIEBOT_HOME", &live_home)
      .arg("release-helper")
      .arg("disable-cron-tasks")
      .arg(&live_home)
      .arg(&repo);
    let disabled = harness::run(&mut disable_tasks);
    assert_eq!(disabled.code, 0, "{}", disabled.describe());

    let live = LiveServer::start(&sandbox, &env, &live_home, &repo);
    // The trial's port range starts at a port the kernel frees for this world, not at the default range, which other
    // trials on the host share. A foreign listener on the port a trial picked fails that side.
    let trial_ports = free_port();
    env.insert("TRIAL_PORT_MIN".to_string(), trial_ports.to_string());
    env.insert("TRIAL_PORT_MAX".to_string(), (trial_ports + TRIAL_PORT_SPAN - 1).to_string());
    env.insert("CHARLIEBOT_HOME".to_string(), live_home.display().to_string());
    env.insert("CHARLIEBOT_RELEASE_RUN_DIR".to_string(), sandbox.dir("evidence").display().to_string());
    Self { sandbox, repo, release, commit, env, trees, _live: live }
  }

  /// The `--tree-root` of this world's serve trials: an existing directory, removed with the world.
  fn tree_root(&self) -> &Path {
    self.trees.path()
  }

  /// A synthetic home of `sessions` sessions, generated with `mock-home generate` under the sandbox, and its path.
  fn seed_home(&self, name: &str, sessions: &str) -> PathBuf {
    let seed = self.sandbox.dir(name);
    let mut command = harness::command(mock_home(), &self.env);
    command.arg("generate").arg(&seed).args(["--sessions", sessions, "--seed", "3"]);
    let run = harness::run(&mut command);
    assert_eq!(run.code, 0, "{}", run.describe());
    seed
  }

  /// One serve trial: the tree root and `seed` as --tree-root and --seed-home, then `command` once the server
  /// answers. The command reads the seed path from MOCK_HOME_SEED.
  fn serve(&self, tree_root: &Path, seed: &Path, command: &[&str]) -> Run {
    let _serial = TRIAL_RUNS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let arguments = self.serve_arguments(tree_root, seed, &self.release.join("charliebot-server"), command);
    let mut trial = harness::command("bash", &self.env);
    trial.env("MOCK_HOME_SEED", seed).env("MOCK_HOME_TOOL", mock_home()).args(arguments);
    harness::run(&mut trial)
  }

  /// The arguments of one serve trial in the caller's scope: the script's path, then the tree root, the seed, the
  /// commit, the server and the command after `--`. The program that runs them is bash.
  fn serve_arguments(&self, tree_root: &Path, seed: &Path, server: &Path, command: &[&str]) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = vec![
      self.repo.join("scripts/release/trial.sh").into_os_string(),
      "--serve".into(),
      "--tree-root".into(),
      tree_root.into(),
      "--seed-home".into(),
      seed.into(),
      "--in-caller-scope".into(),
      self.commit.clone().into(),
      server.into(),
      "--".into(),
    ];
    arguments.extend(command.iter().map(OsString::from));
    arguments
  }

  /// One serve trial started without waiting for it: its port range is `port` alone, and `server` is its server binary,
  /// with the CLI beside it. The caller reads the output once the trial ends.
  fn spawn_serve_on_port(&self, tree_root: &Path, port: u16, server: &Path, command: &[&str]) -> Child {
    let mut trial = harness::command("bash", &self.env);
    trial
      .env("TRIAL_PORT_MIN", port.to_string())
      .env("TRIAL_PORT_MAX", port.to_string())
      .arg(self.repo.join("scripts/release/trial.sh"))
      .args(["--serve", "--in-caller-scope", "--tree-root"])
      .arg(tree_root)
      .arg(&self.commit)
      .arg(server)
      .arg("--")
      .args(command)
      .stdout(Stdio::piped())
      .stderr(Stdio::piped());
    harness::spawn(&mut trial)
  }

  /// One serve trial without --tree-root: its tree goes under this world's TMPDIR.
  fn serve_default_tree(&self, command: &[&str]) -> Run {
    let _serial = TRIAL_RUNS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut trial = harness::command("bash", &self.env);
    trial
      .arg(self.repo.join("scripts/release/trial.sh"))
      .args(["--serve", "--in-caller-scope"])
      .arg(&self.commit)
      .arg(self.release.join("charliebot-server"))
      .arg("--")
      .args(command);
    harness::run(&mut trial)
  }
}

/// The default tree of a trial of `commit` by this user under `tmp`: the user's id and the commit's first 12 hex.
fn default_tree(tmp: &Path, commit: &str) -> PathBuf {
  let uid = fs::metadata(tmp).expect("the TMPDIR").uid();
  tmp.join(format!("charliebot-release-trial-{uid}-{}", &commit[..12]))
}

/// The socket-path limit that trial.sh enforces; the script owns the budget.
fn trial_socket_path_limit() -> usize {
  let script = fs::read_to_string(harness::repo_root().join("scripts/release/trial.sh")).expect("the trial script");
  let limits: Vec<_> = script.lines().filter_map(|line| line.trim().strip_prefix("readonly SUN_PATH_LIMIT=")).collect();
  assert_eq!(limits.len(), 1, "trial.sh owns one socket-path limit");
  limits[0].parse().expect("a numeric trial socket-path limit")
}

/// The uid that the preflight's id stub answers: six digits, the length of the round host's uid. The uid enters the
/// socket paths only through tmux's `tmux-<uid>` directory, so the budget is checked at this length.
const PREFLIGHT_UID: &str = "999999";

/// The CLI beside the server: the verbs that the trial calls before its server starts. Any other verb fails the trial.
const CLI_STUB: &str = r#"#!/usr/bin/env bash
case "$*" in
  'release-helper --help') ;;
  'config get server.enable_local_work_guard') echo false ;;
  'release-helper backend-option '*) echo 'http://127.0.0.1:9/v1' ;;
  'release-helper free-port '*) echo 18650 ;;
  'release-helper redact-stdin') cat ;;
  *) echo "charliebot stub: unexpected arguments: $*" >&2; exit 1 ;;
esac
"#;

/// The inputs of the socket preflight: a throwaway repository whose origin/main names its commit, the trial script, a
/// CLI beside a server stand-in, and stubs for charlie-code and id. The stand-in is a copy of /bin/true, so a trial
/// that passes the preflight stops at its server start with exit 1.
struct PreflightWorld {
  sandbox: Sandbox,
  repo: PathBuf,
  env: Env,
  commit: String,
  server: PathBuf,
}

impl PreflightWorld {
  fn new() -> Self {
    let sandbox = Sandbox::new();
    let repo = sandbox.dir("repo");
    fs::create_dir_all(repo.join("scripts/release")).expect("the script directory");
    harness::copy_file(&harness::repo_root().join("scripts/release/trial.sh"), &repo.join("scripts/release/trial.sh"));
    fs::write(repo.join("README.md"), "preflight\n").expect("the README");
    let mut env = sandbox.env();
    for (name, value) in [
      ("GIT_AUTHOR_NAME", "preflight-test"),
      ("GIT_AUTHOR_EMAIL", "preflight-test@example.invalid"),
      ("GIT_COMMITTER_NAME", "preflight-test"),
      ("GIT_COMMITTER_EMAIL", "preflight-test@example.invalid"),
      ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ] {
      env.insert(name.to_string(), value.to_string());
    }
    git(&env, &repo, &["init", "-q", "-b", "main"]);
    git(&env, &repo, &["add", "."]);
    git(&env, &repo, &["commit", "-q", "-m", "the preflight commit"]);
    let commit = git(&env, &repo, &["rev-parse", "HEAD"]);
    git(&env, &repo, &["update-ref", "refs/remotes/origin/main", &commit]);

    let release = sandbox.dir("release");
    harness::copy_file(Path::new("/bin/true"), &release.join("charliebot-server"));
    harness::write_executable(&release.join("charliebot"), CLI_STUB);
    sandbox.stub("charlie-code", "exit 0\n");
    sandbox.stub(
      "id",
      &format!(
        "[[ $* == -u ]] || {{ echo \"id stub: unexpected arguments: $*\" >&2; exit 1; }}\necho {PREFLIGHT_UID}\n"
      ),
    );
    env.insert("CHARLIEBOT_HOME".to_string(), sandbox.dir("live-home").display().to_string());
    env.insert("CHARLIEBOT_RELEASE_RUN_DIR".to_string(), sandbox.dir("evidence").display().to_string());
    let server = release.join("charliebot-server");
    Self { sandbox, repo, env, commit, server }
  }

  /// One trial of the world's commit with `tmpdir` as its TMPDIR, and `options` between --serve and the commit.
  fn trial(&self, tmpdir: &Path, options: &[&str]) -> Run {
    let mut env = self.env.clone();
    env.insert("TMPDIR".to_string(), tmpdir.display().to_string());
    let mut command = harness::command("bash", &env);
    command
      .arg(self.repo.join("scripts/release/trial.sh"))
      .arg("--serve")
      .args(options)
      .arg(&self.commit)
      .arg(&self.server)
      .args(["--", "true"]);
    harness::run(&mut command)
  }
}

/// Asserts that the preflight passed `socket` and printed its byte count, which is under the socket-path limit.
fn assert_socket_passes(run: &Run, socket: &str) {
  let line = format!("PREFLIGHT PASS socket: {socket} ({} bytes)", socket.len());
  assert!(run.stdout.contains(&line), "the preflight did not pass {socket}: {}", run.describe());
}

/// One round's scope TMPDIR shape: `scope.<8 characters>` under `/tmp/improve-<direction>-tmp`. A random direction
/// component of the same byte length keeps this test's parent private while preserving the round's path length.
struct RoundScope {
  scope: TempDir,
  _round_dir: TempDir,
}

impl RoundScope {
  fn new(direction: &str) -> Self {
    let round_dir = tempfile::Builder::new()
      .prefix("improve-")
      .rand_bytes(direction.len())
      .suffix("-tmp")
      .tempdir_in("/tmp")
      .expect("the isolated round's TMPDIR");
    let scope = tempfile::Builder::new()
      .prefix("scope.")
      .rand_bytes(8)
      .tempdir_in(round_dir.path())
      .expect("the scope directory");
    let expected_path = format!("/tmp/improve-{}-tmp/scope.{}", "x".repeat(direction.len()), "x".repeat(8));
    assert_eq!(scope.path().to_str().expect("the round TMPDIR is UTF-8").len(), expected_path.len());
    Self { scope, _round_dir: round_dir }
  }

  fn path(&self) -> &Path {
    self.scope.path()
  }
}

/// A trial under each round's scope TMPDIR passes the socket preflight with a six-digit uid, and it stops at its
/// server start. The directions are `ux`, the reported round, and `decoupling`, the longest direction name on the round
/// host. A shorter TMPDIR takes a smaller budget, so dev/remote-run and CI also fit.
#[test]
#[ignore = "integration"]
fn test_trial_preflight_passes_under_each_round_scope_tmpdir() {
  let world = PreflightWorld::new();
  let hex = &world.commit[..12];
  for direction in ["ux", "decoupling"] {
    let scope = RoundScope::new(direction);
    let temp = scope.path().join(format!("t-{hex}"));
    let tree = scope.path().join(format!("charliebot-release-trial-{PREFLIGHT_UID}-{hex}"));
    let run = world.trial(scope.path(), &["--in-caller-scope"]);

    assert_eq!(run.code, 1, "{direction}: {}", run.describe());
    assert_socket_passes(&run, &format!("{}/tmux/tmux-{PREFLIGHT_UID}/charliebot", temp.display()));
    assert_socket_passes(&run, &format!("{}/org.chromium.Chromium.XXXXXX/SingletonSocket", temp.display()));
    assert!(
      run.stdout.contains("FAIL serve Startup"),
      "{direction}: the trial did not reach its server start: {}",
      run.describe()
    );
    assert!(!run.stderr.contains("preflight failed"), "{direction}: {}", run.describe());
    assert!(!temp.exists() && !tree.exists(), "{direction}: the trial left its tree or temp root: {}", run.describe());
  }
}

/// A temp root whose Chrome socket path is past the limit refuses the run with exit 2, names the socket and its byte
/// count, and writes nothing under the TMPDIR.
#[test]
fn test_trial_preflight_refuses_a_long_tmpdir_and_names_the_chrome_socket() {
  let world = PreflightWorld::new();
  let base = world.sandbox.dir(&format!("long-{}", "directory-".repeat(8)));
  let socket = format!("{}/t-{}/org.chromium.Chromium.XXXXXX/SingletonSocket", base.display(), &world.commit[..12]);

  let run = world.trial(&base, &["--in-caller-scope"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("socket path must stay under 108 bytes"), "{}", run.describe());
  assert!(run.stderr.contains(&format!("{socket} ({} bytes)", socket.len())), "{}", run.describe());
  assert_eq!(fs::read_dir(&base).expect("the base").count(), 0, "the refused run wrote into {}", base.display());
}

/// A --tree-root trial keeps both sockets under its temp root, inside its tree, and removes its tree when it ends.
#[test]
#[ignore = "integration"]
fn test_trial_preflight_with_tree_root_puts_both_sockets_under_its_temp_root() {
  let world = PreflightWorld::new();
  let tree_root = tempfile::Builder::new().prefix(TREE_ROOT_PREFIX).tempdir().expect("the tree root");
  let root_arg = tree_root.path().display().to_string();
  let temp = tree_root.path().join(format!("trial-{}", &world.commit[..12])).join("tmp");

  let run = world.trial(tree_root.path(), &["--in-caller-scope", "--tree-root", &root_arg]);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert_socket_passes(&run, &format!("{}/tmux/tmux-{PREFLIGHT_UID}/charliebot", temp.display()));
  assert_socket_passes(&run, &format!("{}/org.chromium.Chromium.XXXXXX/SingletonSocket", temp.display()));
  assert_eq!(fs::read_dir(tree_root.path()).expect("the tree root").count(), 0, "the tree root kept its tree");
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
  let tree_root = world.tree_root();

  let run = world.serve(tree_root, &seed, CHECK_SERVED_HOME);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.contains("check passed: 30 sessions"), "{}", run.describe());
  assert_eq!(fs::read_dir(tree_root).expect("the tree root").count(), 0, "{}", run.describe());
}

/// The command that reads the chat page's status. `/?session=` is the chat shell: it renders without redirecting to a
/// session row, so the page reaches the new-session target that the chat route reads.
const CHAT_PAGE_STATUS: &[&str] = &[
  "sh",
  "-c",
  r#"printf 'CHAT_PAGE %s\n' "$(curl -s -o /dev/null -w '%{http_code}' -H "$CHARLIEBOT_SERVE_AUTH_HEADER" "$CHARLIEBOT_SERVE_URL/?session=")""#,
];

/// A serve trial's chat page answers 200. The server refuses a chat page whose config names no new-session target, so
/// the trial's config must name one, as the live config does.
#[test]
#[ignore = "integration"]
fn test_trial_serve_answers_the_chat_page_with_200() {
  let world = ServeWorld::new();
  let seed = world.seed_home("seed", "2");
  let tree_root = world.tree_root();

  let run = world.serve(tree_root, &seed, CHAT_PAGE_STATUS);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.contains("CHAT_PAGE 200\n"), "{}", run.describe());
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
  let tree_root = world.tree_root();
  let copied = world.sandbox.root().join("trial-config.yaml");
  let copied_text = copied.display().to_string();

  let run =
    world.serve(tree_root, &seed, &["sh", "-c", "cp \"$CHARLIEBOT_HOME/config.yaml\" \"$1\"", "sh", &copied_text]);

  assert_eq!(run.code, 0, "{}", run.describe());
  let config = fs::read_to_string(&copied).expect("the trial config");
  assert!(config.contains("\n  enable_session_cpu_weight: false\n"), "{config}");
  // The server's default, the value the trial's server starts with.
  assert!(config.contains("\n  enable_session_io_weight: false\n"), "{config}");
}

/// A serve trial fires no scheduled task: every cron task of its home reads `enabled: false` while the server serves,
/// the copied seed tasks and the repo's defaults alike, and the server seeded none of the defaults at its start, so the
/// disabled files were in place before the server started.
#[test]
#[ignore = "integration"]
fn test_trial_disables_every_cron_task_of_its_home_before_its_server_starts() {
  let world = ServeWorld::new();
  let seed = world.seed_home("seed", "2");
  let tree_root = world.tree_root();
  let copied = world.sandbox.dir("trial-cron");
  let copied_text = copied.display().to_string();

  let run = world.serve(
    tree_root,
    &seed,
    &["sh", "-c", "cp \"$CHARLIEBOT_HOME\"/config.d/cron.d/*.yaml \"$1\"", "sh", &copied_text],
  );

  assert_eq!(run.code, 0, "{}", run.describe());
  let disabled_at = run.stdout.find("SERVE PASS CronDisabled: ").expect("the pre-start cron check");
  let startup_at = run.stdout.find("SERVE PASS Startup ").expect("the ready trial server");
  assert!(disabled_at < startup_at, "cron tasks were disabled after the server started: {}", run.describe());
  let mut names = Vec::new();
  for entry in fs::read_dir(&copied).expect("the copied cron tasks") {
    let path = entry.expect("a copied cron task").path();
    let text = fs::read_to_string(&path).expect("a cron task file");
    let flags: Vec<&str> = text.lines().filter(|line| line.starts_with("enabled:")).collect();
    assert_eq!(flags, ["enabled: false"], "{}:\n{text}", path.display());
    names.push(path.file_stem().expect("a task name").to_string_lossy().into_owned());
  }
  for task in ["memory-curator", "storage-cool", "proactive-agent", "mock-yearly-review"] {
    assert!(names.iter().any(|name| name == task), "{task} is a cron task of the trial home: {names:?}");
  }
  // The server logs each default it seeds at its start. None was seeded, so the trial's files predated the start.
  let trial_evidence = fs::read_dir(world.sandbox.dir("evidence"))
    .expect("the evidence directory")
    .next()
    .expect("the trial's evidence")
    .expect("an evidence entry")
    .path();
  let log = fs::read_to_string(trial_evidence.join("server-1.log")).expect("the server log");
  assert!(log.contains("server_ready"), "{log}");
  assert!(!log.contains("cron_default_task_seeded"), "the server seeded a default at its start: {log}");
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
  let tree_root = world.tree_root();

  let run = world.serve(tree_root, &seed, CHECK_SERVED_HOME);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains(&format!("MISMATCH session {session_id}:")), "{}", run.describe());
  assert!(run.stderr.contains(&format!("Checkpoint {session_id}:")), "the manifest value: {}", run.describe());
  assert!(run.stderr.contains("tampered after the manifest was written"), "the served value: {}", run.describe());
  assert_eq!(fs::read_dir(tree_root).expect("the tree root").count(), 0, "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn test_trial_serve_stops_the_tmux_server_it_started_and_removes_its_tree() {
  let world = ServeWorld::new();
  let seed = world.seed_home("seed", "1");
  let tree_root = world.tree_root();
  let pid_file = world.sandbox.root().join("tmux.pid");
  // The command inherits TMUX_TMPDIR from the trial, so this tmux server listens on the trial's own socket.
  let script = format!(
    "tmux -L charliebot new-session -d -s probe cat && tmux -L charliebot display-message -p '#{{pid}}' > '{}'",
    pid_file.display()
  );

  let run = world.serve(tree_root, &seed, &["sh", "-c", script.as_str()]);

  assert_eq!(run.code, 0, "{}", run.describe());
  let tmux_pid: i32 =
    fs::read_to_string(&pid_file).expect("the tmux server pid").trim().parse().expect("a numeric tmux server pid");
  assert!(!process_is_running(tmux_pid), "the trial left tmux server {tmux_pid} running: {}", run.describe());
  assert_eq!(fs::read_dir(tree_root).expect("the tree root").count(), 0, "{}", run.describe());
}

/// Runs `trial` in its own process group and waits at most `limit` for it. A trial still running at the limit has its
/// group killed, so the server it started dies too, and the run's code is -1: the caller's assertions name the hang.
fn run_within(trial: &mut Command, limit: Duration) -> Run {
  trial.process_group(0).stdout(Stdio::piped()).stderr(Stdio::piped());
  let mut child = harness::spawn(trial);
  let deadline = Instant::now() + limit;
  while child.try_wait().expect("the trial's status").is_none() {
    if Instant::now() >= deadline {
      // SAFETY: kill sends SIGKILL to the process group the trial leads, which this test created for it.
      unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
      break;
    }
    thread::sleep(Duration::from_millis(50));
  }
  let output = child.wait_with_output().expect("the trial's output");
  Run {
    code: output.status.code().unwrap_or(-1),
    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
  }
}

/// The pid that a serve trial logged for its started server, from its `SERVE PASS Startup ... pid=<pid>` line.
fn started_server_pid(run: &Run) -> String {
  let line = run.stdout.lines().find(|line| line.starts_with("SERVE PASS Startup")).expect("the startup line");
  line.split_whitespace().find_map(|field| field.strip_prefix("pid=")).expect("the server pid").to_string()
}

/// A serve trial whose server stops as its command exits: the command sends SIGTERM to the server and ends at once. The
/// trial waits for the runs of its home, and it cannot read a run page from a server that is gone, so it ends within a
/// minute, failing the run wait and naming the server's pid.
#[test]
#[ignore = "integration"]
fn test_trial_serve_ends_within_a_minute_when_its_server_stops_as_the_command_exits() {
  let _serial = TRIAL_RUNS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let world = ServeWorld::new();
  let seed = world.seed_home("seed", "2");
  let tree_root = world.tree_root();
  let server = world.release.join("charliebot-server");
  let mut trial = harness::command("bash", &world.env);
  trial.args(world.serve_arguments(tree_root, &seed, &server, &["sh", "-c", "kill -TERM \"$CHARLIEBOT_SERVE_PID\""]));

  let run = run_within(&mut trial, Duration::from_secs(60));

  assert_eq!(run.code, 1, "the trial did not end within 60 s: {}", run.describe());
  let pid = started_server_pid(&run);
  assert!(run.stdout.contains("SERVE command exit=0"), "{}", run.describe());
  assert!(run.stdout.contains("FAIL serve RunWait:"), "{}", run.describe());
  // The server is gone or still shutting down when the wait reads its runs, so the CAUSE names the pid either way.
  let cause = format!("CAUSE serve: server pid {pid} ");
  assert!(run.stdout.lines().any(|line| line.starts_with(&cause)), "{}", run.describe());
  assert!(!run.stdout.contains("SERVE PASS RunWait"), "{}", run.describe());
  assert_eq!(fs::read_dir(tree_root).expect("the tree root").count(), 0, "{}", run.describe());
}

/// A serve trial whose server ignores SIGTERM. The trial waits its bounded time, sends SIGKILL and fails the side,
/// naming the server's pid. The trial runs under a lock held on fd 9 the way dev/poor-network/lock.sh's bench_lock_hold
/// holds it, so every process the trial starts inherits the lock: a process that survives the trial keeps it held.
#[test]
#[ignore = "integration"]
fn test_trial_serve_names_a_server_that_ignores_sigterm_and_releases_the_bench_lock() {
  let _serial = TRIAL_RUNS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let world = ServeWorld::new();
  let seed = world.seed_home("seed", "1");
  let tree_root = world.tree_root();
  let stub_release = world.sandbox.dir("stub-release");
  symlink(harness::built_binary("term-ignoring-server"), stub_release.join("charliebot-server"))
    .expect("the stub link");
  symlink(world.release.join("charliebot"), stub_release.join("charliebot")).expect("the CLI link");
  let port = TcpListener::bind(("127.0.0.1", 0)).expect("a free port").local_addr().expect("the port").port();
  let lock = world.sandbox.root().join("bench.lock");
  let mut trial = harness::command("sh", &world.env);
  trial
    .arg("-c")
    .arg("exec 9>\"$0\"; flock -n 9 || exit 75; exec \"$@\"")
    .arg(&lock)
    .arg("bash")
    .args(world.serve_arguments(tree_root, &seed, &stub_release.join("charliebot-server"), &["true"]))
    .env("TRIAL_PORT_MIN", port.to_string())
    .env("TRIAL_PORT_MAX", port.to_string());

  let run = run_within(&mut trial, Duration::from_secs(120));

  assert_eq!(run.code, 1, "{}", run.describe());
  let pid = started_server_pid(&run);
  assert!(run.stdout.contains("SERVE command exit=0"), "{}", run.describe());
  assert!(run.stdout.contains("FAIL serve Stop:"), "{}", run.describe());
  let cause = format!("CAUSE serve: server pid {pid} did not exit within 20000 ms of SIGTERM; the trial sent SIGKILL");
  assert!(run.stdout.contains(&cause), "{}", run.describe());
  let server_pid: i32 = pid.parse().expect("a numeric server pid");
  assert!(!process_is_running(server_pid), "the server {pid} outlived the trial: {}", run.describe());
  // A process of the trial that still holds fd 9 keeps the lock, and the test's own flock on a new descriptor fails.
  let file = fs::File::open(&lock).expect("the bench lock file");
  // SAFETY: flock takes a descriptor this test owns and two flag values, and reads no memory.
  let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
  assert_eq!(locked, 0, "a process of the trial still holds the bench lock: {}", run.describe());
  assert_eq!(fs::read_dir(tree_root).expect("the tree root").count(), 0, "{}", run.describe());
}

/// A serve trial whose chosen port another process takes after the trial picks it. The stand-in server logs
/// `server_ready` and never binds, so the test takes the port once that line is in the trial's log, and answers the
/// readiness probe with 200 as a foreign server would. The side fails at start, naming the port and the holder (this
/// test process), and no command runs.
#[test]
#[ignore = "integration"]
fn test_trial_serve_fails_the_side_when_another_process_holds_its_port() {
  let _serial = TRIAL_RUNS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let world = ServeWorld::new();
  let tree_root = world.tree_root();
  let stub_release = world.sandbox.dir("stub-release");
  symlink(harness::built_binary("serve-stub"), stub_release.join("charliebot-server")).expect("the stub link");
  symlink(world.release.join("charliebot"), stub_release.join("charliebot")).expect("the CLI link");
  let port = TcpListener::bind(("127.0.0.1", 0)).expect("a free port").local_addr().expect("the port").port();
  let marker = world.sandbox.root().join("command-ran");
  let touch = format!("touch '{}'", marker.display());
  let trial =
    world.spawn_serve_on_port(tree_root, port, &stub_release.join("charliebot-server"), &["sh", "-c", touch.as_str()]);

  let server_log = tree_root.join(format!("trial-{}", &world.commit[..12])).join("server-1.log");
  wait_for_server_ready(&server_log);
  let holder = TcpListener::bind(("127.0.0.1", port)).expect("the test takes the port the trial picked");
  thread::spawn(move || answer_probes(holder));

  let output = trial.wait_with_output().expect("the trial ends");
  let run = Run {
    code: output.status.code().unwrap_or(-1),
    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
  };
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stdout.contains("FAIL serve Startup"), "{}", run.describe());
  assert!(run.stdout.contains(&format!("CAUSE serve: port {port} is held by another process")), "{}", run.describe());
  assert!(
    run.stdout.contains(&format!("pid {}", std::process::id())),
    "the holder is this process: {}",
    run.describe()
  );
  assert!(!run.stdout.contains("SERVE PASS Startup"), "{}", run.describe());
  assert!(!marker.exists(), "a command ran on a side whose port another process holds: {}", run.describe());
  assert_eq!(fs::read_dir(tree_root).expect("the tree root").count(), 0, "{}", run.describe());
}

/// The `ss` on the test process's PATH, by its absolute path.
fn real_ss() -> PathBuf {
  std::env::var_os("PATH")
    .and_then(|path| std::env::split_paths(&path).map(|dir| dir.join("ss")).find(|candidate| candidate.is_file()))
    .expect("ss resolves on PATH")
}

/// A serve trial whose own server holds its port while `ss -p` names no listener. The stub `ss` strips the users field
/// from every line, as `ss -p` does for a listener that it lists without its owner. The side passes on the trial's own
/// fd tables, and the command runs once the server answers.
#[test]
#[ignore = "integration"]
fn test_trial_serve_passes_its_own_server_when_ss_names_no_listener() {
  let world = ServeWorld::new();
  let seed = world.seed_home("seed", "1");
  let tree_root = world.tree_root();
  world.sandbox.stub(
    "ss",
    &format!(
      "out=$('{}' \"$@\") || exit $?\n[ -z \"$out\" ] || printf '%s\\n' \"$out\" | sed 's/ users:[^ ]*//'\nexit 0\n",
      real_ss().display()
    ),
  );

  let run = world.serve(tree_root, &seed, &["true"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.contains("SERVE PASS Startup"), "{}", run.describe());
  assert!(!run.stdout.contains("CAUSE serve"), "{}", run.describe());
  assert!(run.stdout.contains("SERVE command exit=0"), "{}", run.describe());
}

/// A serve trial whose chosen port a process takes after the trial picks it. That process's fd table is unreadable to
/// this user, so `ss -p` names it by no pid. The holder is not the trial's server: the side fails at start with the
/// port and the unnamed holder, and no command runs.
#[test]
#[ignore = "integration"]
fn test_trial_serve_fails_the_side_when_an_unnamed_process_holds_its_port() {
  let _serial = TRIAL_RUNS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let world = ServeWorld::new();
  let tree_root = world.tree_root();
  let stub_release = world.sandbox.dir("stub-release");
  symlink(harness::built_binary("serve-stub"), stub_release.join("charliebot-server")).expect("the stub link");
  symlink(world.release.join("charliebot"), stub_release.join("charliebot")).expect("the CLI link");
  let port = TcpListener::bind(("127.0.0.1", 0)).expect("a free port").local_addr().expect("the port").port();
  let marker = world.sandbox.root().join("command-ran");
  let touch = format!("touch '{}'", marker.display());
  let trial =
    world.spawn_serve_on_port(tree_root, port, &stub_release.join("charliebot-server"), &["sh", "-c", touch.as_str()]);

  let server_log = tree_root.join(format!("trial-{}", &world.commit[..12])).join("server-1.log");
  wait_for_server_ready(&server_log);
  let mut holder =
    harness::spawn(harness::command(harness::built_binary("port-holder"), &world.env).arg(port.to_string()));

  let output = trial.wait_with_output().expect("the trial ends");
  holder.kill().expect("the holder stops");
  holder.wait().expect("the holder ends");
  let run = Run {
    code: output.status.code().unwrap_or(-1),
    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
  };
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stdout.contains("FAIL serve Startup"), "{}", run.describe());
  assert!(run.stdout.contains(&format!("CAUSE serve: port {port} is held by another process")), "{}", run.describe());
  assert!(run.stdout.contains("a process this user cannot name"), "{}", run.describe());
  assert!(!run.stdout.contains("SERVE PASS Startup"), "{}", run.describe());
  assert!(!marker.exists(), "a command ran on a side whose port an unnamed process holds: {}", run.describe());
  assert_eq!(fs::read_dir(tree_root).expect("the tree root").count(), 0, "{}", run.describe());
}

/// Waits until the trial's server log holds the stand-in's readiness line, which the trial reads before it checks the
/// port.
fn wait_for_server_ready(log: &Path) {
  let deadline = Instant::now() + Duration::from_secs(60);
  while !fs::read_to_string(log).is_ok_and(|text| text.contains(" server_ready ")) {
    assert!(Instant::now() < deadline, "the trial's server log never logged server_ready: {}", log.display());
    thread::sleep(Duration::from_millis(10));
  }
}

/// Answers every connection with 200, as a foreign server answers the trial's readiness probe.
fn answer_probes(listener: TcpListener) {
  for stream in listener.incoming() {
    let mut stream = stream.expect("a probe connection");
    let mut request = [0u8; 1024];
    let read = stream.read(&mut request).expect("the probe request");
    assert!(read > 0, "the probe closed before it sent a request");
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").expect("the probe answer");
  }
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
fn test_trial_serve_refuses_a_tmux_socket_path_at_the_trial_limit() {
  let world = ServeWorld::new();
  let seed = world.seed_home("seed", "1");
  let tree_root = world.sandbox.dir(&format!("tree-root-{}", "long-directory-name-".repeat(3)));
  let uid = fs::metadata(&tree_root).expect("the tree root").uid();
  let socket = format!("{}/trial-{}/tmp/tmux/tmux-{uid}/charliebot", tree_root.display(), &world.commit[..12]);
  assert!(socket.len() >= trial_socket_path_limit(), "the fixture socket path is too short: {socket}");

  let run = world.serve(&tree_root, &seed, &["true"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains(&socket), "{}", run.describe());
  assert!(run.stderr.contains(&format!("{socket} ({} bytes)", socket.len())), "{}", run.describe());
  assert_eq!(
    fs::read_dir(&tree_root).expect("the tree root").count(),
    0,
    "the refusal made a tree: {}",
    run.describe()
  );
}

#[test]
#[ignore = "integration"]
fn test_trial_default_tree_refuses_an_existing_unwritable_tree_and_names_its_path_and_owner() {
  let mut world = ServeWorld::new();
  let tmp = world.sandbox.dir("default-tmp");
  world.env.insert("TMPDIR".to_string(), tmp.display().to_string());
  let tree = default_tree(&tmp, &world.commit);
  fs::create_dir(&tree).expect("the existing tree path");
  fs::set_permissions(&tree, fs::Permissions::from_mode(0o500)).expect("the tree path made unwritable");
  let mut id = harness::command("id", &world.env);
  id.arg("-un");
  let owner = harness::run(&mut id);
  assert_eq!(owner.code, 0, "{}", owner.describe());

  let run = world.serve_default_tree(&["true"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  let refusal = format!("already exists: {} (owned by {})", tree.display(), owner.stdout.trim());
  assert!(run.stderr.contains(&refusal), "{}", run.describe());
}

/// A git command in `cwd` with the fixture's identity. A failing command fails the test.
fn git(env: &Env, cwd: &Path, args: &[&str]) -> String {
  let mut command = harness::command("git", env);
  command.arg("-C").arg(cwd).args(args);
  let run = harness::run(&mut command);
  assert_eq!(run.code, 0, "git {args:?} in {}: {}", cwd.display(), run.describe());
  run.stdout.trim().to_string()
}
