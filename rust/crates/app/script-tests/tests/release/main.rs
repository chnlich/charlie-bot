//! The release entry point (scripts/release.sh) and the scripts it runs, for real, against a stub gh and stub stage
//! commands.
//!
//! These are the eleven test functions of tests/test_release.py, with every parametrized case as its own test. Each
//! test builds a throwaway git repository, release root and log root in a sandbox, so nothing touches
//! ~/charliebot-releases or the live home.

mod world;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox, write_executable};
use serde_json::json;
use sha2::Digest as _;
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
fn test_sweep_check_reports_each_removed_name_against_the_home_text_own_names() {
  sweep_check_reports_each_removed_name_against_the_home(TEXT_LOG, 0);
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

#[test]
fn test_trial_refuses_a_server_binary_whose_cli_lacks_release_helper() {
  let sandbox = Sandbox::new();
  let release = sandbox.dir("release");
  let server = release.join("charliebot-server");
  write_executable(&server, "#!/usr/bin/env bash\nexit 0\n");
  let cli = release.join("charliebot");
  write_executable(&cli, "#!/usr/bin/env bash\nexit 2\n");
  let commit = "abcdef0123456789abcdef0123456789abcdef01";
  let tree = PathBuf::from(format!("/tmp/charliebot-release-trial-{commit}"));
  let mut env = sandbox.env();
  env.insert("CHARLIEBOT_HOME".to_string(), sandbox.dir("home").display().to_string());

  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("scripts/release/trial.sh")).arg(commit).arg(&server);
  let run = harness::run(&mut command);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("release-helper"), "{}", run.describe());
  assert!(!tree.exists(), "the refusal started the trial tree {}", tree.display());
}

#[test]
fn test_rehearsal_refuses_a_new_release_without_a_release_helper_cli() {
  let sandbox = Sandbox::new();
  let releases = sandbox.dir("releases");
  let logs = sandbox.root().join("logs");
  let release = sandbox.dir("new-release");
  write_executable(&release.join("charliebot"), "#!/usr/bin/env bash\nexit 2\n");
  let mut env = sandbox.env();
  env.insert("CHARLIEBOT_HOME".to_string(), sandbox.dir("home").display().to_string());
  env.insert("CHARLIEBOT_RELEASES_DIR".to_string(), releases.display().to_string());
  env.insert("CHARLIEBOT_RELEASE_LOG_DIR".to_string(), logs.display().to_string());

  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("scripts/release/switch-rehearsal.sh")).arg("a".repeat(40)).arg(&release);
  let run = harness::run(&mut command);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("release-helper"), "{}", run.describe());
  assert!(!logs.exists(), "the refusal wrote {}", logs.display());
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

// ---- the trial and rehearsal preflights read the live model endpoint ------------------------------------------------

/// The live endpoint the test's config names: loopback, with no key of its own. No operator host reaches a fixture.
const LIVE_API_BASE: &str = "http://127.0.0.1:9/v1";

/// The `credentials.yaml` section the live endpoint names when it needs the live key.
const LIVE_CREDENTIAL: &str = "deploy-endpoint";

/// A live home holding one `backends.options` entry with an `api_base`. With `credential`, the entry names that
/// section of the live credentials file and the file holds its `api_key`; without it, the credentials file holds no
/// backend section.
fn live_home(sandbox: &Sandbox, credential: bool) -> PathBuf {
  let home = sandbox.dir("live-home");
  let mut config = format!(
    "backends:\n  options:\n    - id: deploy-endpoint\n      type: charlie-code\n      model: openai/test-model\n      \
api_base: {LIVE_API_BASE}\n"
  );
  let mut credentials = String::new();
  if credential {
    config.push_str(&format!("      credential: {LIVE_CREDENTIAL}\n"));
    credentials.push_str(&format!("{LIVE_CREDENTIAL}:\n  api_key: not-a-real-key\n"));
  }
  credentials.push_str(&format!("charliebot:\n  access_key: {}\n", "0".repeat(64)));
  fs::write(home.join("config.yaml"), config).expect("the live config");
  fs::write(home.join("credentials.yaml"), credentials).expect("the live credentials");
  home
}

