//! The route paths that `session-api` mounts. Its routers and `ROUTE_PATTERNS` use these consts, and so do the clients
//! in other crates, so each path is spelled once. A `{session_id}` parameter is filled by the caller.

/// The session collection: `GET` lists the active sessions, `POST` creates one.
pub const SESSIONS_PATH: &str = "/api/sessions/";
pub const SESSION_TREE_PATH: &str = "/api/sessions/tree";
pub const SESSION_PATH: &str = "/api/sessions/{session_id}";
pub const SESSION_BOOTSTRAP_PATH: &str = "/api/sessions/{session_id}/bootstrap";
pub const SESSION_EVENTS_PATH: &str = "/api/sessions/{session_id}/events";
pub const SESSION_EVENTS_JSONL_PATH: &str = "/api/sessions/{session_id}/events.jsonl";
pub const SESSION_GROUP_PATH: &str = "/api/sessions/{session_id}/group";
pub const SESSION_RUNS_PATH: &str = "/api/sessions/{session_id}/runs";
