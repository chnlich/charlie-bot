//! The profile backup (`src/features/backup/backup.py`): a `.tar.gz` of the CharlieBot home in a
//! sibling directory, pruned by a tiered retention policy.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use charliebot_core::config::{CREDENTIALS_FILENAME, SESSIONS_DIR_NAME};
use charliebot_core::home::OPENCODE_DIR_NAME;
use charliebot_store::run_records::THREADS_DIR_NAME;
use chrono::{Datelike, Local, NaiveDateTime, Weekday};
use flate2::Compression;
use flate2::write::GzEncoder;

use crate::tar_writer::{Member, finish, python_code_points, write_member};

const BACKUP_PREFIX: &str = "charliebot-";
const BACKUP_SUFFIX: &str = ".tar.gz";

// Age thresholds in days.
pub(crate) const DAILY_THRESHOLD: i64 = 3;
pub(crate) const WEEKLY_THRESHOLD: i64 = 15;

/// The state directory being backed up: this profile's home (`config.charliebot_home_dir`).
pub fn charliebot_dir() -> anyhow::Result<PathBuf> {
  Ok(charliebot_core::home::charliebot_home_dir()?)
}

/// Where this profile's archives are written, a sibling of its home
/// (`backup_dir`): the default home yields `~/.charliebot_backup`.
pub fn backup_dir(home: &Path) -> anyhow::Result<PathBuf> {
  let name = home.file_name().with_context(|| format!("the home has no name: {}", home.display()))?;
  let mut backup_name = name.to_os_string();
  backup_name.push("_backup");
  Ok(home.with_file_name(backup_name))
}

/// True if the archive member (a relative path) should be excluded (`_should_exclude`). The name is judged
/// as bytes: a byte outside UTF-8 never equals an ASCII name part, so the decision matches Python's.
pub fn should_exclude(arcname: &[u8]) -> bool {
  let parts: Vec<&[u8]> = arcname.split(|byte| *byte == b'/').collect();
  // The profile's opencode store is the top-level directory only; a deeper part with the same name stays.
  if parts.first() == Some(&OPENCODE_DIR_NAME.as_bytes()) {
    return true;
  }
  for part in &parts {
    // CREDENTIALS_FILENAME is the profile's secrets file; it must never ride a
    // backup, which the retention policy keeps for up to 15 days.
    if *part == b".git" || *part == b".claude" || *part == CREDENTIALS_FILENAME.as_bytes() || *part == b"__pycache__" {
      return true;
    }
    if part.ends_with(b".pyc") {
      return true;
    }
  }
  // Exclude sessions/*/threads and everything under it.
  parts.len() >= 3 && parts[0] == SESSIONS_DIR_NAME.as_bytes() && parts[2] == THREADS_DIR_NAME.as_bytes()
}

/// The archive name of a naive local time.
pub(crate) fn archive_name(moment: NaiveDateTime) -> String {
  format!("{BACKUP_PREFIX}{}{BACKUP_SUFFIX}", moment.format("%Y%m%d-%H%M%S"))
}

/// Parses the naive local datetime of a backup filename like `charliebot-20260101-120000.tar.gz`
/// (`_parse_backup_date`), validating each field as `strptime` does.
pub(crate) fn parse_backup_date(name: &str) -> Option<NaiveDateTime> {
  let ts = name.strip_prefix(BACKUP_PREFIX).and_then(|ts| ts.strip_suffix(BACKUP_SUFFIX))?;
  let bytes = ts.as_bytes();
  if bytes.len() != 15 || bytes[8] != b'-' {
    return None;
  }
  if !bytes.iter().enumerate().all(|(index, byte)| index == 8 || byte.is_ascii_digit()) {
    return None;
  }
  let parsed = NaiveDateTime::parse_from_str(ts, "%Y%m%d-%H%M%S").ok()?;
  // Python datetime rejects year 0, which chrono accepts.
  if parsed.year() < 1 {
    return None;
  }
  Some(parsed)
}

/// The user's login name for a uid, or "" when no passwd entry exists (`KeyError`), as Python's `tarfile` does.
fn user_name(uid: u32) -> anyhow::Result<String> {
  let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
    .with_context(|| format!("looking up the passwd entry of uid {uid}"))?;
  Ok(user.map(|user| user.name).unwrap_or_default())
}

/// The group's name for a gid, or "" when no group entry exists (`KeyError`), as Python's `tarfile` does.
fn group_name(gid: u32) -> anyhow::Result<String> {
  let group = nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(gid))
    .with_context(|| format!("looking up the group entry of gid {gid}"))?;
  Ok(group.map(|group| group.name).unwrap_or_default())
}

