//! The persisted owner of backend usage availability: one "out until" row per backend option id.
//!
//! Every quota cooling record, the backend pool's included, lands here and survives a restart: the state lives in
//! `$CHARLIEBOT_HOME/state/backend_availability.json`, written atomically, and a startup reads it back.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use charliebot_core::home;
use charliebot_registry::context::FeatureContext;
use charliebot_store::metadata::UtcDateTime;
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, UtcOffset};

/// The cooling a quota error that states no reset time records.
pub const DEFAULT_COOLDOWN: Duration = Duration::minutes(30);

/// The reason of a record whose backend reported a spent quota.
pub const REASON_QUOTA_ERROR: &str = "quota_error";

/// The reason of a record that holds a backend out after a call it could not finish in time.
pub const REASON_TIMEOUT: &str = "timeout";

const STAGING_SUFFIX: &str = ".staging";

/// One backend option's "out until" row.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AvailabilityRow {
  /// The instant the option becomes selectable again.
  pub out_until: UtcDateTime,
  /// Why the option is out: [`REASON_QUOTA_ERROR`], [`REASON_TIMEOUT`].
  pub reason: String,
  /// The error text the record came from.
  pub detail: String,
  /// When the record was written.
  pub recorded_at: UtcDateTime,
}

/// The in-memory mirror of `$CHARLIEBOT_HOME/state/backend_availability.json`, the one owner of "out until" per
/// backend option id.
#[derive(Debug)]
pub struct BackendAvailability {
  path: PathBuf,
  rows: Mutex<HashMap<String, AvailabilityRow>>,
}

impl BackendAvailability {
  /// Read the home's availability state. A missing file is an empty state; a file that exists but cannot be read or
  /// parsed is an error that names the file, and the caller stops.
  pub fn load(home: &Path) -> anyhow::Result<Self> {
    let path = home::backend_availability_path(home);
    let rows = match fs::read_to_string(&path) {
      Ok(contents) => serde_json::from_str::<HashMap<String, AvailabilityRow>>(&contents).with_context(read_error)?,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
      Err(error) => return Err(error).with_context(read_error),
    };
    Ok(Self { path, rows: Mutex::new(rows) })
  }

  /// Record that `backend_id` is out until its reset, or for [`DEFAULT_COOLDOWN`] when the error states no reset, and
  /// persist the state. Return the instant the option becomes selectable again.
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
    let mut rows = self.rows.lock().expect("backend availability state is not poisoned");
    rows.insert(backend_id.to_string(), row);
    self.persist(&rows)?;
    drop(rows);
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

  /// Return the stored "out until" instant, whether or not it has passed.
  pub fn out_until(&self, backend_id: &str) -> Option<OffsetDateTime> {
    self
      .rows
      .lock()
      .expect("backend availability state is not poisoned")
      .get(backend_id)
      .map(|row| row.out_until.into_inner())
  }

  /// Return the stored "out until" instant in the minute-resolution event format.
  pub fn out_until_minutes(&self, backend_id: &str) -> Option<String> {
    self.out_until(backend_id).map(format_minutes)
  }

  /// Write the state through a staging file and an atomic rename, so a reader never sees a partial file.
  fn persist(&self, rows: &HashMap<String, AvailabilityRow>) -> anyhow::Result<()> {
    let file_name =
      self.path.file_name().expect("the availability path has a file name").to_string_lossy().into_owned();
    let staging = self.path.with_file_name(format!("{file_name}{STAGING_SUFFIX}"));
    let contents = serde_json::to_vec_pretty(rows).context("serializing the backend availability state")?;
    fs::create_dir_all(self.path.parent().expect("the availability path has a parent"))?;
    fs::write(&staging, contents).with_context(write_error)?;
    fs::rename(&staging, &self.path).with_context(write_error)?;
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
    assert!(!path.with_file_name(format!("{}{STAGING_SUFFIX}", home::BACKEND_AVAILABILITY_FILE_NAME)).exists());
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
