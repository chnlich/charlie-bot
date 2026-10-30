//! The home writer fence: the exclusion a server holds over its home (`src/runtime/home_writer_fence.py`).
//!
//! The exclusion is an exclusive non-blocking `flock` on `<home>/state/home_writer.lock`, held for the
//! holder's lifetime; the kernel drops it when the holder dies. `<home>/state/writer_identity.json`
//! names the holder for a refused caller and is evidence only. A Python server, a migration apply and
//! this server take the same lock file the same way, so each refuses while another holds the home.
//! A symlinked state directory or fence file refuses acquisition.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg, OFlag};
use serde_json::{Map, Value, json};

use crate::{fsutil, paths, proc_identity, text_repr, timefmt};

pub const STATE_DIR_NAME: &str = "state";
pub const FENCE_LOCK_NAME: &str = "home_writer.lock";
pub const FENCE_IDENTITY_NAME: &str = "writer_identity.json";

pub fn fence_lock_path(home: &Path) -> PathBuf {
  home.join(STATE_DIR_NAME).join(FENCE_LOCK_NAME)
}

pub fn fence_identity_path(home: &Path) -> PathBuf {
  home.join(STATE_DIR_NAME).join(FENCE_IDENTITY_NAME)
}

/// The recorded identity of the fence's holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FenceHolder {
  pub pid: i64,
  pub pid_start: Option<String>,
  pub started_at: String,
  pub purpose: String,
  pub argv: String,
  pub home: String,
}

/// A refused acquisition.
#[derive(Debug)]
pub enum FenceError {
  /// Another live process holds the fence; `holder` is `None` when its identity record is unreadable.
  Active { holder: Option<Box<FenceHolder>>, home: PathBuf, purpose: String },
  /// A fence path is unsafe or unusable (`FencePathRefusalError`).
  PathRefusal(String),
  /// Writing the identity record failed; the lock was released before returning.
  Identity(std::io::Error),
}

impl fmt::Display for FenceError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      FenceError::Active { holder, home, purpose } => {
        let detail = match holder {
          Some(holder) => format!(
            "home writer fence held by pid {} (started {}, purpose {}, argv: {})",
            holder.pid,
            holder.started_at,
            text_repr::str_repr(&holder.purpose),
            holder.argv
          ),
          None => "home writer fence is held (holder identity unreadable)".to_string(),
        };
        write!(f, "{purpose} refused for home {}: {detail}", paths::display(home))
      }
      FenceError::PathRefusal(message) => f.write_str(message),
      FenceError::Identity(error) => write!(f, "home writer fence identity record could not be written: {error}"),
    }
  }
}

impl std::error::Error for FenceError {}

/// An acquired fence. Dropping it releases the exclusion, so every exit path of a live process lets go.
pub struct HomeWriterFence {
  home: PathBuf,
  lock: Option<Flock<File>>,
}

impl HomeWriterFence {
  /// Drop the identity record, then the lock (idempotent).
  ///
  /// The record is unlinked while this holder still owns the lock, so a successor that acquires
  /// afterwards cannot have its own record erased by this release.
  pub fn release(&mut self) {
    let Some(lock) = self.lock.take() else {
      return;
    };
    let identity = fence_identity_path(&self.home);
    if identity.symlink_metadata().is_ok()
      && let Err(error) = std::fs::remove_file(&identity)
    {
      tracing::error!(path = %paths::display(&identity), error = %error, "home_writer_identity_unlink_failed");
    }
    match lock.unlock() {
      Ok(file) => drop(file),
      Err((_lock, errno)) => tracing::error!(error = %errno, "home_writer_fence_unlock_failed"),
    }
  }
}

impl Drop for HomeWriterFence {
  fn drop(&mut self) {
    self.release();
  }
}

