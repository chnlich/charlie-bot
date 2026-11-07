//! Synthetic opencode stores for the unit tests: the 2.0 `session_message` table, written and changed by the tests.
//! Nothing here opens a real home.

use std::fs::File;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, params};

const SCHEMA: &str = "create table session_message(id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, \
                      time_created INTEGER, time_updated INTEGER, data TEXT)";

/// One `session_message` row.
pub struct Message<'a> {
  pub id: &'a str,
  pub session: &'a str,
  pub role: &'a str,
  pub updated: i64,
  pub data: &'a str,
}

/// A new store at `db` holding `messages`, written in one transaction.
pub fn create_store(db: &Path, messages: &[Message]) {
  let mut connection = open_test_store(db);
  connection.execute(SCHEMA, []).unwrap();
  let transaction = connection.transaction().unwrap();
  for message in messages {
    insert(&transaction, message);
  }
  transaction.commit().unwrap();
}

/// The write of one message row: an insert, or a replace of the row with the same id.
pub fn write_message(db: &Path, message: &Message) {
  insert(&open_test_store(db), message);
}

/// The deletion of every row in one statement, which leaves its pages on the freelist.
pub fn clear_messages(db: &Path) {
  open_test_store(db).execute("delete from session_message", []).unwrap();
}

/// The file's modification time, set by a test so the signature's mtime does not depend on the clock.
pub fn touch(path: &Path, seconds: u64) {
  File::options().write(true).open(path).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)).unwrap();
}

fn insert(connection: &Connection, message: &Message) {
  connection
    .execute(
      "insert or replace into session_message (id, session_id, type, seq, time_created, time_updated, data) \
       values (?, ?, ?, 0, 1700000000000, ?, ?)",
      params![message.id, message.session, message.role, message.updated, message.data],
    )
    .unwrap();
}

fn open_test_store(db: &Path) -> Connection {
  let connection = Connection::open(db).unwrap();
  connection.pragma_update(None, "journal_mode", "OFF").unwrap();
  connection.pragma_update(None, "synchronous", "OFF").unwrap();
  connection
}
