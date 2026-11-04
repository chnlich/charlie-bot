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

/// The session replay websocket. A client connects with its cursor in the query string.
pub const SESSION_WEBSOCKET_PATH: &str = "/ws/sessions/{session_id}";

/// The replay websocket's last catch-up frame, in its `type` field: every event before it has been sent. A client that
/// replays an earlier stretch of the session holds its frames until this one arrives.
pub const CATCHUP_COMPLETE_FRAME: &str = "catchup_complete";
