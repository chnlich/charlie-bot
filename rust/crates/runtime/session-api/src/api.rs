//! The session route paths, response field names and websocket frame names that other crates use. The routers,
//! `ROUTE_PATTERNS`, the paginated responses and the websocket use these consts too, so each one is spelled once. A
//! `{session_id}` parameter is filled by the caller.

/// The session collection: `GET` lists the active sessions, `POST` creates one.
pub const SESSIONS_PATH: &str = "/api/sessions/";
pub const SESSION_TREE_PATH: &str = "/api/sessions/tree";
pub use charliebot_cli_common::route::SESSION_PATH;
pub const SESSION_BOOTSTRAP_PATH: &str = "/api/sessions/{session_id}/bootstrap";
pub const SESSION_EVENTS_PATH: &str = "/api/sessions/{session_id}/events";
pub const SESSION_EVENTS_JSONL_PATH: &str = "/api/sessions/{session_id}/events.jsonl";
pub const SESSION_GROUP_PATH: &str = "/api/sessions/{session_id}/group";
pub const SESSION_RUNS_PATH: &str = "/api/sessions/{session_id}/runs";
pub const SESSION_PENDING_TRIGGERS_PATH: &str = "/api/sessions/{session_id}/pending-triggers";

/// The session-tree query's keys: the parent to root the page at, the archived switch and the keyset cursor.
/// `SESSION_TREE_PATH`'s route reads them and the CLI sends them, so each is spelled once. The page size is
/// `charliebot_cli_common::fields::LIMIT`, which the session read routes and the CLI share.
pub const SESSION_TREE_PARENT_ID_QUERY: &str = "parent_id";
pub const SESSION_TREE_INCLUDE_ARCHIVED_QUERY: &str = "include_archived";
pub const SESSION_TREE_CURSOR_QUERY: &str = "cursor";

/// The fields that page a session response: the flag that older history remains, the cursor for the next older page,
/// and the ordinal of the page's oldest message. The events route, the bootstrap payload and the sessions list write
/// them, and the CLI's dialog reader reads them, so each is spelled once.
pub const HAS_MORE_FIELD: &str = "has_more";
pub const NEXT_BEFORE_FIELD: &str = "next_before";
pub const OLDEST_MESSAGE_ORDINAL_FIELD: &str = "oldest_message_ordinal";

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

/// The web protocol this server speaks: the calls, answer shapes and socket frames the page's JavaScript uses. A release
/// that changes what an already-loaded page cannot work with raises it by one. The chat page renders it, and its socket
/// handshake names it; a page on another protocol reloads to the current code, and a page on this one keeps its code.
pub const WEB_PROTOCOL: u32 = 1;

/// The instant the server became ready. `catchup_complete` reports the uptime from it, which places a watched restart's
/// return point on the page's clock.
pub struct ReadyClock(pub std::time::Instant);