/// The names in a directory, sorted as Python sorts `iterdir()` (`sorted(path.iterdir())`): by the code points
/// of each name, so a name that is not UTF-8 lands where Python puts it. A read error anywhere in the listing fails it.
fn sorted_names(dir: &Path) -> std::io::Result<Vec<OsString>> {
  let mut names =
    fs::read_dir(dir)?.map(|entry| entry.map(|entry| entry.file_name())).collect::<std::io::Result<Vec<_>>>()?;
  names.sort_by_cached_key(|name| python_code_points(name.as_bytes()));
  Ok(names)
}

fn mtime_fields(metadata: &fs::Metadata) -> (i64, u32) {
  let secs = metadata.mtime();
  (secs, metadata.mtime_nsec() as u32)
}

/// Creates a compressed backup of this profile's state directory (`create_backup`): the archive
/// `charliebot-<YYYYMMDD-HHMMSS>.tar.gz` (local time) in the sibling backup directory.
pub fn create_backup() -> anyhow::Result<PathBuf> {
  create_backup_in(&charliebot_dir()?)
}

/// [`create_backup`] against an explicit home, so tests pin the home without the process environment.
pub fn create_backup_in(home: &Path) -> anyhow::Result<PathBuf> {
  let target_dir = backup_dir(home)?;
  fs::create_dir_all(&target_dir).with_context(|| format!("creating the backup directory {}", target_dir.display()))?;
  let archive_name = archive_name(Local::now().naive_local());
  let archive_path = target_dir.join(archive_name);

  let raw_out =
    fs::File::create(&archive_path).with_context(|| format!("creating the archive {}", archive_path.display()))?;
  // The tar stream rides one gzip stream at the fast level, as isal's `igzip` level 1 does in Python;
  // the compressed bytes may differ, the members may not.
  let mut out = GzEncoder::new(raw_out, Compression::new(1));
  let mut bytes_written: u64 = 0;
  let mut hardlinks: HashMap<(u64, u64), Vec<u8>> = HashMap::new();

  // An unreadable home gives an empty archive (`backup_root_iter_failed`).
  let children = match sorted_names(home) {
    Ok(children) => children,
    Err(error) => {
      tracing::warn!(error = %error, "backup_root_iter_failed");
      Vec::new()
    }
  };
  for name in children {
    add_recursive(&mut out, &mut bytes_written, &mut hardlinks, &home.join(&name), name.as_bytes());
  }
  finish(&mut out, bytes_written)?;
  out.finish()?;

  let size_mb = fs::metadata(&archive_path)?.len() as f64 / (1024.0 * 1024.0);
  tracing::info!(path = %archive_path.display(), size_mb = (size_mb * 100.0).round() / 100.0, "backup_created");
  Ok(archive_path)
}

/// Adds one entry without recursion, then descends when the path is a directory
/// (`_add_recursive`; `is_dir` follows symlinks, as in Python).
fn add_recursive<W: Write>(
  out: &mut W,
  bytes_written: &mut u64,
  hardlinks: &mut HashMap<(u64, u64), Vec<u8>>,
  path: &Path,
  arcname: &[u8],
) {
  if should_exclude(arcname) {
    return;
  }
  match fs::symlink_metadata(path) {
    Ok(metadata) => {
      if let Err(error) = add_member(out, bytes_written, hardlinks, path, arcname, &metadata) {
        tracing::warn!(path = %path.display(), error = %error, "backup_skip_file");
        return;
      }
    }
    Err(error) => {
      tracing::warn!(path = %path.display(), error = %error, "backup_skip_file");
      return;
    }
  }
  let is_dir = matches!(fs::metadata(path), Ok(metadata) if metadata.is_dir());
  if !is_dir {
    return;
  }
  let children = match sorted_names(path) {
    Ok(children) => children,
    Err(error) => {
      tracing::warn!(path = %path.display(), error = %error, "backup_skip_dir");
      return;
    }
  };
  for name in children {
    let mut child_arcname = arcname.to_vec();
    child_arcname.push(b'/');
    child_arcname.extend_from_slice(name.as_bytes());
    add_recursive(out, bytes_written, hardlinks, &path.join(&name), &child_arcname);
  }
}

