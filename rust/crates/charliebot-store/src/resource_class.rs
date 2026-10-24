//! Which memory share a session's agent processes run in (`src/runtime/resource_class.py`).
//!
//! The operator's repair mark is stored on the session it was set on. A spawn resolves its class by walking the
//! task-parent chain of its session and taking the nearest `repair` mark, the session's own mark first; everything
//! else is `agent`. Nothing is written on descendants, so clearing a parent's mark moves the descendants' next
//! spawns back to the agents' share.
//!
//! The walk reads each `metadata.json` from disk: a spawn must see a mark the operator just cleared.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use charliebot_core::cgroup::ResourceClass;
use serde_json::Value;

use crate::{StoreError, StoreResult};

const SESSION_METADATA_NAME: &str = "metadata.json";

/// The class of *session_id*'s spawns, read from the metadata files under *sessions_dir*.
pub fn resolve(sessions_dir: &Path, session_id: &str) -> StoreResult<ResourceClass> {
  let mut seen = HashSet::new();
  let mut current = session_id.to_string();
  loop {
    if !seen.insert(current.clone()) {
      return Err(StoreError::new(format!(
        "task-parent cycle while resolving resource class for {session_id}: {current}"
      )));
    }
    let path = sessions_dir.join(&current).join(SESSION_METADATA_NAME);
    let raw = match fs::read_to_string(&path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == io::ErrorKind::NotFound => {
        return Err(StoreError::new(format!(
          "session {current} metadata is missing while resolving resource class for {session_id}"
        )));
      }
      Err(error) => return Err(error.into()),
    };
    let meta: Value = serde_json::from_str(&raw)?;
    let mark = match meta.get("resource_class") {
      Some(value) => serde_json::from_value::<ResourceClass>(value.clone())
        .map_err(|error| StoreError::new(format!("session {current} has invalid resource_class: {error}")))?,
      None => ResourceClass::Agent,
    };
    if mark == ResourceClass::Repair {
      return Ok(ResourceClass::Repair);
    }
    match meta.get("task_parent_id") {
      None | Some(Value::Null) => return Ok(ResourceClass::Agent),
      Some(Value::String(parent)) if !parent.is_empty() => current = parent.clone(),
      Some(other) => {
        return Err(StoreError::new(format!("session {current} has invalid task_parent_id {other}")));
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::*;

  /// Write the one file the walk reads: `task_parent_id` and the optional repair mark.
  fn write_metadata(sessions: &Path, session_id: &str, parent: Option<&str>, mark: Option<&str>) {
    let directory = sessions.join(session_id);
    fs::create_dir_all(&directory).unwrap();
    let mut payload = serde_json::json!({"name": session_id, "profile": "manager", "task_parent_id": parent});
    if let Some(mark) = mark {
      payload["resource_class"] = serde_json::json!(mark);
    }
    fs::write(directory.join("metadata.json"), payload.to_string()).unwrap();
  }

  #[test]
  fn resolution_takes_the_nearest_repair_mark_and_follows_a_cleared_parent() {
    let root = tempfile::tempdir().unwrap();
    let sessions = root.path().join("sessions");
    write_metadata(&sessions, "root", None, Some("repair"));
    write_metadata(&sessions, "mid", Some("root"), None);
    write_metadata(&sessions, "leaf", Some("mid"), None);

    assert_eq!(resolve(&sessions, "leaf").unwrap(), ResourceClass::Repair);
    // Clearing the mark on the ancestor moves the descendant back without writing on it.
    write_metadata(&sessions, "root", None, None);
    assert_eq!(resolve(&sessions, "leaf").unwrap(), ResourceClass::Agent);
    write_metadata(&sessions, "mid", Some("root"), Some("repair"));
    assert_eq!(resolve(&sessions, "leaf").unwrap(), ResourceClass::Repair);
    assert_eq!(resolve(&sessions, "root").unwrap(), ResourceClass::Agent);
  }

  #[test]
  fn resolution_stops_on_a_relationship_cycle() {
    let root = tempfile::tempdir().unwrap();
    let sessions = root.path().join("sessions");
    write_metadata(&sessions, "a", Some("b"), None);
    write_metadata(&sessions, "b", Some("a"), None);

    let result = resolve(&sessions, "a");
    assert!(
      matches!(&result, Err(error) if error.to_string().contains("task-parent cycle")),
      "a cycle must fail the resolution, got {result:?}"
    );
  }

  #[test]
  fn resolution_fails_when_a_task_parent_is_missing() {
    let root = tempfile::tempdir().unwrap();
    let sessions = root.path().join("sessions");
    write_metadata(&sessions, "leaf", Some("missing-parent"), None);

    let result = resolve(&sessions, "leaf");
    assert!(
      matches!(&result, Err(error) if error.to_string().contains("missing-parent")),
      "a missing parent must fail the resolution, got {result:?}"
    );
  }
}