/// A live home whose only backend entry sets no `api_base`.
fn live_home_without_an_endpoint(sandbox: &Sandbox) -> PathBuf {
  let home = sandbox.dir("live-home");
  fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: subscription\n      type: claude\n      model: claude-example\n",
  )
  .expect("the live config");
  fs::write(home.join("credentials.yaml"), format!("charliebot:\n  access_key: {}\n", "0".repeat(64)))
    .expect("the live credentials");
  home
}

/// The environment both preflights read: a sandboxed home, log root and evidence root.
fn live_env(sandbox: &Sandbox, home: &Path) -> Env {
  let mut env = sandbox.env();
  env.insert("CHARLIEBOT_HOME".to_string(), home.display().to_string());
  env.insert("CHARLIEBOT_RELEASE_LOG_DIR".to_string(), sandbox.root().join("logs").display().to_string());
  env.insert("CHARLIEBOT_RELEASE_RUN_DIR".to_string(), sandbox.root().join("evidence").display().to_string());
  env
}

/// Writes the release's `charliebot` as a shim over the built CLI and returns the file it records each call in.
/// `free-port` fails, so a preflight stops after the endpoint read instead of starting a session server.
fn write_cli_shim(directory: &Path) -> PathBuf {
  let real = harness::built_binary("charliebot");
  let calls = directory.join("cli-calls.log");
  let body = format!(
    "#!/usr/bin/env bash\n\
     # The release's CLI under test: the built CLI answers every verb, and free-port refuses so the preflight stops\n\
     # after the endpoint read instead of starting a server. Every call is recorded for the test.\n\
     printf '%s\\n' \"$*\" >> \"{calls}\"\n\
     if [ \"$1 $2\" = \"release-helper free-port\" ]; then exit 1; fi\n\
     exec \"{real}\" \"$@\"\n",
    calls = calls.display(),
    real = real.display(),
  );
  let cli = directory.join("charliebot");
  write_executable(&cli, body);
  calls
}

/// The calls the shim recorded, one line per invocation; a shim that never ran left no file.
fn recorded_calls(calls: &Path) -> String {
  fs::read_to_string(calls).unwrap_or_default()
}

/// Runs the trial preflight for `home` against `release`.
fn trial_preflight(sandbox: &Sandbox, home: &Path, release: &Path) -> Run {
  let env = live_env(sandbox, home);
  let commit = git(&env, &harness::repo_root(), &["rev-parse", "HEAD"]);
  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("scripts/release/trial.sh")).arg(commit).arg(release.join("charliebot-server"));
  harness::run(&mut command)
}

/// A release directory holding the CLI shim and a server file, with the checksums the rehearsal verifies.
fn write_release(directory: &Path) -> PathBuf {
  let calls = write_cli_shim(directory);
  fs::write(directory.join("charliebot-server"), "fake server binary\n").expect("the release server");
  let mut checksums = String::new();
  for name in ["charliebot", "charliebot-server"] {
    let bytes = fs::read(directory.join(name)).expect("a release file");
    checksums.push_str(&format!("{:x}  {name}\n", sha2::Sha256::digest(bytes)));
  }
  fs::write(directory.join("sha256sums.txt"), checksums).expect("the release checksums");
  calls
}

/// Runs the rehearsal preflight for `home` and `new_release`, with `current` selecting an old release of the same
/// shape.
fn rehearsal_preflight(sandbox: &Sandbox, home: &Path, new_release: &Path) -> Run {
  let releases = sandbox.dir("releases");
  let old_commit = "1".repeat(40);
  let old_release = releases.join(&old_commit);
  fs::create_dir(&old_release).expect("the old release");
  write_release(&old_release);
  symlink(&old_commit, releases.join("current")).expect("the current link");
  let new_commit = "2".repeat(40);
  let mut env = live_env(sandbox, home);
  env.insert("CHARLIEBOT_RELEASES_DIR".to_string(), releases.display().to_string());
  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("scripts/release/switch-rehearsal.sh")).arg(&new_commit).arg(new_release);
  harness::run(&mut command)
}

