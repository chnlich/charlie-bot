//! The backup, restore and retention cases against a local-directory restic repository, and the
//! handler's failure-notice wiring.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate, NaiveDateTime, TimeZone};
use tempfile::{TempDir, tempdir};

use crate::backup::*;
use crate::config::BackupConfig;
use crate::{StepFailure, failure_message, run_with_notice};

/// A local-directory restic repository with its password file, both inside a fresh temporary root, so
/// dropping the root removes the repository.
struct Repo {
  _root: TempDir,
  repository: String,
  password_file: PathBuf,
}

fn restic(repository: &str, password_file: &Path) -> Command {
  let mut command = Command::new("restic");
  command.arg("-r").arg(repository).arg("--password-file").arg(password_file);
  command
}

fn new_repo() -> Repo {
  let root = tempdir().unwrap();
  let password_file = root.path().join("password");
  fs::write(&password_file, "synthetic-password\n").unwrap();
  let repository = root.path().join("repo");
  let status = restic(repository.to_str().unwrap(), &password_file).arg("init").status().unwrap();
  assert!(status.success(), "restic init runs against the local repository");
  Repo { _root: root, repository: repository.to_str().unwrap().to_string(), password_file }
}

/// A home inside a fresh temporary root.
fn temp_home() -> (TempDir, PathBuf) {
  let root = tempdir().unwrap();
  let home = root.path().join("profile");
  fs::create_dir(&home).unwrap();
  (root, home)
}

/// Writes one fixture file, creating its parent directories.
fn write(home: &Path, relative: impl AsRef<Path>, content: &[u8]) {
  let path = home.join(relative);
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).unwrap();
  }
  fs::write(path, content).unwrap();
}

/// Builds a home with the shapes the exclusion list judges: nested directories, an empty directory, a
/// credentials file, `.git` and `.claude` directories, bytecode at two depths, the opencode store at
/// the top level and one level deeper, a session thread store beside a session file, names that are
/// not UTF-8 at two depths, a non-ASCII UTF-8 name, a name over 100 bytes, and symlinks.
fn build_fixture_home(home: &Path) {
  write(home, "config.yaml", b"server: {}\n");
  write(home, "nested/a/b/deep.txt", b"deep content\n");
  write(home, std::ffi::OsStr::from_bytes(b"raw-\xff\xfe.txt"), b"a top-level name that is not UTF-8\n");
  write(home, std::ffi::OsStr::from_bytes(b"nested/a/bad-\xc3(.txt"), b"a nested name with a truncated sequence\n");
  write(home, "nested/a/café-日本.txt", b"a non-ASCII UTF-8 name\n");
  write(home, format!("{}.txt", "n".repeat(110)), b"a name over 100 bytes\n");
  fs::create_dir_all(home.join("empty_dir")).unwrap();
  symlink("nested", home.join("link")).unwrap();
  symlink("target-".repeat(15), home.join("long-link")).unwrap();
  write(home, "credentials.yaml", b"credentials:\n  access_key: secret\n");
  write(home, ".git/HEAD", b"ref: refs/heads/main\n");
  write(home, ".claude/settings.json", b"{}\n");
  write(home, "__pycache__/x.pyc", b"bytecode\n");
  write(home, "cache/y.pyc", b"more bytecode\n");
  write(home, "opencode/data/opencode/opencode.db", b"db\n");
  write(home, "sessions/s1/opencode/note.json", b"{}\n");
  write(home, "sessions/s1/threads/t1/metadata.json", b"{}\n");
}

/// One line per tree entry: the kind, the path and, for files and symlinks, the content or target.
fn tree_entries(root: &Path) -> Vec<String> {
  fn walk(dir: &Path, relative: &str, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
      let entry = entry.unwrap();
      let name = entry.file_name().to_string_lossy().into_owned();
      let relative = if relative.is_empty() { name } else { format!("{relative}/{name}") };
      let file_type = entry.file_type().unwrap();
      if file_type.is_symlink() {
        out.push(format!("l {} -> {}", relative, fs::read_link(entry.path()).unwrap().to_string_lossy()));
      } else if file_type.is_dir() {
        walk(&entry.path(), &relative, out);
        out.push(format!("d {relative}"));
      } else {
        out.push(format!("f {relative} {}", String::from_utf8_lossy(&fs::read(entry.path()).unwrap())));
      }
    }
  }
  let mut out = Vec::new();
  walk(root, "", &mut out);
  out.sort();
  out
}

