use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use charliebot_shell_harness::copy_file;
use nix::fcntl::{Flock, FlockArg};

fn prepare_server_bin_dir(root: &Path, with_cli: bool) -> PathBuf {
  let directory = root.join("server-bin");
  fs::create_dir_all(&directory).unwrap();
  let server = directory.join("charliebot-server");
  copy_file(Path::new(env!("CARGO_BIN_EXE_charliebot-server")), &server);
  if with_cli {
    write_fake_cli(&directory);
  }
  directory
}

fn write_fake_cli(directory: &Path) {
  let cli = directory.join("charliebot");
  fs::write(&cli, "test fixture; this file is not run\n").unwrap();
  let mut permissions = fs::metadata(&cli).unwrap().permissions();
  permissions.set_mode(0o755);
  fs::set_permissions(&cli, permissions).unwrap();
}

fn server_command(home: &Path, args: &[&str], include_repo_root: bool, server_bin_dir: &Path) -> Command {
  let synthetic_root = home.parent().expect("the synthetic home has a parent");
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
  let mut command = Command::new(server_bin_dir.join("charliebot-server"));
  command
    .args(args)
    .current_dir(&repo_root)
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("HOME", synthetic_root)
    .env("CHARLIEBOT_HOME", home);
  if include_repo_root {
    command.env("CHARLIEBOT_REPO_ROOT", repo_root);
  }
  command
}

fn run_server(home: &Path, args: &[&str], include_repo_root: bool, server_bin_dir: &Path) -> Output {
  server_command(home, args, include_repo_root, server_bin_dir).output().expect("the server process starts")
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
  fn visit(root: &Path, directory: &Path, snapshot: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
      let entry = entry?;
      let path = entry.path();
      let relative = path.strip_prefix(root).expect("the entry is under the snapshot root").to_path_buf();
      if entry.file_type()?.is_dir() {
        snapshot.insert(relative, None);
        visit(root, &path, snapshot)?;
      } else {
        snapshot.insert(relative, Some(fs::read(path)?));
      }
    }
    Ok(())
  }

  let mut snapshot = BTreeMap::new();
  visit(root, root, &mut snapshot).expect("the synthetic home can be read");
  snapshot
}

#[test]
#[ignore = "integration"]
fn check_config_is_read_only_and_does_not_take_the_home_writer_fence() {
  let temporary = tempfile::tempdir().unwrap();
  let home = temporary.path().join("home");
  let server_bin_dir = prepare_server_bin_dir(temporary.path(), false);
  fs::create_dir_all(home.join("state")).unwrap();
  fs::write(
    home.join("config.yaml"),
    "server:\n  enable_local_work_guard: false\nbackends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\n",
  )
  .unwrap();
  fs::write(home.join("credentials.yaml"), "").unwrap();

  let lock_file = OpenOptions::new()
    .read(true)
    .write(true)
    .create(true)
    .truncate(false)
    .open(home.join("state/home_writer.lock"))
    .unwrap();
  let home_fence = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock).unwrap();
  let before = snapshot_tree(&home);
  let output = run_server(&home, &["--check-config"], true, &server_bin_dir);
  assert_eq!(output.status.code(), Some(1));
  let expected_cli = server_bin_dir.join("charliebot");
  assert!(
    String::from_utf8_lossy(&output.stderr).contains(&expected_cli.display().to_string()),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  write_fake_cli(&server_bin_dir);
  let output = run_server(&home, &["--check-config"], true, &server_bin_dir);
  assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
  assert_eq!(output.stdout, b"config ok\n");
  assert_eq!(snapshot_tree(&home), before);

  fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\nunknown_check_key: true\n",
  )
  .unwrap();
  let before = snapshot_tree(&home);
  let output = run_server(&home, &["--check-config"], true, &server_bin_dir);
  assert_eq!(output.status.code(), Some(1));
  assert!(String::from_utf8_lossy(&output.stderr).contains("unknown_check_key"));
  assert_eq!(snapshot_tree(&home), before);

  // A retired backends key refuses the load, and the error names the form that replaces it.
  for retired in ["  pool: [claude-synthetic]\n", "  auto:\n    label: Auto\n    members: [claude-synthetic]\n"] {
    fs::write(
      home.join("config.yaml"),
      format!("backends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\n{retired}"),
    )
    .unwrap();
    let before = snapshot_tree(&home);
    let output = run_server(&home, &["--check-config"], true, &server_bin_dir);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("retired") && stderr.contains("backends.pools"), "{stderr}");
    assert_eq!(snapshot_tree(&home), before);
  }

  fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\n",
  )
  .unwrap();
  let before = snapshot_tree(&home);
  let output = run_server(&home, &["--check-config"], false, &server_bin_dir);
  assert_eq!(output.status.code(), Some(1));
  assert!(String::from_utf8_lossy(&output.stderr).contains("CHARLIEBOT_REPO_ROOT"));
  assert_eq!(snapshot_tree(&home), before);

  let before = snapshot_tree(&home);
  let output = run_server(&home, &["--unknown"], true, &server_bin_dir);
  assert_eq!(output.status.code(), Some(2));
  assert!(String::from_utf8_lossy(&output.stderr).contains("Usage: charliebot-server"));
  assert_eq!(snapshot_tree(&home), before);
  drop(home_fence);
}

