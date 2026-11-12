//! The persisted owner of backend usage availability: one "out until" row per backend option id.
//!
//! Every quota cooling record, the backend pool's included, lands here and survives a restart: the state lives in
//! `$CHARLIEBOT_HOME/state/backend_availability.json`, written atomically, and a startup reads it back. Every process
//! (the server and each CLI host) reads the file at each call and merges its writes under a file lock. A record one
//! process writes is visible to the next read of every other process. No write drops another process's row.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use charliebot_core::{fsutil, home};
use charliebot_registry::context::FeatureContext;
use charliebot_store::metadata::UtcDateTime;
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, UtcOffset};

/// The cooling a member failure that states no reset time records.
pub const DEFAULT_COOLDOWN: Duration = Duration::minutes(30);

/// The reason of a record whose backend reported a spent quota.
pub const REASON_QUOTA_ERROR: &str = charliebot_core::event_types::QUOTA_REASON_QUOTA_ERROR;

/// The reason of a record whose backend refused authentication or model access.
pub const REASON_ACCESS_REFUSED: &str = charliebot_core::event_types::QUOTA_REASON_ACCESS_REFUSED;

/// The reason of a record that holds a backend out after a call it could not finish in time.
pub const REASON_TIMEOUT: &str = charliebot_core::event_types::QUOTA_REASON_TIMEOUT;

/// The reason of a record that holds a pool member out after an iteration failed for a reason other than its quota.
pub const REASON_ITERATION_FAILURE: &str = charliebot_core::event_types::QUOTA_REASON_ITERATION_FAILURE;

/// Every reason a record carries. A switch record names one of them, and the readers of a switch accept any.
pub const RECORDED_REASONS: &[&str] =
  &[REASON_QUOTA_ERROR, REASON_ACCESS_REFUSED, REASON_TIMEOUT, REASON_ITERATION_FAILURE];

/// Whether `reason` is one this owner records on a row.
pub fn is_recorded_reason(reason: &str) -> bool {
  RECORDED_REASONS.contains(&reason)
}

/// Why the member a switch left is out, as the switch note states it.
pub fn switch_cause(reason: &str) -> &'static str {
  match reason {
    REASON_QUOTA_ERROR => "the member's quota is spent",
    REASON_ACCESS_REFUSED => "the member refused access",
    REASON_TIMEOUT => "the member timed out",
    REASON_ITERATION_FAILURE => "the member's iteration failed",
    _ => "the member is unavailable",
  }
}

/// When the member a switch left recovers, as the switch note states it.
pub fn switch_recovery_label(reason: &str) -> &'static str {
  if reason == REASON_QUOTA_ERROR { "resets at" } else { "recovers at" }
}

/// Why a retry run's handoff note says its earlier backend left, in the form "after {from} …".
pub fn switch_after_reason(reason: &str) -> &'static str {
  match reason {
    REASON_QUOTA_ERROR => "spent its quota",
    REASON_ACCESS_REFUSED => "refused access",
    REASON_TIMEOUT => "timed out",
    REASON_ITERATION_FAILURE => "failed its iteration",
    _ => "became unavailable",
  }
}

const LOCK_SUFFIX: &str = ".lock";

/// One backend option's "out until" row.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AvailabilityRow {
  /// The instant the option becomes selectable again.
  pub out_until: UtcDateTime,
  /// Why the option is out: one of [`RECORDED_REASONS`].
  pub reason: String,
  /// The error text the record came from.
  pub detail: String,
  /// When the record was written.
  pub recorded_at: UtcDateTime,
}

/// The file-backed owner of "out until" per backend option id.
#[derive(Debug)]
pub struct BackendAvailability {
  path: PathBuf,
}

impl BackendAvailability {
  /// Read the home's availability state. A missing file is an empty state; a file that exists but cannot be read or
  /// parsed is an error that names the file, and the caller stops.
  pub fn load(home: &Path) -> anyhow::Result<Self> {
    let path = home::backend_availability_path(home);
    Self::read_rows(&path)?;
    Ok(Self { path })
  }

