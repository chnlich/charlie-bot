//! The behavior cases of `tests/test_backup.py`, retention cases pinned to calendar dates at each age
//! boundary, and the member fields Python's `tarfile` records.

use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use charliebot_core::home::CHARLIEBOT_HOME_ENV;
use chrono::{Duration as ChronoDuration, NaiveDate};
use tempfile::{TempDir, tempdir};

use crate::backup::*;
use crate::tar_writer::{Member, write_member};

/// A home inside a fresh temporary root. Its `_backup` sibling is created inside the same root, so dropping
/// the root removes every archive a test writes.
fn temp_home() -> (TempDir, PathBuf) {
  let root = tempdir().unwrap();
  let home = root.path().join("profile");
  fs::create_dir(&home).unwrap();
  (root, home)
}

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

/// The member names of a `.tar.gz` archive as raw bytes, in archive order.
fn member_name_bytes(archive: &Path) -> Vec<Vec<u8>> {
  let file = fs::File::open(archive).unwrap();
  let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
  archive.entries().unwrap().map(|entry| entry.unwrap().path_bytes().into_owned()).collect()
}

#[test]
fn exclusions_cover_the_secrets_file_and_noise() {
  assert!(should_exclude(b"credentials.yaml"));
  assert!(should_exclude(b".git/HEAD"));
  assert!(should_exclude(b"__pycache__/x.pyc"));
  assert!(should_exclude(b"cache/x.pyc"));
  assert!(should_exclude(b"sessions/s1/threads/t1/metadata.json"));
  assert!(should_exclude(b"raw-\xff.pyc"));
  assert!(!should_exclude(b"config.yaml"));
  assert!(!should_exclude(b"sessions/s1/triggers/t1.json"));
  assert!(!should_exclude(b"raw-\xff.txt"));
}

#[test]
fn create_backup_omits_the_secrets_file() {
  let (_root, home) = temp_home();
  fs::write(home.join("credentials.yaml"), "credentials:\n  access_key: secret\n").unwrap();
  fs::write(home.join("config.yaml"), "server: {}\n").unwrap();

  let archive = create_backup_in(&home).unwrap();
  let names = member_names(&archive);

  assert!(names.iter().any(|name| name == "config.yaml"));
  assert!(names.iter().all(|name| !name.contains("credentials.yaml")));
}

#[test]
fn create_backup_archive_round_trips_bytes() {
  let (_root, home) = temp_home();
  let payload = "key: value\nlist:\n  - a\n  - b\n";
  fs::write(home.join("config.yaml"), payload).unwrap();

  let archive = create_backup_in(&home).unwrap();
  assert_eq!(member_content(&archive, "config.yaml").unwrap(), payload.as_bytes());
}

#[test]
fn create_backup_archives_names_that_are_not_utf8_as_their_bytes() {
  let (_root, home) = temp_home();
  fs::write(home.join(OsStr::from_bytes(b"raw-\xff.txt")), "top\n").unwrap();
  fs::create_dir(home.join("nested")).unwrap();
  fs::write(home.join("nested").join(OsStr::from_bytes(b"bad-\xc3(.txt")), "nested\n").unwrap();

  // Every entry is present, in Python's order: a read that skips or fails on a name would drop one of them.
  let archive = create_backup_in(&home).unwrap();
  assert_eq!(
    member_name_bytes(&archive),
    [b"nested/".to_vec(), b"nested/bad-\xc3(.txt".to_vec(), b"raw-\xff.txt".to_vec()]
  );
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
  let (_root, home) = temp_home();
  fs::write(home.join("config.yaml"), "server: {}\n").unwrap();
  let archive = create_backup_in(&home).unwrap();
  let name = archive.file_name().unwrap().to_str().unwrap();
  assert!(name.starts_with("charliebot-") && name.ends_with(".tar.gz"));
  assert_eq!(name.len(), "charliebot-YYYYMMDD-HHMMSS.tar.gz".len());
  assert!(parse_backup_date(name).is_some());
}

