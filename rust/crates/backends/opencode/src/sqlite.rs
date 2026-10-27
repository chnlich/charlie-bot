//! The opencode store's SQLite connections: the read-only open that the usage log and the sweep share.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

/// The busy timeout of every read-only open: `sqlite3.connect`'s default `timeout=5.0`.
const CONNECT_WAIT: Duration = Duration::from_secs(5);

/// The store opened read-only, as `sqlite3.connect("file:<path>?mode=ro", uri=True)` opens it.
pub(super) fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
  let connection = Connection::open_with_flags(
    format!("file:{}?mode=ro", path.display()),
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
  )?;
  connection.busy_timeout(CONNECT_WAIT)?;
  Ok(connection)
}