fn backup_config(repository: &str, password_file: &Path) -> BackupConfig {
  BackupConfig {
    repository: Some(repository.to_string()),
    password_file: Some(password_file.to_string_lossy().into_owned()),
    notify_user_id: Some("U0SYNTHETIC".to_string()),
  }
}

#[test]
fn the_exclusion_list_builds_on_the_canonical_directory_names() {
  let patterns = exclude_patterns(Path::new("/home/u/.charliebot"));
  assert!(patterns.contains(&"/home/u/.charliebot/opencode".to_string()));
  assert!(patterns.contains(&"sessions/*/threads".to_string()));
  assert!(patterns.contains(&"credentials.yaml".to_string()));
}

#[test]
fn a_backup_summary_line_yields_the_snapshot_id() {
  let output = concat!(
    "{\"message_type\":\"status\",\"percent_done\":0}\n",
    "{\"message_type\":\"summary\",\"snapshot_id\":\"37f88337\"}\n",
  );
  assert_eq!(snapshot_id(output).unwrap(), "37f88337");
  assert!(snapshot_id("{\"message_type\":\"status\"}\n").is_err());
}

#[test]
fn a_failure_message_names_the_step_and_the_cause() {
  let failure = StepFailure { step: "backup", error: anyhow::anyhow!("restic exited 1") };
  let message = failure_message(&failure);
  assert!(message.contains("backup"), "{message}");
  assert!(message.contains("restic exited 1"), "{message}");
}

/// The exclusion policy of the backup, written out for the test: the source entries the restore is
/// expected to reproduce. A top-level `opencode` goes, the named entries go at any depth, and every
/// session's thread store goes.
fn excluded(relative: &str) -> bool {
  let parts: Vec<&str> = relative.split('/').collect();
  if parts.first() == Some(&"opencode") {
    return true;
  }
  if parts
    .iter()
    .any(|part| matches!(*part, ".git" | ".claude" | "credentials.yaml" | "__pycache__") || part.ends_with(".pyc"))
  {
    return true;
  }
  parts.len() >= 3 && parts[0] == "sessions" && parts[2] == "threads"
}

/// The source entries a restore should hold: [`tree_entries`] minus the excluded paths.
fn kept_source_entries(home: &Path) -> Vec<String> {
  tree_entries(home)
    .into_iter()
    .filter(|entry| {
      let relative = entry.split(' ').nth(1).unwrap();
      !excluded(relative)
    })
    .collect()
}

#[test]
fn backup_then_restore_reproduces_the_home_minus_the_excluded_paths() {
  let repo = new_repo();
  let (_root, home) = temp_home();
  build_fixture_home(&home);

  let snapshot = create_snapshot(&home, &repo.repository, &repo.password_file).unwrap();
  assert_eq!(snapshot.len(), 64, "the snapshot id is a full restic id");

  let target = repo._root.path().join("restored");
  fs::create_dir(&target).unwrap();
  restore(&repo.repository, &repo.password_file, &snapshot, &target).unwrap();

  let entries = tree_entries(&target);
  assert_eq!(entries, kept_source_entries(&home));
  // Spot checks over the shapes the exclusion list judges: kept, and gone.
  assert!(entries.iter().any(|entry| entry.starts_with("f sessions/s1/opencode/note.json ")), "{entries:?}");
  assert!(entries.iter().any(|entry| entry.starts_with("f raw-")), "{entries:?}");
  assert!(
    entries.iter().all(|entry| !entry.contains("credentials.yaml")
      && !entry.contains("sessions/s1/threads")
      && !entry.ends_with(".pyc")
      && !entry.contains(".git")
      && !entry.contains(".claude")
      && !entry.starts_with("d opencode")
      && !entry.starts_with("f opencode/")),
    "{entries:?}"
  );
}