#[test]
fn backup_name_and_date_parser_match_python_timestamp_bytes() {
  let moment = NaiveDate::from_ymd_opt(2026, 10, 26).unwrap().and_hms_opt(9, 7, 5).unwrap();
  let name = archive_name(moment);
  assert_eq!(name, "charliebot-20261026-090705.tar.gz");
  assert_eq!(parse_backup_date(&name), Some(moment));
  assert_eq!(parse_backup_date("charliebot-00000101-000000.tar.gz"), None);
}

#[test]
fn create_backup_records_the_permission_bits_and_directory_slashes() {
  let (_root, home) = temp_home();
  let config = home.join("config.yaml");
  fs::write(&config, "server: {}\n").unwrap();
  fs::set_permissions(&config, fs::Permissions::from_mode(0o640)).unwrap();
  fs::create_dir(home.join("sub")).unwrap();

  let archive = create_backup_in(&home).unwrap();
  let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(&archive).unwrap()));
  let mut members = Vec::new();
  for entry in tar.entries().unwrap() {
    let entry = entry.unwrap();
    members.push((entry.path_bytes().into_owned(), entry.header().mode().unwrap()));
  }

  // Python's `get_info` stores the permission bits only, and a directory's name ends with a slash.
  assert!(members.contains(&(b"config.yaml".to_vec(), 0o640)));
  assert!(members.iter().any(|(name, _)| name.as_slice() == b"sub/"));
}

#[test]
fn write_member_moves_a_uid_past_the_ustar_field_to_a_pax_record() {
  let member = Member {
    name: b"owned.txt",
    mode: 0o100644,
    uid: 2_097_152,
    gid: 0,
    size: 0,
    mtime_sec: 1_700_000_000,
    mtime_nsec: 0,
    typeflag: b'0',
    linkname: b"",
    uname: b"",
    gname: b"",
    devmajor: 0,
    devminor: 0,
  };
  let mut out = Vec::new();
  write_member(&mut out, &member, None).unwrap();

  // The member's own header ends the output: its uid field holds 0, and the PAX entry before it carries the uid.
  let header = &out[out.len() - 512..];
  assert_eq!(&header[108..116], b"0000000\0");
  assert!(String::from_utf8_lossy(&out).contains("uid=2097152\n"));
}

/// Writes the archives named `names` into a fresh backup directory, runs one retention pass at `now`, and
/// returns the names that survive, sorted.
fn surviving_after_retention(now: i64, names: &[&str]) -> Vec<String> {
  let root = tempdir().unwrap();
  let home = root.path().join("profile");
  let backups = backup_dir(&home).unwrap();
  fs::create_dir_all(&backups).unwrap();
  for name in names {
    write_archive(&backups, name);
  }

  apply_retention_in_with_now(&home, now);

  let mut kept: Vec<String> =
    fs::read_dir(&backups).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
  kept.sort();
  kept
}

fn write_archive(dir: &Path, name: &str) {
  fs::write(dir.join(name), b"archive").unwrap();
}

/// Noon of a local calendar date, in the naive-local seconds the retention pass compares against.
fn noon(year: i64, month: i64, day: i64) -> i64 {
  naive_epoch(year as i32, month as u32, day as u32, 12, 0, 0)
}

fn naive_epoch(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
  NaiveDate::from_ymd_opt(year, month, day).unwrap().and_hms_opt(hour, minute, second).unwrap().and_utc().timestamp()
}

#[test]
fn retention_keeps_every_archive_younger_than_three_days() {
  // Sunday 2026-10-18 at noon: the archives of 10-16 (age 2) and 10-18 (age 0) stay. The 10-15 archive (age 3, a
  // Thursday) goes, and so does 10-14 (age 4).
  let now = noon(2026, 10, 18);
  let names = [
    "charliebot-20261014-000000.tar.gz",
    "charliebot-20261015-000000.tar.gz",
    "charliebot-20261016-000000.tar.gz",
    "charliebot-20261018-000000.tar.gz",
  ];
  let kept = ["charliebot-20261016-000000.tar.gz", "charliebot-20261018-000000.tar.gz"];
  assert_eq!(surviving_after_retention(now, &names), kept);
}