/// A trial release directory: the CLI shim beside a link to the built server binary, which `ldd` resolves.
fn write_trial_release(sandbox: &Sandbox) -> (PathBuf, PathBuf) {
  let release = sandbox.dir("release");
  let calls = write_cli_shim(&release);
  symlink(harness::built_binary("charliebot-server"), release.join("charliebot-server")).expect("the server link");
  (release, calls)
}

#[test]
fn test_trial_preflight_accepts_a_live_endpoint_without_a_credential() {
  let sandbox = Sandbox::new();
  sandbox.stub("charlie-code", "exit 0\n");
  let home = live_home(&sandbox, false);
  let (release, calls) = write_trial_release(&sandbox);

  let run = trial_preflight(&sandbox, &home, &release);

  assert_eq!(run.code, 2, "{}", run.describe());
  let calls = recorded_calls(&calls);
  assert!(calls.contains("release-helper backend-option"), "the preflight did not read the live endpoint: {calls}");
  assert!(!calls.contains("credential-key"), "an endpoint without a credential must not read the live key: {calls}");
  assert!(!run.stderr.contains("backends.options entry with api_base"), "{}", run.describe());
  assert!(!run.stderr.contains("api_key"), "{}", run.describe());
  // The read succeeded: the preflight went on to the checks after it. Session cgroups are unavailable on some hosts,
  // and the free-port check follows them.
  assert!(
    run.stderr.contains("a free 127.0.0.1 port exists in 18650-18669")
      || run.stderr.contains("agents' slice cgroup directory is writable"),
    "the preflight stopped before the endpoint read: {}",
    run.describe()
  );
}

#[test]
fn test_trial_preflight_reads_the_live_key_when_the_endpoint_names_a_credential() {
  let sandbox = Sandbox::new();
  sandbox.stub("charlie-code", "exit 0\n");
  let home = live_home(&sandbox, true);
  let (release, calls) = write_trial_release(&sandbox);

  let run = trial_preflight(&sandbox, &home, &release);

  assert_eq!(run.code, 2, "{}", run.describe());
  let calls = recorded_calls(&calls);
  assert!(
    calls.contains(&format!("credential-key {}", home.join("credentials.yaml").display())),
    "the preflight did not read the named credential section: {calls}"
  );
  assert!(!run.stderr.contains("backends.options entry with api_base"), "{}", run.describe());
  assert!(!run.stderr.contains("the live credentials file holds api_key"), "{}", run.describe());
}

#[test]
fn test_trial_preflight_refuses_a_live_config_without_an_api_base_entry() {
  let sandbox = Sandbox::new();
  sandbox.stub("charlie-code", "exit 0\n");
  let home = live_home_without_an_endpoint(&sandbox);
  let (release, calls) = write_trial_release(&sandbox);

  let run = trial_preflight(&sandbox, &home, &release);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("the live config has a backends.options entry with api_base"), "{}", run.describe());
  let calls = recorded_calls(&calls);
  assert!(calls.contains("release-helper backend-option"), "the preflight did not read the live config: {calls}");
  assert!(!calls.contains("free-port"), "the preflight continued past a config with no endpoint: {calls}");
}

#[test]
fn test_rehearsal_preflight_accepts_a_live_endpoint_without_a_credential() {
  let sandbox = Sandbox::new();
  let home = live_home(&sandbox, false);
  let new_release = sandbox.dir("new-release");
  let calls = write_release(&new_release);

  let run = rehearsal_preflight(&sandbox, &home, &new_release);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("a free 127.0.0.1 port exists in 18600-18699"), "{}", run.describe());
  let calls = recorded_calls(&calls);
  assert!(calls.contains("release-helper backend-option"), "the preflight did not read the live endpoint: {calls}");
  assert!(!calls.contains("credential-key"), "an endpoint without a credential must not read the live key: {calls}");
  assert!(!run.stderr.contains("backends.options entry with api_base"), "{}", run.describe());
  assert!(!run.stderr.contains("api_key"), "{}", run.describe());
}

