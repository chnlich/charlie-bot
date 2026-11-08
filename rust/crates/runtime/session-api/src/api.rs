//! The session route paths and websocket frame names that other crates use. The routers, `ROUTE_PATTERNS` and the
//! websocket use these consts too, so each one is spelled once. A `{session_id}` parameter is filled by the caller.

/// The session collection: `GET` lists the active sessions, `POST` creates one.
pub const SESSIONS_PATH: &str = "/api/sessions/";
pub const SESSION_TREE_PATH: &str = "/api/sessions/tree";
pub const SESSION_PATH: &str = "/api/sessions/{session_id}";
pub const SESSION_BOOTSTRAP_PATH: &str = "/api/sessions/{session_id}/bootstrap";
pub const SESSION_EVENTS_PATH: &str = "/api/sessions/{session_id}/events";
pub const SESSION_EVENTS_JSONL_PATH: &str = "/api/sessions/{session_id}/events.jsonl";
pub const SESSION_GROUP_PATH: &str = "/api/sessions/{session_id}/group";
pub const SESSION_RUNS_PATH: &str = "/api/sessions/{session_id}/runs";

/// The session-tree query's keys: the parent to root the page at, the archived switch, the page size and the keyset
/// cursor. `SESSION_TREE_PATH`'s route reads them and the CLI sends them, so each is spelled once.
pub const SESSION_TREE_PARENT_ID_QUERY: &str = "parent_id";
pub const SESSION_TREE_INCLUDE_ARCHIVED_QUERY: &str = "include_archived";
pub const SESSION_TREE_LIMIT_QUERY: &str = "limit";
pub const SESSION_TREE_CURSOR_QUERY: &str = "cursor";

/// The session replay websocket. A client connects with its cursor in the query string.
pub const SESSION_WEBSOCKET_PATH: &str = "/ws/sessions/{session_id}";

/// The session ids a comma-separated `ids` query value names, trimmed, without empty entries, each once in first-seen
/// order.
pub fn parse_session_ids(raw: &str) -> Vec<String> {
  let mut seen = std::collections::HashSet::new();
  let mut ids = Vec::new();
  for id in raw.split(',').map(str::trim).filter(|id| !id.is_empty()) {
    if seen.insert(id) {
      ids.push(id.to_string());
    }
  }
  ids
}

/// The replay websocket's last catch-up frame, in its `type` field: every event before it has been sent. A client that
/// replays an earlier stretch of the session holds its frames until this one arrives.
pub const CATCHUP_COMPLETE_FRAME: &str = "catchup_complete";