#[test]
#[ignore = "integration"]
fn the_local_work_guard_starts_on_a_home_without_a_runner_section() {
  let temporary = tempfile::tempdir().unwrap();
  let home = temporary.path().join("home");
  fs::create_dir_all(&home).unwrap();
  let server_bin_dir = prepare_server_bin_dir(temporary.path(), true);
  fs::write(home.join("credentials.yaml"), "").unwrap();
  // The live config's keys: no `remote_run:` section and no guard switch, so the guard runs on its default.
  let backends = "backends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\n";
  fs::write(home.join("config.yaml"), backends).unwrap();

  let output = run_server(&home, &["--check-config"], true, &server_bin_dir);
  assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
}

/// The live host config's backends in the released form: every pool names option ids, and the billing of a member
/// sits on its `backends.options` entry.
const LIVE_BACKENDS: &str = r#"backends:
  routing:
    command: [charliebot, router]
  pools:
  - id: auto
    label: Auto
    rule: priority
    members: [claude-opus-5, devin-swe2-flash, claude-code-gemini-4-argon, codex-gpt-sol, claude-code-glm53-flash]
  - id: light
    label: Light
    rule: rotate
    members: [claude-code-deepseek-flash, claude-haiku, claude-code-glm53-flash]
  - id: implement
    label: Implement
    rule: rotate
    members: [claude-code-deepseek-flash, claude-code-glm53-flash, claude-haiku]
  - id: review
    label: Review
    rule: rotate
    members: [claude-code-kimi-k3.1-pro, claude-code-glm53, claude-code-gemini-4-argon]
  - id: verify
    label: Verify
    rule: rotate
    members: [claude-code-gemini-3.8-flash, claude-code-deepseek-flash, claude-code-glm53-flash]
  - id: cold-read
    label: Cold read
    rule: rotate
    members: [claude-haiku, claude-code-gemini-3.8-flash, claude-code-deepseek-flash]
  calls:
    worker: implement
    improve: implement
    verify: verify
    review: review
    cold_read: cold-read
    probe: cold-read
    autoname: light
    triage: light
    thread: auto
    new_session: claude-opus-5
  options:
    - {id: claude-opus-5, label: Opus 5, type: cc-claude, model: claude-opus-5-5, billing: subscription}
    - {id: devin-swe2-flash, label: Devin SWE 2 Flash, type: devin, model: swe-2-flash, billing: subscription}
    - {id: claude-code-gemini-4-argon, label: Gemini 4 Argon, type: cc-claude, model: gemini-4-argon, billing: payg}
    - {id: codex-gpt-sol, label: GPT Sol, type: codex, model: gpt-sol, billing: payg}
    - {id: claude-code-glm53-flash, label: GLM 5.3 Flash, type: cc-claude, model: glm-5.3-flash, billing: payg}
    - {id: claude-code-deepseek-flash, label: DeepSeek Flash, type: cc-claude, model: deepseek-flash}
    - {id: claude-haiku, label: Haiku, type: cc-claude, model: claude-haiku-5-5}
    - {id: claude-code-kimi-k3.1-pro, label: Kimi K3.1 Pro, type: cc-claude, model: kimi-k3.1-pro}
    - {id: claude-code-glm53, label: GLM 5.3, type: cc-claude, model: glm-5.3}
    - {id: claude-code-gemini-3.8-flash, label: Gemini 3.8 Flash, type: cc-claude, model: gemini-3.8-flash}
"#;

#[test]
#[ignore = "integration"]
fn the_live_option_form_passes_check_config_on_this_release() {
  let temporary = tempfile::tempdir().unwrap();
  let home = temporary.path().join("home");
  fs::create_dir_all(&home).unwrap();
  let server_bin_dir = prepare_server_bin_dir(temporary.path(), true);
  fs::write(home.join("credentials.yaml"), "").unwrap();
  fs::write(home.join("config.yaml"), LIVE_BACKENDS).unwrap();

  let output = run_server(&home, &["--check-config"], true, &server_bin_dir);
  assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
  assert_eq!(output.stdout, b"config ok\n");
}

#[test]
#[ignore = "integration"]
fn a_mapping_pool_member_fails_check_config_through_the_ordinary_member_error() {
  let temporary = tempfile::tempdir().unwrap();
  let home = temporary.path().join("home");
  fs::create_dir_all(&home).unwrap();
  let server_bin_dir = prepare_server_bin_dir(temporary.path(), true);
  fs::write(home.join("credentials.yaml"), "").unwrap();
  fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: \
     claude-fable-5-1\n      billing: subscription\n  pools:\n    - id: auto\n      label: Auto\n      rule: \
     priority\n      members:\n      - {backend: claude-synthetic, billing: subscription}\n",
  )
  .unwrap();

  let output = run_server(&home, &["--check-config"], true, &server_bin_dir);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(output.status.code(), Some(1), "{stderr}");
  // One line: the ordinary members-list error, with no second case named for the mapping form.
  let lines: Vec<&str> = stderr.trim_end().lines().collect();
  assert_eq!(lines.len(), 1, "{stderr}");
  assert!(
    lines[0].contains("backends.pools[0].members[0]")
      && lines[0].contains("a pool member is a backends.options id, got a mapping"),
    "{stderr}"
  );
}

