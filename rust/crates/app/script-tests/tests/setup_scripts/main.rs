//! The setup-adjacent shell scripts, run for real against stub commands: the release selection they share
//! (`$CHARLIEBOT_RELEASES_DIR/current`) and the `charliebot` calls they make through it.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox, write_executable};

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
  write_executable(&binary, format!("#!/usr/bin/env bash\n{FAKE_CHARLIEBOT}"));
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
  env.insert("PATH".to_string(), format!("{}:/usr/local/bin:/usr/bin:/bin", sandbox.bin().display()));
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

// ---- the unit install: the commit's units, read from the commit object ----------------------------------------------

const COMMITTED_SLICE: &str = "[Slice]\nCPUWeight=100\n";
const CHECKOUT_SLICE: &str = "[Slice]\nCPUWeight=1\n";
const COMMITTED_SERVICE: &str = "[Service]\nExecStart=/bin/true @REPO_ROOT@/scripts/start-server.sh\n";

#[test]
#[ignore = "integration"]
fn install_units_installs_the_commits_units_with_the_repo_path_and_reloads_the_user_manager() {
  let sandbox = Sandbox::new();
  let repo = sandbox.dir("repo");
  let unit_dir = sandbox.dir("unit-dir");
  let calls = sandbox.root().join("systemctl.calls");
  sandbox.stub("systemctl", "printf '%s\\n' \"$*\" >> \"$SYSTEMCTL_CALLS\"\n");
  let mut env = sandbox.env();
  env.insert("SYSTEMCTL_CALLS".to_string(), calls.display().to_string());
  env.insert("CHARLIEBOT_SYSTEMD_USER_DIR".to_string(), unit_dir.display().to_string());
  for (name, value) in [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_AUTHOR_NAME", "setup-test"),
    ("GIT_AUTHOR_EMAIL", "setup-test@example.invalid"),
    ("GIT_COMMITTER_NAME", "setup-test"),
    ("GIT_COMMITTER_EMAIL", "setup-test@example.invalid"),
  ] {
    env.insert(name.to_string(), value.to_string());
  }
  let git = |args: &[&str]| {
    let mut command = harness::command("git", &env);
    command.arg("-C").arg(&repo).args(args);
    let run = harness::run(&mut command);
    assert_eq!(run.code, 0, "{}", run.describe());
    run.stdout.trim().to_string()
  };
  git(&["init", "-q", "-b", "main"]);
  fs::create_dir_all(repo.join("scripts/systemd")).expect("the unit directory");
  fs::write(repo.join("scripts/systemd/charliebot-agents.slice"), COMMITTED_SLICE).expect("the committed unit");
  git(&["add", "scripts"]);
  git(&["commit", "-q", "-m", "the slice units"]);
  let old_commit = git(&["rev-parse", "HEAD"]);

  fs::write(repo.join("scripts/systemd/charliebot-boot.service"), COMMITTED_SERVICE).expect("the committed service");
  git(&["add", "scripts"]);
  git(&["commit", "-q", "-m", "the units"]);
  let commit = git(&["rev-parse", "HEAD"]);
  // A checkout that edits the unit after the commit: the install must read the commit, not this file.
  fs::write(repo.join("scripts/systemd/charliebot-agents.slice"), CHECKOUT_SLICE).expect("the edited unit");

  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("scripts/install-slice-units.sh")).arg(&repo).arg(&commit);
  let run = harness::run(&mut command);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(
    fs::read_to_string(unit_dir.join("charliebot-agents.slice")).expect("the installed unit"),
    COMMITTED_SLICE
  );
  // The service names the install's checkout where the commit names @REPO_ROOT@.
  assert_eq!(
    fs::read_to_string(unit_dir.join("charliebot-boot.service")).expect("the installed service"),
    COMMITTED_SERVICE.replace("@REPO_ROOT@", &repo.display().to_string())
  );

  // A release predating service units still installs its slices successfully.
  let mut command = harness::command("bash", &env);
  command.arg(harness::repo_root().join("scripts/install-slice-units.sh")).arg(&repo).arg(&old_commit);
  let old_run = harness::run(&mut command);
  assert_eq!(old_run.code, 0, "{}", old_run.describe());
  assert_eq!(recorded(&calls), ["--user daemon-reload", "--user daemon-reload"]);
}

