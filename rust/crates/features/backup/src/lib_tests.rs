//! The behavior cases of `tests/test_backup.py`, plus retention cases over synthetic archive names at
//! each age boundary.

use std::fs;
use std::io::Read;
use std::path::Path;

use charliebot_core::home::CHARLIEBOT_HOME_ENV;
use tempfile::tempdir;

use crate::backup::*;

/// Reads one member's content out of a `.tar.gz` archive, by name.
fn member_content(archive: &Path, name: &str) -> Option<Vec<u8>> {
  let file = fs::File::open(archive).unwrap();
  let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
  for entry in archive.entries().unwrap() {
    let mut entry = entry.unwrap();
    if entry.path().unwrap().to_string_lossy() == name {
      let mut bytes = Vec::new();
      entry.read_to_end(&mut bytes).unwrap();
      return Some(bytes);
    }
  }
  None
}

/// The member names of a `.tar.gz` archive, in archive order.
fn member_names(archive: &Path) -> Vec<String> {
  let file = fs::File::open(archive).unwrap();
  let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
  let names: Vec<String> =
    archive.entries().unwrap().map(|entry| entry.unwrap().path().unwrap().to_string_lossy().into_owned()).collect();
  names
}

#[test]
fn exclusions_cover_the_secrets_file_and_noise() {
  assert!(should_exclude("credentials.yaml"));
  assert!(should_exclude(".git/HEAD"));
  assert!(should_exclude("__pycache__/x.pyc"));
  assert!(should_exclude("cache/x.pyc"));
  assert!(should_exclude("sessions/s1/threads/t1/metadata.json"));
  assert!(!should_exclude("config.yaml"));
  assert!(!should_exclude("sessions/s1/triggers/t1.json"));
}

#[test]
fn create_backup_omits_the_secrets_file() {
  let home = tempdir().unwrap();
  fs::write(home.path().join("credentials.yaml"), "credentials:\n  access_key: secret\n").unwrap();
  fs::write(home.path().join("config.yaml"), "server: {}\n").unwrap();

  let archive = create_backup_in(home.path()).unwrap();
  let names = member_names(&archive);

  assert!(names.iter().any(|name| name == "config.yaml"));
  assert!(names.iter().all(|name| !name.contains("credentials.yaml")));
}

#[test]
fn create_backup_archive_round_trips_bytes() {
  let home = tempdir().unwrap();
  let payload = "key: value\nlist:\n  - a\n  - b\n";
  fs::write(home.path().join("config.yaml"), payload).unwrap();

  let archive = create_backup_in(home.path()).unwrap();
  assert_eq!(member_content(&archive, "config.yaml").unwrap(), payload.as_bytes());
}

#[test]
fn create_backup_leaves_out_only_the_top_level_opencode_store() {
  let profile_root = tempdir().unwrap();
  let profile = profile_root.path().join("profile");
  let deep = profile.join("opencode/data/opencode");
  fs::create_dir_all(&deep).unwrap();
  fs::write(deep.join("opencode.db"), b"db").unwrap();
  let session = profile.join("sessions/s1/opencode");
  fs::create_dir_all(&session).unwrap();
  fs::write(session.join("note.json"), "{}").unwrap();

  let archive = create_backup_in(&profile).unwrap();
  let names = member_names(&archive);

  assert!(names.iter().all(|name| name != "opencode" && !name.starts_with("opencode/")));
  assert!(names.iter().any(|name| name == "sessions/s1/opencode/note.json"));
}

#[test]
fn create_backup_reads_the_profile_home_from_the_environment() {
  let profile_root = tempdir().unwrap();
  let profile = profile_root.path().join("profile");
  fs::create_dir_all(&profile).unwrap();
  fs::write(profile.join("config.yaml"), "server: {}\n").unwrap();
  // One env-var writer per process: the test body is the only code that reads the variable.
  unsafe { std::env::set_var(CHARLIEBOT_HOME_ENV, &profile) };
  let archive = create_backup().unwrap();
  unsafe { std::env::remove_var(CHARLIEBOT_HOME_ENV) };
  assert!(member_names(&archive).iter().any(|name| name == "config.yaml"));
  assert_eq!(archive.parent().unwrap(), profile_root.path().join("profile_backup"));
}

#[test]
fn create_backup_names_the_archive_by_local_time() {
  let home = tempdir().unwrap();
  fs::write(home.path().join("config.yaml"), "server: {}\n").unwrap();
  let archive = create_backup_in(home.path()).unwrap();
  let name = archive.file_name().unwrap().to_str().unwrap();
  assert!(name.starts_with("charliebot-") && name.ends_with(".tar.gz"));
  assert_eq!(name.len(), "charliebot-YYYYMMDD-HHMMSS.tar.gz".len());
  assert!(parse_backup_date(name).is_some());
}