/// The check-config home's backends with the visibility field: one option and one pool hidden from the
/// web pickers, the rest left at the default.
const VISIBILITY_BACKENDS: &str = r#"backends:
  options:
    - {id: claude-synthetic, label: Synthetic, type: cc-claude, model: claude-fable-5-1, billing: subscription, visible: false}
    - {id: codex-synthetic, label: Codex, type: codex, model: codex-synthetic}
  pools:
    - id: auto
      label: Auto
      rule: priority
      visible: false
      members: [claude-synthetic]
"#;

#[test]
#[ignore = "integration"]
fn check_config_accepts_the_visibility_field_and_refuses_a_non_boolean() {
  let temporary = tempfile::tempdir().unwrap();
  let home = temporary.path().join("home");
  fs::create_dir_all(&home).unwrap();
  let server_bin_dir = prepare_server_bin_dir(temporary.path(), true);
  fs::write(home.join("credentials.yaml"), "").unwrap();

  fs::write(home.join("config.yaml"), VISIBILITY_BACKENDS).unwrap();
  let output = run_server(&home, &["--check-config"], true, &server_bin_dir);
  assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
  assert_eq!(output.stdout, b"config ok\n");

  fs::write(
    home.join("config.yaml"),
    VISIBILITY_BACKENDS.replace("billing: subscription, visible: false", "billing: subscription, visible: certainly"),
  )
  .unwrap();
  let output = run_server(&home, &["--check-config"], true, &server_bin_dir);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(output.status.code(), Some(1), "{stderr}");
  assert!(stderr.contains("backends.options[0].visible") && stderr.contains("expected a boolean"), "{stderr}");
}

/// A check-config command whose launch path, `argv[0]`, the test chooses.
fn release_server_command(server: &Path, argv0: &OsStr, temporary: &Path) -> Command {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
  let mut command = Command::new(server);
  command.arg0(argv0);
  command
    .args(["--check-config"])
    .current_dir(&repo_root)
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("HOME", temporary)
    .env("CHARLIEBOT_HOME", temporary.join("home"))
    .env("CHARLIEBOT_REPO_ROOT", &repo_root);
  command
}

/// The server takes its agent PATH entry from the launch path, `argv[0]`, not from the symlink-resolved executable:
/// started through the `current` link, a later release switch moves the entry to the release that serves. The launch
/// directory is what the agent environment checks, and a bare command name determines no launch path: the server
/// refuses with the named input instead of falling back to the resolved path.
#[test]
#[ignore = "integration"]
fn check_config_runs_through_a_release_link_and_takes_the_agent_path_entry_from_argv0() {
  let temporary = tempfile::tempdir().unwrap();
  let home = temporary.path().join("home");
  fs::create_dir_all(&home).unwrap();
  fs::write(home.join("credentials.yaml"), "").unwrap();
  fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\n",
  )
  .unwrap();
  let releases = temporary.path().join("releases");
  let release = releases.join("0e876cb17");
  fs::create_dir_all(&release).unwrap();
  let server = release.join("charliebot-server");
  copy_file(Path::new(env!("CARGO_BIN_EXE_charliebot-server")), &server);
  write_fake_cli(&release);
  let current = releases.join("current");
  std::os::unix::fs::symlink(&release, &current).unwrap();

  let output =
    release_server_command(&server, current.join("charliebot-server").as_os_str(), temporary.path()).output().unwrap();
  assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
  assert_eq!(output.stdout, b"config ok\n");

  // Without a CLI in the release, the refusal names the path through the link: the launch directory, not the
  // resolved release directory, is what the agent environment checks.
  fs::remove_file(release.join("charliebot")).unwrap();
  let output =
    release_server_command(&server, current.join("charliebot-server").as_os_str(), temporary.path()).output().unwrap();
  assert_eq!(output.status.code(), Some(1));
  let stderr = String::from_utf8_lossy(&output.stderr);
  let through_link = current.join("charliebot");
  let resolved = release.join("charliebot");
  assert!(stderr.contains(&through_link.display().to_string()), "{stderr}");
  assert!(!stderr.contains(&resolved.display().to_string()), "{stderr}");

  let output = release_server_command(&server, OsStr::new("charliebot-server"), temporary.path()).output().unwrap();
  assert_eq!(output.status.code(), Some(1));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("cannot determine the server launch path"), "{stderr}");
  assert!(stderr.contains("charliebot-server"), "{stderr}");
}