  /// Record that `backend_id` is out until its reset, or for [`DEFAULT_COOLDOWN`] when the error states no reset, and
  /// persist the state. Return the instant the option becomes selectable again.
  ///
  /// The write is a read-merge-write under an exclusive file lock, so a row another process recorded since this one
  /// last read the file survives.
  pub fn record(
    &self,
    backend_id: &str,
    resets_at: Option<UtcDateTime>,
    reason: &str,
    detail: &str,
  ) -> anyhow::Result<OffsetDateTime> {
    let out_until =
      resets_at.map(UtcDateTime::into_inner).unwrap_or_else(|| UtcDateTime::now().into_inner() + DEFAULT_COOLDOWN);
    let row = AvailabilityRow {
      out_until: out_until.into(),
      reason: reason.to_string(),
      detail: detail.to_string(),
      recorded_at: UtcDateTime::now(),
    };
    let _lock = self.lock()?;
    let mut rows = Self::read_rows(&self.path)?;
    rows.insert(backend_id.to_string(), row);
    self.persist(&rows)?;
    drop(_lock);
    tracing::warn!(
      target: "charliebot_agents",
      member = backend_id,
      until = %format_minutes(out_until),
      reason,
      reset_known = resets_at.is_some(),
      "backend_availability_recorded"
    );
    Ok(out_until)
  }

  /// Return the stored row for a backend, whether or not it has passed, from the file's current state.
  pub fn row(&self, backend_id: &str) -> Option<AvailabilityRow> {
    Self::read_rows(&self.path)
      .unwrap_or_else(|error| {
        tracing::error!(target: "charliebot_agents", error = %error, "backend_availability_read_failed");
        HashMap::new()
      })
      .remove(backend_id)
  }

  /// Return the stored "out until" instant, whether or not it has passed, from the file's current state.
  pub fn out_until(&self, backend_id: &str) -> Option<OffsetDateTime> {
    self.row(backend_id).map(|row| row.out_until.into_inner())
  }

  /// Return the stored "out until" instant in the minute-resolution event format.
  pub fn out_until_minutes(&self, backend_id: &str) -> Option<String> {
    self.out_until(backend_id).map(format_minutes)
  }

  /// Take the exclusive lock that serializes every process's read-merge-write of the state file.
  fn lock(&self) -> anyhow::Result<Flock<fs::File>> {
    let file_name =
      self.path.file_name().expect("the availability path has a file name").to_string_lossy().into_owned();
    let lock_path = self.path.with_file_name(format!("{file_name}{LOCK_SUFFIX}"));
    fs::create_dir_all(self.path.parent().expect("the availability path has a parent"))?;
    let file = OpenOptions::new()
      .create(true)
      .read(true)
      .write(true)
      .truncate(false)
      .open(&lock_path)
      .with_context(|| format!("could not lock {}", display_state_path()))?;
    Flock::lock(file, FlockArg::LockExclusive)
      .map_err(|(_, error)| anyhow!("could not lock {}: {error}", display_state_path()))
  }

  /// Read the file's current rows. A missing file is empty; anything else unreadable names the file.
  fn read_rows(path: &Path) -> anyhow::Result<HashMap<String, AvailabilityRow>> {
    match fs::read_to_string(path) {
      Ok(contents) => serde_json::from_str::<HashMap<String, AvailabilityRow>>(&contents).with_context(read_error),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
      Err(error) => Err(error).with_context(read_error),
    }
  }

  /// Write the state through an atomic publication, so a reader never sees a partial file.
  fn persist(&self, rows: &HashMap<String, AvailabilityRow>) -> anyhow::Result<()> {
    let contents = serde_json::to_string_pretty(rows).context("serializing the backend availability state")?;
    fs::create_dir_all(self.path.parent().expect("the availability path has a parent"))?;
    fsutil::atomic_write_text(&self.path, &contents).with_context(write_error)?;
    Ok(())
  }
}

/// Read the process-wide availability owner inserted by the startup step.
pub fn backend_availability(features: &FeatureContext) -> anyhow::Result<std::sync::Arc<BackendAvailability>> {
  Ok(features.startup.require::<BackendAvailability>()?)
}

fn read_error() -> String {
  format!("could not read {}", display_state_path())
}

fn write_error() -> String {
  format!("could not write {}", display_state_path())
}

fn display_state_path() -> String {
  home::display_path(&home::state_rel_path(home::BACKEND_AVAILABILITY_FILE_NAME))
}

