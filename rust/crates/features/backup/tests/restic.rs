//! Backup, restore and retention against a local directory restic repository.
//!
//! Each case runs the installed `restic`, so they are integration tests (`cargo test -- --ignored`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use charliebot_backup::backup::restic_program;
use charliebot_backup::config::ResticSettings;
use tempfile::TempDir;

/// A local directory repository initialized with a synthetic password, beside the home a case backs up.
struct Fixture {
  root: TempDir,
  settings: ResticSettings,
}

impl Fixture {
  fn new() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("key");
    fs::write(&key, "synthetic-backup-password\n").unwrap();
    let settings = ResticSettings { repository: root.path().join("repo").display().to_string(), password_file: key };
    let status = restic_command(&settings).arg("init").status().unwrap();
    assert!(status.success(), "the local repository initializes");
    Fixture { root, settings }
  }

  fn home(&self) -> PathBuf {
    self.root.path().join("home")
  }
}

/// A restic command against the fixture's repository.
fn restic_command(settings: &ResticSettings) -> Command {
  let mut command = Command::new(restic_program().expect("restic is on PATH"));
  command.arg("--repo").arg(&settings.repository).arg("--password-file").arg(&settings.password_file);
  command
}

/// Every path under `root`, relative and sorted, so a case can assert that a failed run wrote nothing.
fn tree(root: &Path) -> Vec<String> {
  fn walk(directory: &Path, root: &Path, out: &mut Vec<String>) {
    for entry in fs::read_dir(directory).unwrap() {
      let entry = entry.unwrap();
      let path = entry.path();
      out.push(path.strip_prefix(root).unwrap().display().to_string());
      if entry.file_type().unwrap().is_dir() {
        walk(&path, root, out);
      }
    }
  }
  let mut out = Vec::new();
  walk(root, root, &mut out);
  out.sort();
  out
}

#[test]
#[ignore = "integration"]
fn backup_and_restore_round_trip() {
  let fixture = Fixture::new();
  let home = fixture.home();
  fs::create_dir_all(home.join("sessions/s1/threads/t1")).unwrap();
  fs::write(home.join("config.yaml"), "server: {}\n").unwrap();
  fs::write(home.join("sessions/s1/metadata.json"), "{}\n").unwrap();
  fs::write(home.join("credentials.yaml"), "credentials:\n  access_key: secret\n").unwrap();
  fs::write(home.join("sessions/s1/threads/t1/meta.json"), "{}\n").unwrap();

  fixture.settings.backup(&home).expect("the snapshot is created");

  // Move the live home past the snapshot: the restore must bring back the snapshot's content, not this state.
  fs::write(home.join("config.yaml"), "changed: true\n").unwrap();
  fs::remove_file(home.join("sessions/s1/metadata.json")).unwrap();
  let target = fixture.root.path().join("target");
  fs::create_dir(&target).unwrap();

  fixture.settings.restore(&home, "latest", &target).expect("the snapshot restores");

  assert_eq!(fs::read_to_string(target.join("config.yaml")).unwrap(), "server: {}\n");
  assert_eq!(fs::read_to_string(target.join("sessions/s1/metadata.json")).unwrap(), "{}\n");
  assert!(!target.join("credentials.yaml").exists(), "the secrets file must not be in a snapshot");
  assert!(!target.join("sessions/s1/threads").exists(), "a session's threads must not be in a snapshot");
}

#[test]
#[ignore = "integration"]
fn an_unreachable_repository_fails_with_the_reason_and_writes_nothing() {
  let root = tempfile::tempdir().unwrap();
  let home = root.path().join("home");
  fs::create_dir(&home).unwrap();
  fs::write(home.join("config.yaml"), "server: {}\n").unwrap();
  let key = root.path().join("key");
  fs::write(&key, "synthetic-backup-password\n").unwrap();
  let settings =
    ResticSettings { repository: root.path().join("absent/repo").display().to_string(), password_file: key };

  let before = tree(root.path());
  let error = settings.backup(&home).expect_err("the repository does not exist");

  assert!(error.to_string().contains("restic exit 10"), "{error:#}");
  assert!(error.to_string().contains("repository does not exist"), "{error:#}");
  assert_eq!(tree(root.path()), before, "a failed preflight must write nothing");
  assert!(!root.path().join("home_backup").exists(), "the local archive destination must not come back");
}

#[test]
#[ignore = "integration"]
fn a_missing_password_file_fails_with_the_reason_and_writes_nothing() {
  let fixture = Fixture::new();
  let home = fixture.home();
  fs::create_dir(&home).unwrap();
  fs::write(home.join("config.yaml"), "server: {}\n").unwrap();
  let settings = ResticSettings {
    repository: fixture.settings.repository.clone(),
    password_file: fixture.root.path().join("absent.key"),
  };

  let before = tree(fixture.root.path());
  let error = settings.backup(&home).expect_err("the password file does not exist");

  assert!(error.to_string().contains("backup.password_file"), "{error:#}");
  assert!(error.to_string().contains("is not readable"), "{error:#}");
  assert_eq!(tree(fixture.root.path()), before, "a failed preflight must write nothing");
}

#[test]
#[ignore = "integration"]
fn retention_keeps_the_last_three_days_and_one_snapshot_of_each_recent_week() {
  let fixture = Fixture::new();
  let home = fixture.home();
  fs::create_dir(&home).unwrap();
  fs::write(home.join("config.yaml"), "server: {}\n").unwrap();
  for day in ["2026-10-25", "2026-11-01", "2026-11-02", "2026-11-03", "2026-11-04", "2026-11-05", "2026-11-06"] {
    let status = restic_command(&fixture.settings)
      .arg("backup")
      .arg(&home)
      .arg("--time")
      .arg(format!("{day} 04:45:00"))
      .arg("--no-cache")
      .status()
      .unwrap();
    assert!(status.success(), "the {day} snapshot is created");
  }

  fixture.settings.forget().expect("the retention pass runs");

  let output = restic_command(&fixture.settings).arg("snapshots").arg("--json").output().unwrap();
  assert!(output.status.success(), "the snapshot list reads");
  let snapshots: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
  let mut dates: Vec<String> =
    snapshots.iter().map(|snapshot| snapshot["time"].as_str().unwrap()[..10].to_string()).collect();
  dates.sort();
  assert_eq!(dates, ["2026-10-25", "2026-11-01", "2026-11-04", "2026-11-05", "2026-11-06"]);
}
