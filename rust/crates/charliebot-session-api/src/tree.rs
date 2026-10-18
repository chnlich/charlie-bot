//! Revision-bound task-tree pages used by the session listing route.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use charliebot_core::pyfmt;
use charliebot_store::control_events;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use charliebot_store::{Store, StoreError, StoreResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Serialize)]
struct SessionRow {
  id: String,
  name: String,
  profile: charliebot_store::metadata::TaskProfile,
  task_parent_id: Option<String>,
  task_state: String,
  work_state: String,
  archived: bool,
  child_count: usize,
  open_descendant_count: usize,
  running_descendant_count: usize,
  has_unread: bool,
}

#[derive(Deserialize)]
struct TreeCursor {
  r: String,
  a: String,
  i: String,
}

/// Return one revision-bound keyset page from the task tree.
pub(crate) fn tree_page(
  store: &Store,
  parent_id: Option<&str>,
  include_archived: bool,
  limit: usize,
  cursor: Option<&str>,
) -> StoreResult<Value> {
  let metas = store.load_session_metas(None)?;
  if let Some(parent_id) = parent_id
    && !metas.iter().any(|meta| meta.id == parent_id)
  {
    return Err(StoreError::not_found(format!("task {parent_id} not found")));
  }
  let revision = tree_revision(store, &metas)?;
  let mut children: Vec<&SessionMetadata> =
    metas.iter().filter(|meta| meta.task_parent_id.as_deref() == parent_id).collect();
  children.sort_by(|left, right| left.created_at.cmp(&right.created_at).then_with(|| left.id.cmp(&right.id)));
  let mut rows = Vec::with_capacity(children.len());
  for meta in children {
    let row = session_row(store, meta)?;
    let mut has_running_descendant = false;
    for descendant in store.descendant_ids(&meta.id)? {
      has_running_descendant |= store.task_tree_activity(&descendant)?.work_state == "running";
    }
    if !include_archived && row.archived && row.work_state != "running" && !has_running_descendant {
      continue;
    }
    rows.push((meta.created_at, meta.id.clone(), row));
  }

  if let Some(cursor) = cursor {
    let (cursor_revision, cursor_time, cursor_id) = decode_cursor(cursor)?;
    if cursor_revision != revision {
      return Err(StoreError::conflict(vec!["task tree changed during pagination; refresh and re-paginate".into()]));
    }
    rows.retain(|(created_at, id, _)| (*created_at, id.as_str()) > (cursor_time, cursor_id.as_str()));
  }

  let has_more = rows.len() > limit;
  rows.truncate(limit);
  let next_cursor =
    if has_more { rows.last().map(|(created_at, id, _)| encode_cursor(&revision, *created_at, id)) } else { None };
  Ok(json!({
    "items": rows.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>(),
    "next_cursor": next_cursor,
    "tree_revision": revision,
  }))
}

fn session_row(store: &Store, meta: &SessionMetadata) -> StoreResult<SessionRow> {
  let descendants = store.descendant_ids(&meta.id)?;
  let mut open_descendant_count = 0;
  let mut running_descendant_count = 0;
  for descendant in descendants {
    if store.task_state(&descendant)? == "open" {
      open_descendant_count += 1;
    }
    if store.task_tree_activity(&descendant)?.work_state == "running" {
      running_descendant_count += 1;
    }
  }
  let activity = store.task_tree_activity(&meta.id)?;
  Ok(SessionRow {
    id: meta.id.clone(),
    name: meta.name.clone(),
    profile: meta.profile,
    task_parent_id: meta.task_parent_id.clone(),
    task_state: store.task_state(&meta.id)?,
    work_state: activity.work_state,
    archived: store.archived_of(&meta.id)?,
    child_count: store.child_ids(&meta.id)?.len(),
    open_descendant_count,
    running_descendant_count,
    has_unread: meta.has_unread,
  })
}

fn tree_revision(store: &Store, metas: &[SessionMetadata]) -> StoreResult<String> {
  let mut structural = Vec::with_capacity(metas.len());
  for meta in metas {
    let profile = serde_json::to_value(meta.profile).expect("task profile serializes to JSON");
    structural.push(format!(
      "{}|{}|{}|{}|{}|{}",
      meta.id,
      meta.task_parent_id.as_deref().unwrap_or_default(),
      profile.as_str().expect("task profile serializes as a string"),
      match meta.status {
        charliebot_store::metadata::SessionStatus::Active => "active",
        charliebot_store::metadata::SessionStatus::Archived => "archived",
      },
      store.task_state(&meta.id)?,
      if store.archived_of(&meta.id)? { "True" } else { "False" },
    ));
  }
  structural.sort();
  Ok(control_events::sha256_hex(&structural.join("\n")))
}

fn encode_cursor(revision: &str, created_at: UtcDateTime, session_id: &str) -> String {
  let payload = json!({"r": revision, "a": created_at.to_python_isoformat(), "i": session_id});
  URL_SAFE_NO_PAD.encode(charliebot_compat::json::fast_json_bytes(&payload))
}

fn decode_cursor(cursor: &str) -> StoreResult<(String, UtcDateTime, String)> {
  let decoded = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| malformed_cursor(cursor))?;
  let payload: TreeCursor = serde_json::from_slice(&decoded).map_err(|_| malformed_cursor(cursor))?;
  let created_at = UtcDateTime::parse(&payload.a).map_err(|_| malformed_cursor(cursor))?;
  Ok((payload.r, created_at, payload.i))
}

fn malformed_cursor(cursor: &str) -> StoreError {
  StoreError::invalid(format!("malformed tree page cursor: {}", pyfmt::str_repr(cursor)))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tree_cursor_round_trips_and_keeps_python_datetime_text() {
    let created_at = UtcDateTime::parse("2026-01-02T03:04:05Z").unwrap();
    let cursor = encode_cursor("revision", created_at, "synthetic-session");
    let (revision, parsed, id) = decode_cursor(&cursor).unwrap();
    assert_eq!(revision, "revision");
    assert_eq!(parsed, created_at);
    assert_eq!(id, "synthetic-session");
  }

  #[test]
  fn malformed_tree_cursors_are_invalid_requests() {
    let error = decode_cursor("bad cursor").unwrap_err();
    assert_eq!(error.status_code(), Some(400));
    assert_eq!(error.to_string(), "malformed tree page cursor: 'bad cursor'");
  }
}