#[test]
fn retention_keeps_only_sundays_from_three_to_fifteen_days() {
  // Sunday 2026-10-18 at noon. Ages: 1 (Sat 10-17, kept as younger than three days), 3 (Thu 10-15), 7 (Sun 10-11),
  // 14 (Sun 10-04), 15 (Sat 10-03), 21 (Sun 09-27).
  let now = noon(2026, 10, 18);
  let names = [
    "charliebot-20260927-000000.tar.gz",
    "charliebot-20261003-000000.tar.gz",
    "charliebot-20261004-000000.tar.gz",
    "charliebot-20261011-000000.tar.gz",
    "charliebot-20261015-000000.tar.gz",
    "charliebot-20261017-000000.tar.gz",
  ];
  let kept =
    ["charliebot-20261004-000000.tar.gz", "charliebot-20261011-000000.tar.gz", "charliebot-20261017-000000.tar.gz"];
  assert_eq!(surviving_after_retention(now, &names), kept);
}

#[test]
fn retention_deletes_first_of_month_archives_older_than_fifteen_days() {
  // Sunday 2026-10-18 at noon. The 1st of September (age 47) and of August (age 78) go, since no monthly tier
  // keeps them. The 10-16 archive (age 2) stays.
  let now = noon(2026, 10, 18);
  let names =
    ["charliebot-20260801-000000.tar.gz", "charliebot-20260901-000000.tar.gz", "charliebot-20261016-000000.tar.gz"];
  let kept = ["charliebot-20261016-000000.tar.gz"];
  assert_eq!(surviving_after_retention(now, &names), kept);
}

#[test]
fn retention_keeps_the_last_three_days_and_recent_sundays() {
  // The scheduled run stamps each day's archive at 04:45 and applies retention at 04:50 on Wednesday 2026-11-04.
  // Of forty daily archives, the three days before the run stay, and so do the Sundays 11-01 (age 3) and 10-25
  // (age 10). The rest go.
  let now = naive_epoch(2026, 11, 4, 4, 50, 0);
  let names: Vec<String> = (0..40).map(scheduled_archive).collect();
  let names: Vec<&str> = names.iter().map(String::as_str).collect();
  let kept = [
    "charliebot-20261025-044500.tar.gz",
    "charliebot-20261101-044500.tar.gz",
    "charliebot-20261102-044500.tar.gz",
    "charliebot-20261103-044500.tar.gz",
    "charliebot-20261104-044500.tar.gz",
  ];
  assert_eq!(surviving_after_retention(now, &names), kept);
}

/// The archive the scheduled run writes at 04:45 on the civil day `days_ago` before 2026-11-04.
fn scheduled_archive(days_ago: i64) -> String {
  let date = NaiveDate::from_ymd_opt(2026, 11, 4).unwrap() - ChronoDuration::days(days_ago);
  format!("charliebot-{}-044500.tar.gz", date.format("%Y%m%d"))
}

#[test]
fn retention_skips_unparseable_names() {
  // Kept, with no delete attempt: a year 0 that `datetime` rejects, a date `strptime` rejects, and a
  // name with no timestamp.
  let now = noon(2026, 10, 18);
  let names = [
    "charliebot-00000101-000000.tar.gz",
    "charliebot-20260230-120000.tar.gz",
    "charliebot-not-a-date.tar.gz",
    "other-file.tar.gz",
  ];
  assert_eq!(surviving_after_retention(now, &names), names);
}

#[test]
fn retention_without_a_backup_directory_is_a_no_op() {
  let home = tempdir().unwrap();
  apply_retention_in_with_now(home.path(), noon(2026, 10, 18));
}
