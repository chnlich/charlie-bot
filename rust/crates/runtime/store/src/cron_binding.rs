//! The session ids the current cron configuration binds, read from the home's `config.d/cron.d`.
//!
//! The reparent scope rule needs one fact of the cron configuration: the sessions its tasks point at. The cron
//! feature owns the task files: their name rules, field validation and prompt pointers. The store cannot depend on a
//! feature crate, so this module decodes each task file the way the loader does and reads only its
//! `session_id`. The configuration is the binding's single source: no session metadata carries a marker, and a
//! disabled task keeps its `session_id`, so its binding counts. A file the read or decode rejects binds nothing;
//! the loader rejects the same file and raises its own broken-file alert for it.

use std::collections::HashSet;
use std::path::Path;

use charliebot_core::config::{CONFIG_D_DIR_NAME, CRON_D_DIR_NAME};
use serde_json::Value;

use crate::{StoreError, StoreResult};

/// The sessions the cron task files under `home` bind: the `session_id` of each file that names one.
pub(crate) fn bound_sessions(home: &Path) -> StoreResult<HashSet<String>> {
  let directory = home.join(CONFIG_D_DIR_NAME).join(CRON_D_DIR_NAME);
  let entries = match std::fs::read_dir(&directory) {
    Ok(entries) => entries,
    // A home with no cron directory binds no session.
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
    Err(error) => return Err(StoreError::new(format!("{}: {error}", directory.display()))),
  };
  let mut bound = HashSet::new();
  for entry in entries {
    let path = entry.map_err(|error| StoreError::new(format!("{}: {error}", directory.display())))?.path();
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if !name.ends_with(".yaml") || name.starts_with('.') {
      continue;
    }
    if let Some(session_id) = named_session(&path) {
      bound.insert(session_id);
    }
  }
  Ok(bound)
}

/// The `session_id` of one task file, or `None` when the file names no session. A read or decode failure logs and
/// binds nothing: the file is no task's configuration, and the loader's broken-file alert already names it.
fn named_session(path: &Path) -> Option<String> {
  let raw = match std::fs::read_to_string(path) {
    Ok(raw) => raw,
    Err(error) => {
      tracing::warn!(path = %path.display(), error = %error, "cron_binding_read_failed");
      return None;
    }
  };
  let yaml = match serde_yaml::from_str::<serde_yaml::Value>(&raw) {
    Ok(yaml) => yaml,
    Err(error) => {
      tracing::warn!(path = %path.display(), error = %error, "cron_binding_read_failed");
      return None;
    }
  };
  let value = match serde_json::to_value(yaml) {
    Ok(Value::Object(value)) => value,
    Ok(_) => return None,
    Err(error) => {
      tracing::warn!(path = %path.display(), error = %error, "cron_binding_read_failed");
      return None;
    }
  };
  value.get("session_id").and_then(Value::as_str).filter(|session_id| !session_id.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn task_file(home: &Path, name: &str, body: &str) {
    let directory = home.join(CONFIG_D_DIR_NAME).join(CRON_D_DIR_NAME);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(name), body).unwrap();
  }

  #[test]
  fn the_cron_files_name_their_bound_sessions_and_a_home_without_the_dir_binds_none() {
    let home = tempfile::tempdir().unwrap();
    assert!(bound_sessions(home.path()).unwrap().is_empty());

    task_file(
      home.path(),
      "backup.yaml",
      "name: backup\nsession_id: 11111111-1111-4111-8111-111111111111\nenabled: true\n",
    );
    task_file(
      home.path(),
      "paused.yaml",
      "name: paused\nsession_id: 22222222-2222-4222-8222-222222222222\nenabled: false\n",
    );
    task_file(home.path(), "autobind.yaml", "name: autobind\nsession_id: null\n");
    // A file the decode rejects is no task's configuration; the loader alerts it.
    task_file(home.path(), "broken.yaml", "session_id: [unclosed\n");
    task_file(home.path(), "notes.txt", "session_id: 33333333-3333-4333-8333-333333333333\n");

    assert_eq!(
      bound_sessions(home.path()).unwrap(),
      HashSet::from([
        "11111111-1111-4111-8111-111111111111".to_string(),
        "22222222-2222-4222-8222-222222222222".to_string(),
      ])
    );
  }
}