#[test]
fn a_restore_refuses_a_non_empty_target() {
  let repo = new_repo();
  let (_root, home) = temp_home();
  write(&home, "config.yaml", b"server: {}\n");
  let snapshot = create_snapshot(&home, &repo.repository, &repo.password_file).unwrap();

  let target = repo._root.path().join("restored");
  fs::create_dir(&target).unwrap();
  fs::write(target.join("occupied.txt"), b"occupied\n").unwrap();

  let error = restore(&repo.repository, &repo.password_file, &snapshot, &target).unwrap_err();
  assert!(error.to_string().contains("not empty"), "{error:#}");
}

#[test]
fn a_restore_names_the_unreachable_repository() {
  let root = tempdir().unwrap();
  let password_file = root.path().join("password");
  fs::write(&password_file, "synthetic-password\n").unwrap();
  let repository = root.path().join("never-initialized");

  let target = root.path().join("restored");
  fs::create_dir(&target).unwrap();
  let error = restore(repository.to_str().unwrap(), &password_file, "latest", &target).unwrap_err();
  let cause = format!("{error:#}");
  assert!(cause.contains("unreachable"), "{cause}");
}

/// The policy `apply_retention` implements: the last snapshot of each of the last three calendar
/// days, and the last snapshot of each ISO week holding a snapshot at most fifteen days old.
fn policy_keeps(times: &[DateTime<Local>], now: DateTime<Local>) -> Vec<NaiveDate> {
  let mut keep: Vec<NaiveDate> = Vec::new();
  for day_offset in 0..3 {
    let date = now.date_naive() - ChronoDuration::days(day_offset);
    if let Some(latest) = times.iter().filter(|time| time.date_naive() == date).map(|time| time.date_naive()).max() {
      keep.push(latest);
    }
  }
  let cutoff = now - ChronoDuration::days(15);
  let mut weeks: Vec<(NaiveDate, DateTime<Local>)> = Vec::new();
  for time in times.iter().filter(|time| **time >= cutoff) {
    let week_start = time.date_naive() - ChronoDuration::days(time.weekday().num_days_from_monday() as i64);
    match weeks.iter_mut().find(|(start, _)| *start == week_start) {
      Some((_, latest)) if *latest < *time => *latest = *time,
      Some(_) => {}
      None => weeks.push((week_start, *time)),
    }
  }
  keep.extend(weeks.into_iter().map(|(_, latest)| latest.date_naive()));
  keep.sort();
  keep.dedup();
  keep
}

/// The snapshot dates `--time` stamps carry, read back with `restic snapshots --json`.
fn snapshot_dates(repository: &str, password_file: &Path) -> Vec<NaiveDate> {
  let output = restic(repository, password_file).args(["snapshots", "--json"]).output().unwrap();
  assert!(output.status.success(), "restic snapshots runs");
  let snapshots: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
  let mut dates: Vec<NaiveDate> = snapshots
    .iter()
    .map(|snapshot| {
      let time = snapshot["time"].as_str().unwrap();
      DateTime::parse_from_rfc3339(time).unwrap().with_timezone(&Local).date_naive()
    })
    .collect();
  dates.sort();
  dates.dedup();
  dates
}

#[test]
fn forget_keeps_three_daily_and_each_iso_weeks_last_snapshot() {
  let repo = new_repo();
  let (_root, home) = temp_home();
  write(&home, "config.yaml", b"server: {}\n");
  // The reviewed date set is 10-25, 11-01..11-06 with the forget run on 11-06; the offsets below are
  // that structure relative to the run's day, so the test holds on any day.
  let offsets = [12, 5, 4, 3, 2, 1, 0];
  let now = Local::now();
  let mut times: Vec<DateTime<Local>> = Vec::new();
  for offset in offsets {
    let date = now.date_naive() - ChronoDuration::days(offset);
    let stamp: NaiveDateTime = date.and_hms_opt(4, 45, 0).unwrap();
    let status = restic(&repo.repository, &repo.password_file)
      .args(["backup", &home.to_string_lossy(), "--quiet"])
      .arg("--time")
      .arg(stamp.format("%Y-%m-%d %H:%M:%S").to_string())
      .status()
      .unwrap();
    assert!(status.success(), "the timed backup for -{offset} runs");
    times.push(Local.from_local_datetime(&stamp).unwrap());
  }

  apply_retention(&repo.repository, &repo.password_file).unwrap();

  assert_eq!(snapshot_dates(&repo.repository, &repo.password_file), policy_keeps(&times, now));
}

