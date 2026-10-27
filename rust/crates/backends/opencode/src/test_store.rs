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
  let mut connection = Connection::open(db).unwrap();
  connection.execute(SCHEMA, []).unwrap();
  let transaction = connection.transaction().unwrap();
  for message in messages {
    insert(&transaction, message);
  }
  transaction.commit().unwrap();
}

/// Inserts a row, or replaces the row with the same id.
pub fn write_message(db: &Path, message: &Message) {
  insert(&Connection::open(db).unwrap(), message);
}

pub fn delete_message(db: &Path, id: &str) {
  Connection::open(db).unwrap().execute("delete from session_message where id = ?", [id]).unwrap();
}

/// Deletes every row in one statement, which leaves its pages on the freelist.
pub fn clear_messages(db: &Path) {
  Connection::open(db).unwrap().execute("delete from session_message", []).unwrap();
}

/// Sets the file's modification time, so a test sets the signature's mtime instead of depending on the clock.
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