/// Adds one archive member with the metadata Python's `tarfile.add` records (`tarfile.gettarinfo`).
fn add_member<W: Write>(
  out: &mut W,
  bytes_written: &mut u64,
  hardlinks: &mut HashMap<(u64, u64), Vec<u8>>,
  path: &Path,
  arcname: &[u8],
  metadata: &fs::Metadata,
) -> anyhow::Result<()> {
  let mode = metadata.mode();
  let (mtime_sec, mtime_nsec) = mtime_fields(metadata);
  let file_type = metadata.file_type();
  let (typeflag, linkname, size): (u8, Vec<u8>, u64) = if file_type.is_symlink() {
    (b'2', fs::read_link(path)?.into_os_string().into_vec(), 0)
  } else if metadata.is_dir() {
    (b'5', Vec::new(), 0)
  } else if metadata.is_file() {
    // A hard link to an already archived inode becomes a LNKTYPE member, as Python's `tarfile` does.
    let inode = (metadata.dev(), metadata.ino());
    if metadata.nlink() > 1 {
      if let Some(first) = hardlinks.get(&inode) {
        (b'1', first.clone(), 0)
      } else {
        hardlinks.insert(inode, arcname.to_vec());
        (b'0', Vec::new(), metadata.len())
      }
    } else {
      hardlinks.insert(inode, arcname.to_vec());
      (b'0', Vec::new(), metadata.len())
    }
  } else if metadata.file_type().is_char_device() {
    (b'3', Vec::new(), 0)
  } else if file_type.is_block_device() {
    (b'4', Vec::new(), 0)
  } else if file_type.is_fifo() {
    (b'6', Vec::new(), 0)
  } else {
    // gettarinfo returns None for a socket, and tar.add skips it.
    return Ok(());
  };
  // Python's `get_info` writes a directory's name with its trailing slash.
  let mut name = arcname.to_vec();
  if typeflag == b'5' {
    name.push(b'/');
  }
  let uname = user_name(metadata.uid())?;
  let gname = group_name(metadata.gid())?;
  let member = Member {
    name: &name,
    mode,
    uid: metadata.uid(),
    gid: metadata.gid(),
    size,
    mtime_sec,
    mtime_nsec,
    typeflag,
    linkname: &linkname,
    uname: uname.as_bytes(),
    gname: gname.as_bytes(),
    devmajor: if typeflag == b'3' || typeflag == b'4' { (metadata.rdev() >> 8) as u32 } else { 0 },
    devminor: if typeflag == b'3' || typeflag == b'4' { (metadata.rdev() & 0xff) as u32 } else { 0 },
  };
  match typeflag {
    b'0' => {
      let file = fs::File::open(path)?;
      let wrote = write_member(out, &member, Some(&mut Box::new(file) as &mut dyn Read))?;
      *bytes_written += wrote;
    }
    _ => *bytes_written += write_member(out, &member, None)?,
  }
  Ok(())
}

/// Applies the tiered retention policy to this profile's backups (`apply_retention`).
pub fn apply_retention() -> anyhow::Result<()> {
  apply_retention_in(&charliebot_dir()?)
}

/// [`apply_retention`] against an explicit home, so tests pin the home without the process environment.
pub fn apply_retention_in(home: &Path) -> anyhow::Result<()> {
  apply_retention_in_with_now(home, Local::now().naive_local().and_utc().timestamp());
  Ok(())
}

/// [`apply_retention_in`] with the naive local now given in seconds, so tests pin the clock.
pub fn apply_retention_in_with_now(home: &Path, now: i64) {
  let Ok(target_dir) = backup_dir(home) else {
    return;
  };
  if !target_dir.exists() {
    return;
  }
  let entries: Vec<PathBuf> = match fs::read_dir(&target_dir) {
    Ok(entries) => {
      let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
          path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(BACKUP_PREFIX) && name.ends_with(BACKUP_SUFFIX))
        })
        .collect();
      paths.sort();
      paths
    }
    Err(error) => {
      tracing::warn!(path = %target_dir.display(), error = %error, "backup_root_iter_failed");
      return;
    }
  };
  for backup_file in entries {
    let Some(name) = backup_file.file_name().and_then(|name| name.to_str()) else {
      continue;
    };
    let Some(parsed) = parse_backup_date(name) else {
      tracing::debug!(name, error = "not a backup timestamp", "backup_parse_date_failed");
      tracing::debug!(name, "backup_retention_skip_unparseable");
      continue;
    };
    let backup_naive = parsed.and_utc().timestamp();
    // Python's `timedelta.days` floors toward negative infinity.
    let age = (now - backup_naive).div_euclid(86_400);
    let keep = if age < DAILY_THRESHOLD {
      true
    } else if age < WEEKLY_THRESHOLD {
      parsed.weekday() == Weekday::Sun
    } else {
      false
    };
    if !keep {
      match fs::remove_file(&backup_file) {
        Ok(()) => tracing::info!(name, age_days = age, "backup_deleted"),
        Err(error) => tracing::warn!(name, error = %error, "backup_delete_failed"),
      }
    }
  }
}

/// The built-in cron handler body (`run_scheduled_backup`): create a backup and apply retention.
pub fn run_backup_blocking() -> anyhow::Result<PathBuf> {
  let archive = create_backup()?;
  apply_retention()?;
  Ok(archive)
}