#[test]
#[ignore = "integration"]
fn the_rewritten_shell_entry_points_parse() {
  for script in [
    "scripts/setup.sh",
    "scripts/install-slice-units.sh",
    "scripts/preflight-charlie-code.sh",
    "scripts/sync-skills.sh",
    "dev/check-skills-host-leak.sh",
    "dev/stress-tests.sh",
  ] {
    let sandbox = Sandbox::new();
    let mut command = harness::command("bash", &sandbox.env());
    command.arg("-n").arg(harness::repo_root().join(script));
    let run = harness::run(&mut command);
    assert_eq!(run.code, 0, "{script}: {}", run.describe());
  }
}

/// Parse an `ExecStart=` directive from a systemd unit using systemd command-line quoting rules.
fn systemd_exec_start_args(unit_text: &str) -> Vec<String> {
  let mut in_exec = false;
  let mut raw = String::new();
  for line in unit_text.lines() {
    if !in_exec {
      if let Some(rest) = line.strip_prefix("ExecStart=") {
        in_exec = true;
        let trimmed = rest.strip_suffix('\\').unwrap_or(rest);
        raw.push_str(trimmed);
        if !rest.ends_with('\\') {
          break;
        }
      }
    } else {
      let trimmed = line.strip_suffix('\\').unwrap_or(line);
      raw.push(' ');
      raw.push_str(trimmed.trim_start());
      if !line.ends_with('\\') {
        break;
      }
    }
  }

  let mut words = Vec::new();
  let chars: Vec<char> = raw.chars().collect();
  let mut i = 0;
  while i < chars.len() {
    while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
      i += 1;
    }
    if i >= chars.len() {
      break;
    }
    let mut word = String::new();
    let mut quote: Option<char> = None;
    while i < chars.len() {
      let c = chars[i];
      match quote {
        None => {
          if c == ' ' || c == '\t' {
            break;
          } else if c == '\'' || c == '"' {
            quote = Some(c);
            i += 1;
          } else if c == '\\' && i + 1 < chars.len() {
            i += 1;
            word.push(chars[i]);
            i += 1;
          } else if c == '$' && i + 1 < chars.len() && chars[i + 1] == '$' {
            word.push('$');
            i += 2;
          } else {
            word.push(c);
            i += 1;
          }
        }
        Some('\'') => {
          if c == '\'' {
            quote = None;
            i += 1;
          } else if c == '\\' && i + 1 < chars.len() && (chars[i + 1] == '\'' || chars[i + 1] == '\\') {
            word.push(chars[i + 1]);
            i += 2;
          } else if c == '$' && i + 1 < chars.len() && chars[i + 1] == '$' {
            word.push('$');
            i += 2;
          } else {
            word.push(c);
            i += 1;
          }
        }
        Some('"') => {
          if c == '"' {
            quote = None;
            i += 1;
          } else if c == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            match next {
              '"' | '\\' => word.push(next),
              'n' => word.push('\n'),
              't' => word.push('\t'),
              's' => word.push(' '),
              _ => word.push(next),
            }
            i += 2;
          } else if c == '$' && i + 1 < chars.len() && chars[i + 1] == '$' {
            word.push('$');
            i += 2;
          } else {
            word.push(c);
            i += 1;
          }
        }
        _ => unreachable!(),
      }
    }
    words.push(word);
  }
  words
}

#[test]
#[ignore = "integration"]
fn the_boot_service_exec_start_command_parses_through_systemd() {
  let unit_path = harness::repo_root().join("scripts/systemd/charliebot-boot.service");
  let unit_text = fs::read_to_string(&unit_path).expect("the boot service unit");
  let args = systemd_exec_start_args(&unit_text);
  assert_eq!(args.first().map(String::as_str), Some("/bin/bash"));
  assert_eq!(args.get(1).map(String::as_str), Some("-c"));
  let script = args.get(2).expect("the script argument to bash -c");

  let sandbox = Sandbox::new();
  let mut command = harness::command("bash", &sandbox.env());
  command.args(["-n", "-c", script]);
  let run = harness::run(&mut command);
  assert_eq!(run.code, 0, "boot start command failed to parse: {}", run.describe());
}

#[test]
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