/// A backup archive name for `days_ago` days before `now`, at midnight, in naive local time.
fn name_for_days_ago(now: i64, days_ago: i64) -> String {
  let (year, month, day) = civil_from_days((now - days_ago * 86_400).div_euclid(86_400));
  archive_name(year, month, day, 0, 0, 0)
}

fn write_archive(dir: &Path, name: &str) {
  fs::write(dir.join(name), b"archive").unwrap();
}

#[test]
fn retention_keeps_archives_younger_than_seven_days() {
  let home = tempdir().unwrap();
  let now = naive_now_for_tests();
  let backups = backup_dir(home.path()).unwrap();
  fs::create_dir_all(&backups).unwrap();
  write_archive(&backups, &name_for_days_ago(now, 0));
  write_archive(&backups, &name_for_days_ago(now, 6));

  apply_retention_in_with_now(home.path(), now);

  let kept: Vec<_> = fs::read_dir(&backups).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
  assert_eq!(kept.len(), 2);
}

#[test]
fn retention_keeps_only_sundays_from_seven_to_thirty_days() {
  let home = tempdir().unwrap();
  let now = naive_now_for_tests();
  let today = now.div_euclid(86_400);
  let sunday = (8..WEEKLY_THRESHOLD).find(|days| weekday(today - days) == 6).unwrap();
  let non_sunday = (8..WEEKLY_THRESHOLD).find(|days| weekday(today - days) != 6).unwrap();
  let backups = backup_dir(home.path()).unwrap();
  fs::create_dir_all(&backups).unwrap();
  write_archive(&backups, &name_for_days_ago(now, sunday));
  write_archive(&backups, &name_for_days_ago(now, non_sunday));

  apply_retention_in_with_now(home.path(), now);

  let kept: Vec<_> = fs::read_dir(&backups).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
  assert_eq!(kept, vec![name_for_days_ago(now, sunday)]);
}

#[test]
fn retention_keeps_only_month_starts_from_thirty_to_ninety_days() {
  let home = tempdir().unwrap();
  let now = naive_now_for_tests();
  let today = now.div_euclid(86_400);
  let month_start = (30..MONTHLY_THRESHOLD).find(|days| civil_from_days(today - days).2 == 1).unwrap();
  let mid_month = (30..MONTHLY_THRESHOLD).find(|days| civil_from_days(today - days).2 != 1).unwrap();
  let backups = backup_dir(home.path()).unwrap();
  fs::create_dir_all(&backups).unwrap();
  write_archive(&backups, &name_for_days_ago(now, month_start));
  write_archive(&backups, &name_for_days_ago(now, mid_month));

  apply_retention_in_with_now(home.path(), now);

  let kept: Vec<_> = fs::read_dir(&backups).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
  assert_eq!(kept, vec![name_for_days_ago(now, month_start)]);
}

#[test]
fn retention_deletes_everything_ninety_days_old() {
  let home = tempdir().unwrap();
  let now = naive_now_for_tests();
  let backups = backup_dir(home.path()).unwrap();
  fs::create_dir_all(&backups).unwrap();
  write_archive(&backups, &name_for_days_ago(now, 89));
  write_archive(&backups, &name_for_days_ago(now, 90));
  write_archive(&backups, &name_for_days_ago(now, 400));

  apply_retention_in_with_now(home.path(), now);

  assert_eq!(fs::read_dir(&backups).unwrap().count(), 0);
}

#[test]
fn retention_skips_unparseable_names() {
  let home = tempdir().unwrap();
  let now = naive_now_for_tests();
  let backups = backup_dir(home.path()).unwrap();
  fs::create_dir_all(&backups).unwrap();
  write_archive(&backups, "charliebot-not-a-date.tar.gz");
  write_archive(&backups, "charliebot-20260230-120000.tar.gz");
  write_archive(&backups, "other-file.tar.gz");

  apply_retention_in_with_now(home.path(), now);

  assert_eq!(fs::read_dir(&backups).unwrap().count(), 3);
}

#[test]
fn retention_without_a_backup_directory_is_a_no_op() {
  let home = tempdir().unwrap();
  apply_retention_in_with_now(home.path(), naive_now_for_tests());
}

/// The naive local now the retention tests pin their clock against.
fn naive_now_for_tests() -> i64 {
  let tm = local_now();
  naive_epoch_from_tm(&tm)
}
