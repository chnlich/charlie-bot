//! The release entry point (scripts/release.sh) and the scripts it runs, for real, against a stub gh and stub stage
//! commands.
//!
//! These are the eleven test functions of tests/test_release.py, with every parametrized case as its own test. Each
//! test builds a throwaway git repository, release root and log root in a sandbox, so nothing touches
//! ~/charliebot-releases or the live home.

mod world;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use charliebot_shell_harness::{self as harness, Sandbox};
use serde_json::json;
use world::World;

// ---- preflight refusals: exit 2 and nothing written ----------------------------------------------------------------

#[test]
fn test_preflight_refuses_an_argument_that_is_not_a_full_sha() {
  let world = World::new();

  let run = world.release("main");

  world.assert_refused_and_wrote_nothing(&run);
  assert!(run.stderr.contains("40-character"), "{}", run.describe());
}

#[test]
fn test_preflight_refuses_a_commit_that_is_not_on_origin_main() {
  let world = World::new();

  let run = world.release(&world.off_main);

  world.assert_refused_and_wrote_nothing(&run);
  assert!(run.stderr.contains("is not on origin/main"), "{}", run.describe());
}

#[test]
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
fn test_preflight_refuses_a_commit_without_both_rust_jobs_green_no_run_for_the_commit() {
  preflight_refuses_a_commit_without_both_rust_jobs_green("no-run-for-the-commit");
}

#[test]
fn test_preflight_refuses_a_commit_without_both_rust_jobs_green_rust_test_failed() {
  preflight_refuses_a_commit_without_both_rust_jobs_green("rust-test-failed");
}

#[test]
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

// ---- a failed stage: exit 1, the stage named, nothing installed -----------------------------------------------------

#[test]
fn test_a_failed_stage_exits_1_names_the_stage_and_installs_nothing() {
  let mut world = World::new();
  let stage_fail = world.bin().join("stage-fail").display().to_string();
  world.env.insert("CHARLIEBOT_RELEASE_TRIAL".to_string(), stage_fail);

  let run = world.release(&world.on_main);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("stage trial failed"), "{}", run.describe());
  let log = world.stage_log("trial.log");
  assert!(run.stderr.contains(&format!("stage trial failed: {}", log.display())), "{}", run.describe());
  assert!(!world.releases.join(&world.on_main).exists(), "{}", run.describe());
  assert!(!world.has_temporary_release(), "{}", run.describe());
  assert_eq!(world.current_link(), PathBuf::from("prior"));
}

#[test]
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

// ---- a passing run: both binaries installed, current left alone -----------------------------------------------------

#[test]
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
  for stage in ["download", "trial", "rehearsal", "install"] {
    assert!(release_log.contains(&format!("stage={stage} verdict=pass")), "{release_log}");
  }
  assert_eq!(world.current_link(), PathBuf::from("prior"));
  assert!(run.stdout.contains(&format!("scripts/switch-server.sh rust {}", world.on_main)), "{}", run.describe());
}

// ---- the moved helpers' own checks ----------------------------------------------------------------------------------

/// A console log whose sweep removed two names, both of them sessions under the home.
const CONSOLE_LOG: &str = concat!(
  "2026-10-21 10:00:00 [info     ] session_cgroup_enabled         memory_max_mb=1024 swap_max_mb=0 tasks_max=256\n",
  "2026-10-21 10:00:01 [info     ] session_cgroup_sweep_removed   count=2 names=['charliebot-sess-aaaaaaaa', ",
  "'charliebot-sess-bbbbbbbb']\n",
);

/// A JSON log whose sweep removed one name under the home and one name from another home.
const JSON_LOG: &str = concat!(
  "{\"event\": \"session_cgroup_sweep_removed\", \"count\": 2, \"names\": [\"charliebot-sess-aaaaaaaa\", ",
  "\"charliebot-sess-cccccccc\"]}\n",
);

#[test]
fn test_sweep_check_reports_each_removed_name_against_the_home_console_own_names() {
  sweep_check_reports_each_removed_name_against_the_home(CONSOLE_LOG, 0);
}

#[test]
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

  let mut command = harness::command("python3", &sandbox.env());
  command.arg("-I").arg(harness::repo_root().join("scripts/release/sweep-check.py")).arg(&log).arg(&sessions);
  let run = harness::run(&mut command);

  assert_eq!(run.code, expected_rc, "{}", run.describe());
  if expected_rc == 0 {
    assert!(run.stdout.contains("foreign=none"), "{}", run.describe());
  } else {
    assert!(run.stdout.contains("foreign=charliebot-sess-cccccccc"), "{}", run.describe());
  }
}

#[test]
fn test_trial_helper_self_test_passes() {
  let sandbox = Sandbox::new();
  let mut command = harness::command("bash", &sandbox.env());
  command.arg(harness::repo_root().join("scripts/release/trial.sh")).arg("--self-test");

  let run = harness::run(&mut command);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(
    run.stdout.contains("PASS self-test delegation error reports the server's git timeout text"),
    "{}",
    run.describe()
  );
}

#[test]
fn test_rehearsal_usage_refuses_a_missing_argument() {
  let sandbox = Sandbox::new();
  let mut command = harness::command("bash", &sandbox.env());
  command.arg(harness::repo_root().join("scripts/release/switch-rehearsal.sh")).arg("a".repeat(40));

  let run = harness::run(&mut command);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("usage: switch-rehearsal.sh <new commit> <new release dir>"), "{}", run.describe());
}
