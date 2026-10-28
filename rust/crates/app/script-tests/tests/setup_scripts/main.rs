//! The setup-adjacent shell scripts, run for real against stub commands: the release selection they share
//! (`$CHARLIEBOT_RELEASES_DIR/current`) and the `charliebot` calls they make through it.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox};

const RELEASE_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// A `charliebot` stand-in: it records every invocation's arguments, answers the two `config get` reads with
/// `$FAKE_HOME`, and exits `$FAKE_EXIT` for everything else.
const FAKE_CHARLIEBOT: &str = r#"printf '%s\n' "$*" >> "${RECORD:?}"
case " $* " in
  *" config "*)
    echo "${FAKE_HOME:?}"
    exit 0
    ;;
esac
exit "${FAKE_EXIT:-0}"
"#;

/// The release root with one release holding the stub `charliebot`, and its `current` symlink.
fn install_release(sandbox: &Sandbox) -> PathBuf {
  let releases = sandbox.dir("releases");
  let release = releases.join(RELEASE_SHA);
  fs::create_dir_all(&release).expect("create the release directory");
  let binary = release.join("charliebot");
  fs::write(&binary, format!("#!/usr/bin/env bash\n{FAKE_CHARLIEBOT}")).expect("write the release CLI");
  fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("make the release CLI executable");
  symlink(RELEASE_SHA, releases.join("current")).expect("write the current release link");
  releases
}

/// The test environment: the release root, the recording and the home the stub answers with, a sandbox `HOME`
/// so no global skill directory is touched, and a system-only PATH that holds no `claude`, so the sync's
/// discovery check skips itself.
fn script_environment(sandbox: &Sandbox, releases: &Path, record: &Path, fake_home: &Path) -> Env {
  let mut env = sandbox.env();
  env.insert("CHARLIEBOT_RELEASES_DIR".to_string(), releases.display().to_string());
  env.insert("RECORD".to_string(), record.display().to_string());
  env.insert("FAKE_HOME".to_string(), fake_home.display().to_string());
  env.insert("HOME".to_string(), sandbox.dir("sandbox-home").display().to_string());
  let venv_bin = harness::repo_root().join(".venv/bin");
  env.insert(
    "PATH".to_string(),
    format!("{}:{}:/usr/local/bin:/usr/bin:/bin", sandbox.bin().display(), venv_bin.display()),
  );
  env
}

fn run_script(env: &Env, script: &str, args: &[&str]) -> Run {
  let mut command = harness::command("bash", env);
  command.arg(harness::repo_root().join(script)).args(args);
  harness::run(&mut command)
}

fn recorded(record: &Path) -> Vec<String> {
  fs::read_to_string(record).unwrap_or_default().lines().map(String::from).collect()
}

#[test]
fn the_rewritten_shell_entry_points_parse() {
  for script in [
    "scripts/setup.sh",
    "scripts/preflight-charlie-code.sh",
    "scripts/sync-skills.sh",
    "tools/check-skills-host-leak.sh",
    "rust/scripts/stress-tests.sh",
  ] {
    let sandbox = Sandbox::new();
    let mut command = harness::command("bash", &sandbox.env());
    command.arg("-n").arg(harness::repo_root().join(script));
    let run = harness::run(&mut command);
    assert_eq!(run.code, 0, "{script}: {}", run.describe());
  }
}

#[test]
fn a_script_without_a_current_release_exits_1_and_names_the_path() {
  for (script, args) in [
    ("scripts/setup.sh", vec![]),
    ("scripts/preflight-charlie-code.sh", vec!["synthetic-cc"]),
    ("scripts/sync-skills.sh", vec![]),
  ] {
    let sandbox = Sandbox::new();
    let releases = sandbox.dir("releases"); // no release, no current
    let mut env = sandbox.env();
    env.insert("CHARLIEBOT_RELEASES_DIR".to_string(), releases.display().to_string());
    let run = run_script(&env, script, &args);

    assert_eq!(run.code, 1, "{script}: {}", run.describe());
    assert!(run.stderr.contains(&releases.join("current").display().to_string()), "{script}: {}", run.stderr);
  }
}

#[test]
fn setup_sh_runs_the_release_cli_dry_run() {
  let sandbox = Sandbox::new();
  let record = sandbox.root().join("charliebot.record");
  let fake_home = sandbox.dir("home");
  let releases = install_release(&sandbox);
  sandbox.stub("systemctl", "exit 0\n");
  let env = script_environment(&sandbox, &releases, &record, &fake_home);
  let run = run_script(&env, "scripts/setup.sh", &["--dry-run"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  let calls = recorded(&record);
  assert!(calls.contains(&"setup --dry-run".to_string()), "the release CLI did not run setup --dry-run: {calls:?}");
  assert!(!calls.contains(&"setup".to_string()), "the dry run must not run the real setup: {calls:?}");
  assert!(calls.contains(&"config get charliebot_home".to_string()), "{calls:?}");
  assert!(run.stdout.contains("==> Syncing skills"), "{}", run.stdout);
  assert!(run.stdout.contains("Setup complete."), "{}", run.stdout);
}

#[test]
fn setup_sh_propagates_a_failing_setup_call() {
  let sandbox = Sandbox::new();
  let record = sandbox.root().join("charliebot.record");
  let fake_home = sandbox.dir("home");
  let releases = install_release(&sandbox);
  sandbox.stub("systemctl", "exit 0\n");
  let mut env = script_environment(&sandbox, &releases, &record, &fake_home);
  env.insert("FAKE_EXIT".to_string(), "3".to_string());
  let run = run_script(&env, "scripts/setup.sh", &[]);

  assert_eq!(run.code, 3, "{}", run.describe());
  assert!(recorded(&record).contains(&"setup".to_string()), "{:?}", recorded(&record));
  assert!(!run.stdout.contains("Setup complete."), "{}", run.stdout);
}

#[test]
fn preflight_charlie_code_sh_execs_the_release_cli_with_the_option_id() {
  let sandbox = Sandbox::new();
  let record = sandbox.root().join("charliebot.record");
  let fake_home = sandbox.dir("home");
  let releases = install_release(&sandbox);
  let env = script_environment(&sandbox, &releases, &record, &fake_home);
  let run = run_script(&env, "scripts/preflight-charlie-code.sh", &["synthetic-cc"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(recorded(&record), ["preflight-charlie-code synthetic-cc"]);
}

#[test]
fn preflight_charlie_code_sh_keeps_its_usage_exit_code() {
  let sandbox = Sandbox::new();
  let releases = sandbox.dir("releases");
  let mut env = sandbox.env();
  env.insert("CHARLIEBOT_RELEASES_DIR".to_string(), releases.display().to_string());
  let run = run_script(&env, "scripts/preflight-charlie-code.sh", &[]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("Usage:"), "{}", run.stderr);
}

#[test]
fn sync_skills_sh_reads_both_homes_through_the_release_cli() {
  let sandbox = Sandbox::new();
  let record = sandbox.root().join("charliebot.record");
  let fake_home = sandbox.dir("home");
  let releases = install_release(&sandbox);
  let env = script_environment(&sandbox, &releases, &record, &fake_home);
  let run = run_script(&env, "scripts/sync-skills.sh", &["-n"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  let calls = recorded(&record);
  assert!(
    calls.contains(&"config get charliebot_home".to_string())
      && calls.contains(&"config get default_charliebot_home".to_string()),
    "{calls:?}"
  );
  // The dry run syncs into the home the CLI answered with and skips the discovery probe.
  assert!(run.stdout.contains("[dry-run]"), "{}", run.stdout);
}
