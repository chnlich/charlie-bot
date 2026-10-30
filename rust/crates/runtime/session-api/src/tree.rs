//! Revision-bound task-tree pages used by the session listing route.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use charliebot_core::text_repr;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use charliebot_store::{Store, StoreError, StoreResult, TaskTreeRow};
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
  let index = store.task_tree_snapshot()?;
  if let Some(parent_id) = parent_id {
    index.meta(parent_id)?;
  }
  let revision = index.revision().to_string();
  let after = match cursor {
    Some(cursor) => {
      let (cursor_revision, cursor_time, cursor_id) = decode_cursor(cursor)?;
      if cursor_revision != revision {
        return Err(StoreError::conflict(vec!["task tree changed during pagination; refresh and re-paginate".into()]));
      }
      Some((cursor_time, cursor_id))
    }
    None => None,
  };
  let page = store.task_tree_page(
    &index,
    parent_id,
    include_archived,
    after.as_ref().map(|(time, id)| (*time, id.as_str())),
    limit,
  )?;
  let mut last = None;
  let mut items = Vec::with_capacity(page.rows.len());
  for row in page.rows {
    let meta = index.meta(&row.session_id)?;
    last = Some((meta.created_at, meta.id.clone()));
    items.push(session_row(meta, row));
  }
  let next_cursor =
    if page.has_more { last.map(|(created_at, id)| encode_cursor(&revision, created_at, &id)) } else { None };
  Ok(json!({
    "items": items,
    "next_cursor": next_cursor,
    "tree_revision": revision,
  }))
}

fn session_row(meta: &SessionMetadata, row: TaskTreeRow) -> SessionRow {
  SessionRow {
    id: meta.id.clone(),
    name: meta.name.clone(),
    profile: meta.profile,
    task_parent_id: meta.task_parent_id.clone(),
    task_state: row.task_state,
    work_state: row.work_state,
    archived: row.archived,
    child_count: row.child_count,
    open_descendant_count: row.open_descendant_count,
    running_descendant_count: row.running_descendant_count,
    has_unread: meta.has_unread,
  }
}

fn encode_cursor(revision: &str, created_at: UtcDateTime, session_id: &str) -> String {
  let payload = json!({"r": revision, "a": created_at.to_isoformat_text(), "i": session_id});
  URL_SAFE_NO_PAD.encode(charliebot_compat::json::fast_json_bytes(&payload))
}

fn decode_cursor(cursor: &str) -> StoreResult<(String, UtcDateTime, String)> {
  let decoded = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| malformed_cursor(cursor))?;
  let payload: TreeCursor = serde_json::from_slice(&decoded).map_err(|_| malformed_cursor(cursor))?;
  let created_at = UtcDateTime::parse(&payload.a).map_err(|_| malformed_cursor(cursor))?;
  Ok((payload.r, created_at, payload.i))
}

fn malformed_cursor(cursor: &str) -> StoreError {
  StoreError::invalid(format!("malformed tree page cursor: {}", text_repr::str_repr(cursor)))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::test_support::{create_node, feature_context};
  use charliebot_store::metadata::TaskProfile;

  #[tokio::test]
  async fn a_tree_page_builds_one_revision_index_and_visits_each_row_descendant_once() {
    let (_home, features) = feature_context();
    let root = create_node(&features.store, "tree-root", None, TaskProfile::Manager).await;
    let first = create_node(&features.store, "tree-first", Some(&root.id), TaskProfile::Manager).await;
    create_node(&features.store, "tree-second", Some(&root.id), TaskProfile::Manager).await;
    create_node(&features.store, "tree-grandchild", Some(&first.id), TaskProfile::Manager).await;
    features.store.invalidate_tree_index();

    let index_builds_before = features.store.task_tree_index_build_count();
    let tail_reads_before = features.store.task_state_tail_read_count();
    let activity_calls_before = features.store.task_tree_activity_call_count();
    tree_page(&features.store, Some(&root.id), true, 20, None).unwrap();

    assert_eq!(features.store.task_tree_index_build_count() - index_builds_before, 1);
    assert!(features.store.task_state_tail_read_count() - tail_reads_before <= 4);
    assert_eq!(features.store.task_tree_activity_call_count() - activity_calls_before, 3);
    let after_first_page = features.store.task_tree_index_build_count();
    let after_first_tail_reads = features.store.task_state_tail_read_count();
    tree_page(&features.store, Some(&root.id), true, 20, None).unwrap();
    assert_eq!(features.store.task_tree_index_build_count(), after_first_page);
    assert_eq!(features.store.task_state_tail_read_count(), after_first_tail_reads);
  }

  #[tokio::test]
  async fn a_tree_page_computes_only_the_rows_it_returns() {
    let (_home, features) = feature_context();
    let root = create_node(&features.store, "tree-root", None, TaskProfile::Manager).await;
    for index in 0..5 {
      create_node(&features.store, &format!("tree-child-{index}"), Some(&root.id), TaskProfile::Manager).await;
    }
    features.store.invalidate_tree_index();

    let all = tree_page(&features.store, Some(&root.id), true, 100, None).unwrap();
    let expected = page_ids(&all);

    let activity_calls_before = features.store.task_tree_activity_call_count();
    let first = tree_page(&features.store, Some(&root.id), true, 2, None).unwrap();
    assert_eq!(
      features.store.task_tree_activity_call_count() - activity_calls_before,
      3,
      "a two-row page computes its rows and the one row that proves another follows, not every child"
    );
    assert_eq!(page_ids(&first), expected[..2]);
    let cursor = first["next_cursor"].as_str().expect("a further row follows the page").to_string();

    let second = tree_page(&features.store, Some(&root.id), true, 100, Some(&cursor)).unwrap();
    assert_eq!(page_ids(&second), expected[2..]);
    assert!(second["next_cursor"].is_null());
  }

  fn page_ids(page: &Value) -> Vec<String> {
    page["items"]
      .as_array()
      .expect("a page carries items")
      .iter()
      .map(|row| row["id"].as_str().expect("a row carries its id").to_string())
      .collect()
  }

  #[tokio::test]
  async fn a_tree_page_leaves_the_events_cache_unchanged() {
    let (_home, features) = feature_context();
    let root = create_node(&features.store, "tree-root", None, TaskProfile::Manager).await;
    let first = create_node(&features.store, "tree-first", Some(&root.id), TaskProfile::Manager).await;
    let second = create_node(&features.store, "tree-second", Some(&root.id), TaskProfile::Manager).await;
    let grandchild = create_node(&features.store, "tree-grandchild", Some(&first.id), TaskProfile::Manager).await;
    for session_id in [&root.id, &first.id, &second.id, &grandchild.id] {
      features.store.append_chat_event(session_id, json!({"type": "user", "content": "hello"})).await.unwrap();
      features.store.clear_chat_events_cache(session_id);
    }
    let cached_before = features.store.events_cache_totals();

    tree_page(&features.store, Some(&root.id), true, 20, None).unwrap();
    tree_page(&features.store, None, true, 20, None).unwrap();

    assert_eq!(features.store.events_cache_totals(), cached_before, "a tree page reads no history into the cache");
  }

  #[test]
  fn tree_cursor_round_trips_and_keeps_datetime_text() {
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