/// Records the notices a `run_with_notice` test sends.
type Notices = Arc<Mutex<Vec<String>>>;

fn record_notice(notices: &Notices) -> impl Fn(String) -> std::future::Ready<()> + '_ {
  |message: String| {
    notices.lock().unwrap().push(message);
    std::future::ready(())
  }
}

#[tokio::test]
async fn a_missing_password_file_fails_the_preflight_and_notifies_once() {
  let root = tempdir().unwrap();
  let repository = root.path().join("repo");
  let config = BackupConfig {
    repository: Some(repository.to_string_lossy().into_owned()),
    password_file: Some(root.path().join("no-such-key").to_string_lossy().into_owned()),
    notify_user_id: Some("U0SYNTHETIC".to_string()),
  };
  let notices: Notices = Arc::new(Mutex::new(Vec::new()));
  let notice = record_notice(&notices);

  let result = run_with_notice(root.path().join("profile"), config, notice).await;

  assert!(result.is_err());
  let sent = notices.lock().unwrap();
  assert_eq!(sent.len(), 1, "exactly one notice: {sent:?}");
  assert!(sent[0].contains("preflight"), "{sent:?}");
  assert!(sent[0].contains("password file is missing"), "{sent:?}");
  drop(sent);
  assert!(!repository.exists(), "a failed run writes no repository or archive anywhere");
}

#[tokio::test]
async fn an_unreachable_repository_fails_the_backup_step_and_notifies_once() {
  let root = tempdir().unwrap();
  let repository = root.path().join("never-initialized");
  let password_file = root.path().join("password");
  fs::write(&password_file, "synthetic-password\n").unwrap();
  let home = root.path().join("profile");
  fs::create_dir(&home).unwrap();
  let config = backup_config(repository.to_str().unwrap(), &password_file);
  let notices: Notices = Arc::new(Mutex::new(Vec::new()));
  let notice = record_notice(&notices);

  let result = run_with_notice(home, config, notice).await;

  assert!(result.is_err());
  let sent = notices.lock().unwrap();
  assert_eq!(sent.len(), 1, "exactly one notice: {sent:?}");
  assert!(sent[0].contains("backup"), "{sent:?}");
  drop(sent);
  assert!(!repository.exists(), "a failed run writes no repository or archive anywhere");
}

#[tokio::test]
async fn a_successful_run_notifies_never() {
  let repo = new_repo();
  let (_root, home) = temp_home();
  write(&home, "config.yaml", b"server: {}\n");
  let config = backup_config(&repo.repository, &repo.password_file);
  let notices: Notices = Arc::new(Mutex::new(Vec::new()));
  let notice = record_notice(&notices);

  let result = run_with_notice(home, config, notice).await;

  let summary = result.unwrap();
  assert!(summary.starts_with("snapshot "), "{summary}");
  assert!(notices.lock().unwrap().is_empty(), "a successful run sends no notice");
}

/// Restic exits 3 when a source file cannot be read; the snapshot of the rest is kept and no notice is
/// sent. Run as root the file still reads and restic exits 0, and the assertions hold either way.
#[tokio::test]
async fn an_unreadable_source_file_keeps_the_snapshot_and_sends_no_notice() {
  let repo = new_repo();
  let (_root, home) = temp_home();
  write(&home, "config.yaml", b"server: {}\n");
  write(&home, "locked.txt", b"unreadable to restic\n");
  fs::set_permissions(home.join("locked.txt"), fs::Permissions::from_mode(0o000)).unwrap();
  let config = backup_config(&repo.repository, &repo.password_file);
  let notices: Notices = Arc::new(Mutex::new(Vec::new()));
  let notice = record_notice(&notices);

  let result = run_with_notice(home, config, notice).await;

  let summary = result.unwrap();
  assert!(summary.starts_with("snapshot "), "{summary}");
  assert_eq!(snapshot_dates(&repo.repository, &repo.password_file).len(), 1, "the partial snapshot is kept");
  assert!(notices.lock().unwrap().is_empty(), "a partial read is not a failed backup");
}