/// Render the minute-resolution UTC form the pool events and reports carry.
pub(crate) fn format_minutes(datetime: OffsetDateTime) -> String {
  const MINUTES: &[time::format_description::FormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day]T[hour]:[minute][offset_hour sign:mandatory]:[offset_minute]"
  );
  datetime.to_offset(UtcOffset::UTC).format(MINUTES).expect("a UTC timestamp formats")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_missing_file_loads_an_empty_state() {
    let home = tempfile::tempdir().unwrap();
    let availability = BackendAvailability::load(home.path()).unwrap();
    assert_eq!(availability.out_until("any"), None);
  }

  #[test]
  fn a_second_owner_from_the_same_home_shares_rows_without_dropping_them() {
    let home = tempfile::tempdir().unwrap();
    let first = BackendAvailability::load(home.path()).unwrap();
    let second = BackendAvailability::load(home.path()).unwrap();
    first.record("a", None, REASON_QUOTA_ERROR, "a is spent").unwrap();
    second.record("b", None, REASON_QUOTA_ERROR, "b is spent").unwrap();

    assert!(first.out_until("b").is_some(), "the first owner sees the second owner's row");
    assert!(second.out_until("a").is_some(), "the second owner sees the first owner's row");
    let rows = BackendAvailability::read_rows(&home::backend_availability_path(home.path())).unwrap();
    let mut keys = rows.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(keys, ["a", "b"]);
  }

  #[test]
  fn a_record_survives_a_rebuild_from_the_same_home() {
    let home = tempfile::tempdir().unwrap();
    let availability = BackendAvailability::load(home.path()).unwrap();
    let reset = UtcDateTime::parse("2030-01-02T03:04:05Z").unwrap();
    let out_until = availability.record("member", Some(reset), REASON_QUOTA_ERROR, "quota is spent").unwrap();

    let reloaded = BackendAvailability::load(home.path()).unwrap();
    assert_eq!(reloaded.out_until("member").unwrap(), out_until);
    assert_eq!(reloaded.out_until_minutes("member").unwrap(), "2030-01-02T03:04+00:00");
    assert_eq!(reloaded.out_until("other"), None);
  }

  #[test]
  fn a_record_without_a_stated_reset_uses_the_default_cooldown() {
    let home = tempfile::tempdir().unwrap();
    let availability = BackendAvailability::load(home.path()).unwrap();
    let out_until = availability.record("member", None, REASON_QUOTA_ERROR, "quota is spent").unwrap();
    let remaining = out_until - OffsetDateTime::now_utc();
    assert!(remaining >= DEFAULT_COOLDOWN - Duration::seconds(5));
    assert!(remaining <= DEFAULT_COOLDOWN + Duration::seconds(5));
  }

  #[test]
  fn the_written_file_parses_and_leaves_no_staging_file() {
    let home = tempfile::tempdir().unwrap();
    let availability = BackendAvailability::load(home.path()).unwrap();
    availability
      .record("member", Some(UtcDateTime::parse("2030-01-02T03:04:05Z").unwrap()), REASON_TIMEOUT, "judge timed out")
      .unwrap();

    let path = home::backend_availability_path(home.path());
    let leftovers: Vec<String> = fs::read_dir(path.parent().unwrap())
      .unwrap()
      .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
      .filter(|name| name.ends_with(".tmp") || name.ends_with(".staging"))
      .collect();
    assert!(leftovers.is_empty(), "the publication left sibling files: {leftovers:?}");
    let contents = fs::read_to_string(&path).unwrap();
    let rows = serde_json::from_str::<HashMap<String, AvailabilityRow>>(&contents).unwrap();
    let row = rows.get("member").expect("the recorded member is in the file");
    assert_eq!(row.out_until.to_str(), "2030-01-02T03:04:05Z");
    assert_eq!(row.reason, REASON_TIMEOUT);
    assert_eq!(row.detail, "judge timed out");
    UtcDateTime::parse(&row.recorded_at.to_str()).expect("recorded_at parses");
  }

  #[test]
  fn an_unreadable_file_is_an_error_that_names_the_file() {
    let home = tempfile::tempdir().unwrap();
    let path = home::backend_availability_path(home.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "not json").unwrap();
    let error = BackendAvailability::load(home.path()).unwrap_err().to_string();
    assert_eq!(error, format!("could not read {}", display_state_path()));
  }

  #[test]
  fn an_unreadable_state_directory_is_an_error_that_names_the_file() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("state"), "a file where the directory belongs").unwrap();
    let error = BackendAvailability::load(home.path()).unwrap_err().to_string();
    assert_eq!(error, format!("could not read {}", display_state_path()));
  }
}
