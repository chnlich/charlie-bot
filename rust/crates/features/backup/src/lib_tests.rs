//! The behavior cases of `tests/test_backup.py`, retention cases pinned to calendar dates at each age
//! boundary, and the member fields Python's `tarfile` records.

use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use charliebot_core::home::CHARLIEBOT_HOME_ENV;
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
  days_from_civil(year, month, day) * 86_400 + 12 * 3_600
}

#[test]
fn retention_keeps_every_archive_younger_than_seven_days() {
  // Sunday 2026-10-18 at noon: the archives of 10-12 (age 6) and 10-18 (age 0).
  let now = noon(2026, 10, 18);
  let names = ["charliebot-20261012-000000.tar.gz", "charliebot-20261018-000000.tar.gz"];
  assert_eq!(surviving_after_retention(now, &names), names);
}

#[test]
fn retention_keeps_only_sundays_from_seven_to_thirty_days() {
  // Sunday 2026-10-18 at noon. Ages: 7 (Sun 10-11), 8 (Sat 10-10), 14 (Sun 10-04), 28 (Sun 09-20), 29 (Sat 09-19).
  let now = noon(2026, 10, 18);
  let names = [
    "charliebot-20260919-000000.tar.gz",
    "charliebot-20260920-000000.tar.gz",
    "charliebot-20261004-000000.tar.gz",
    "charliebot-20261010-000000.tar.gz",
    "charliebot-20261011-000000.tar.gz",
  ];
  let kept =
    ["charliebot-20260920-000000.tar.gz", "charliebot-20261004-000000.tar.gz", "charliebot-20261011-000000.tar.gz"];
  assert_eq!(surviving_after_retention(now, &names), kept);
}

#[test]
fn retention_keeps_only_month_starts_from_thirty_to_ninety_days() {
  // Tuesday 2026-10-20 at noon. Ages: 30 (Sun 09-20, not the 1st, so the Sunday rule does not apply),
  // 49 (09-01), 80 (08-01), 89 (07-23), 90 (07-22), 111 (07-01).
  let now = noon(2026, 10, 20);
  let names = [
    "charliebot-20260701-000000.tar.gz",
    "charliebot-20260722-000000.tar.gz",
    "charliebot-20260723-000000.tar.gz",
    "charliebot-20260801-000000.tar.gz",
    "charliebot-20260901-000000.tar.gz",
    "charliebot-20260920-000000.tar.gz",
  ];
  let kept = ["charliebot-20260801-000000.tar.gz", "charliebot-20260901-000000.tar.gz"];
  assert_eq!(surviving_after_retention(now, &names), kept);
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
