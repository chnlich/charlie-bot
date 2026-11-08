//! The OpenCode event stream's protocol: the event types this crate reads and writes, and the variable that names
//! the connection-record directory the backend and the relay both find.
//!
//! The backend translates the stream, the server dispatches it, and the relay records it. One module owns the names,
//! so a change reaches all three.

/// The event the server emits once its stream carries events. The server holds a turn until it arrives.
pub const SERVER_CONNECTED: &str = "server.connected";

/// The event that opens a session execution: an execution the server dispatches before a turn claims it is
/// interrupted, because no relay will read its events.
pub const EXECUTION_STARTED: &str = "session.execution.started";

/// The event that ends a session execution with its result.
pub const EXECUTION_SUCCEEDED: &str = "session.execution.succeeded";

/// The event that ends a session execution with an error.
pub const EXECUTION_FAILED: &str = "session.execution.failed";

/// The event that ends a session execution before its result, and settles the turn.
pub const EXECUTION_INTERRUPTED: &str = "session.execution.interrupted";

/// The event the relay appends when it stops before the stream reports a turn's end.
pub const RELAY_ENDED: &str = "charliebot.opencode_relay.ended";

/// The environment variable that names the connection-record directory. The backend sets it on the relay process,
/// and the relay reads it to find the directory.
pub const SERVER_DIRECTORY_VAR: &str = "CHARLIEBOT_OPENCODE_SERVER_DIRECTORY";