/// Take the home's exclusive writer fence, or refuse with the holder's details.
///
/// The lock file is created if absent. The identity record is written after the lock is held; a failed
/// write releases the lock before the error returns, so a half-acquired fence never stays behind.
pub fn acquire_home_writer_fence(home: &Path, purpose: &str) -> Result<HomeWriterFence, FenceError> {
  checked_fence_paths(home)?;
  let lock_path = fence_lock_path(home);
  let state_dir = home.join(STATE_DIR_NAME);
  std::fs::create_dir_all(&state_dir).map_err(|error| {
    FenceError::PathRefusal(format!("home writer fence lock is not usable at {}: {error}", paths::display(&lock_path)))
  })?;
  let file = OpenOptions::new()
    .read(true)
    .write(true)
    .create(true)
    .truncate(false)
    .mode(0o600)
    .custom_flags(OFlag::O_NOFOLLOW.bits())
    .open(&lock_path)
    .map_err(|error| {
      FenceError::PathRefusal(format!(
        "home writer fence lock is not usable at {}: {error}",
        paths::display(&lock_path)
      ))
    })?;
  let lock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
    Ok(lock) => lock,
    Err((_file, _errno)) => {
      return Err(FenceError::Active {
        holder: read_fence_holder(home).map(Box::new),
        home: home.to_path_buf(),
        purpose: purpose.to_string(),
      });
    }
  };
  let mut fence = HomeWriterFence { home: home.to_path_buf(), lock: Some(lock) };
  let pid = i64::from(std::process::id());
  let holder = FenceHolder {
    pid,
    pid_start: proc_identity::read_pid_stat(std::process::id()).map(|stat| stat.pid_start),
    started_at: timefmt::utc_now_iso(),
    purpose: purpose.to_string(),
    argv: std::env::args().collect::<Vec<_>>().join(" "),
    home: paths::display(home),
  };
  let record = json!({
    "pid": holder.pid,
    "pid_start": holder.pid_start,
    "started_at": holder.started_at,
    "purpose": holder.purpose,
    "argv": holder.argv,
    "home": holder.home,
  });
  if let Err(error) =
    fsutil::atomic_write_text(&fence_identity_path(home), &text_repr::json_dumps(&record, &text_repr::FENCE_RECORD_JSON))
  {
    // Only the lock is ours to drop here: the record never landed.
    if let Some(lock) = fence.lock.take() {
      drop(lock.unlock());
    }
    return Err(FenceError::Identity(error));
  }
  tracing::info!(home = %paths::display(home), purpose = purpose, pid = pid, "home_writer_fence_acquired");
  Ok(fence)
}

/// The identity record's holder, or `None` when the record is absent or unreadable.
pub fn read_fence_holder(home: &Path) -> Option<FenceHolder> {
  let text = std::fs::read_to_string(fence_identity_path(home)).ok()?;
  let Value::Object(raw) = serde_json::from_str::<Value>(&text).ok()? else {
    return None;
  };
  let pid = match raw.get("pid")? {
    Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|float| float.trunc() as i64))?,
    Value::String(text) => text.trim().parse().ok()?,
    Value::Bool(flag) => i64::from(*flag),
    _ => return None,
  };
  let text_field = |raw: &Map<String, Value>, key: &str| raw.get(key).map_or_else(String::new, text_repr::python_str);
  Some(FenceHolder {
    pid,
    pid_start: raw.get("pid_start").and_then(Value::as_str).map(str::to_string),
    started_at: text_field(&raw, "started_at"),
    purpose: text_field(&raw, "purpose"),
    argv: text_field(&raw, "argv"),
    home: text_field(&raw, "home"),
  })
}

/// Refuse fence paths that would place or read the exclusion outside the home.
fn checked_fence_paths(home: &Path) -> Result<(), FenceError> {
  let state = home.join(STATE_DIR_NAME);
  let state_metadata = state.symlink_metadata();
  if state_metadata.as_ref().is_ok_and(|metadata| metadata.file_type().is_symlink()) {
    return Err(FenceError::PathRefusal(format!("home state directory is a symlink: {}", paths::display(&state))));
  }
  if state.exists() && !state.is_dir() {
    return Err(FenceError::PathRefusal(format!("home state path is not a directory: {}", paths::display(&state))));
  }
  if !paths::is_within(&paths::realpath(&state), &paths::realpath(home)) {
    return Err(FenceError::PathRefusal(format!(
      "home state directory resolves outside the home: {}",
      paths::display(&state)
    )));
  }
  for path in [fence_lock_path(home), fence_identity_path(home)] {
    if path.symlink_metadata().is_ok_and(|metadata| metadata.file_type().is_symlink()) {
      return Err(FenceError::PathRefusal(format!("home writer fence path is a symlink: {}", paths::display(&path))));
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_second_acquisition_refuses_and_names_the_holder() {
    let home = tempfile::tempdir().unwrap();
    let mut first = acquire_home_writer_fence(home.path(), "server startup").unwrap();
    let record: Value =
      serde_json::from_str(&std::fs::read_to_string(fence_identity_path(home.path())).unwrap()).unwrap();
    assert_eq!(record["pid"], json!(std::process::id()));
    assert_eq!(record["purpose"], json!("server startup"));
    match acquire_home_writer_fence(home.path(), "test") {
      Err(error @ FenceError::Active { .. }) => {
        let message = error.to_string();
        assert!(message.starts_with("test refused for home "), "{message}");
        assert!(
          message.contains(&format!("held by pid {} (started ", std::process::id()))
            && message.contains("purpose 'server startup'"),
          "{message}"
        );
      }
      other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
    }
    first.release();
    assert!(!fence_identity_path(home.path()).exists());
    let again = acquire_home_writer_fence(home.path(), "test").unwrap();
    drop(again);
    assert!(!fence_identity_path(home.path()).exists(), "dropping the fence releases it");
  }

  #[test]
  fn a_symlinked_state_directory_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), home.path().join(STATE_DIR_NAME)).unwrap();
    let error = acquire_home_writer_fence(home.path(), "test").err().expect("a symlinked state dir refuses");
    assert!(error.to_string().starts_with("home state directory is a symlink: "), "{error}");
  }
}