#[test]
fn test_rehearsal_preflight_reads_the_live_key_when_the_endpoint_names_a_credential() {
  let sandbox = Sandbox::new();
  let home = live_home(&sandbox, true);
  let new_release = sandbox.dir("new-release");
  let calls = write_release(&new_release);

  let run = rehearsal_preflight(&sandbox, &home, &new_release);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("a free 127.0.0.1 port exists in 18600-18699"), "{}", run.describe());
  let calls = recorded_calls(&calls);
  assert!(
    calls.contains(&format!("credential-key {}", home.join("credentials.yaml").display())),
    "the preflight did not read the named credential section: {calls}"
  );
  assert!(!run.stderr.contains("the live credentials file holds api_key"), "{}", run.describe());
}

#[test]
fn test_rehearsal_redacts_the_backend_key_from_saved_evidence() {
  let sandbox = Sandbox::new();
  let home = live_home(&sandbox, true);
  let new_release = sandbox.dir("new-release");
  write_release(&new_release);
  let cli = new_release.join("charliebot");
  let body =
    fs::read_to_string(&cli).unwrap().replace("if [ \"$1 $2\" = \"release-helper free-port\" ]; then exit 1; fi\n", "");
  fs::write(&cli, body).unwrap();
  let mut checksums = String::new();
  for name in ["charliebot", "charliebot-server"] {
    checksums.push_str(&format!("{:x}  {name}\n", sha2::Sha256::digest(fs::read(new_release.join(name)).unwrap())));
  }
  fs::write(new_release.join("sha256sums.txt"), checksums).unwrap();
  sandbox.stub(
    "git",
    "tree=${4%/origin.git}\nprintf 'key not-a-real-key\n' > \"$tree/logs/server_test.log\"\n\
     ln -s server_test.log \"$tree/logs/server-latest.log\"\n\
     mkdir -p \"$tree/home/.charliebot/sessions/test/data\"\n\
     printf 'key not-a-real-key\n' > \"$tree/home/.charliebot/sessions/test/data/chat_events.jsonl\"\nexit 1\n",
  );

  let run = rehearsal_preflight(&sandbox, &home, &new_release);

  assert_eq!(run.code, 2, "{}", run.describe());
  let logs = sandbox.root().join("logs");
  let evidence = fs::read_dir(logs).unwrap().next().unwrap().unwrap().path();
  for path in ["logs/server_test.log", "rehearsal-home/sessions/test/data/chat_events.jsonl"] {
    assert_eq!(fs::read_to_string(evidence.join(path)).unwrap(), "key [redacted]\n");
  }
}

// ---- trial --serve: the seeded home, the tmux server and the tmux socket limit ----------------------------------------

/// The serve trials run one at a time: each one picks a free port of the trial's range, so two starts could pick the
/// same port.
static SERVE_TRIALS: Mutex<()> = Mutex::new(());

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
    // The trial reads its stand-in approval policy table from its own repository, beside trial.sh.
    harness::copy_file(&harness::repo_root().join("scripts/release/trial.sh"), &repo.join("scripts/release/trial.sh"));
    harness::copy_file(
      &harness::repo_root().join("scripts/release/standin-approval-policy.yaml"),
      &repo.join("scripts/release/standin-approval-policy.yaml"),
    );
    fs::write(repo.join("README.md"), "serve trial\n").expect("the README");

    let mut env = sandbox.env();
    // The server refuses a repository root without web/templates, and the trial's archive is this repository's commit.
    let mut copy = harness::command("cp", &env);
    copy.arg("-r").arg(harness::repo_root().join("web")).arg(&repo);
    let copied = harness::run(&mut copy);
    assert_eq!(copied.code, 0, "{}", copied.describe());
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
      "backends:\n  options:\n    - id: charlie-code-serve-test\n      type: charlie-code\n      model: openai/test-model\n      api_base: http://127.0.0.1:9/v1\n      credential: serve-test\n",
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
    let _serial = SERVE_TRIALS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
