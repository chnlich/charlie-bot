//! opencode's part of the cold-storage sweep (`src/backends/opencode/usage_sweep.py`): no deletion and no category. The
//! sweep runs a manual vacuum that refuses past live writers, and reports the store's freelist.
//!
//! The 2.0 store is never deleted from: its `event` table stays empty in use, and `event_sequence` feeds live writes.
//! The usage log reads `session_message`, which the sweep never touches.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_registry::usage::{FreelistResult, SourceSweep, SweepScope};
use memchr::memmem;
use rusqlite::Connection;

use super::usage_logs::open_read_only;

/// The report line of the store's free pages.
const FREELIST: &str = "opencode-freelist";

/// The `timeout` of the vacuum's connection, until its busy_timeout pragma (`timeouts.SQLITE_LOCK_WAIT_SECONDS`).
const SQLITE_LOCK_WAIT_SECONDS: Duration = Duration::from_secs(5);

/// The `busy_timeout` of the vacuum, in milliseconds (`timeouts.SQLITE_LOCK_WAIT_MS`).
const SQLITE_LOCK_WAIT_MS: i64 = 2000;

/// The argv of a live writer. CharlieBot starts an opencode server with exactly this argv.
const SERVE_CMDLINE: &[u8] = b"opencode serve";

/// opencode's part of the sweep: a requested vacuum when the scope is not a dry run, then the freelist, which the vacuum
/// has already reduced.
pub fn sweep(db: &Path, proc_root: &Path, scope: &SweepScope) -> anyhow::Result<SourceSweep> {
  if scope.vacuum && !scope.dry_run {
    vacuum_store(db, proc_root, scope.force)?;
  }
  Ok(SourceSweep {
    categories: Vec::new(),
    freelist: Some(FreelistResult { name: FREELIST, bytes: freelist_bytes(db) }),
  })
}

/// Vacuums the store, refusing past live writers unless `force`. A refusal is an error, raised before the store is
/// touched for vacuum purposes (`_vacuum_opencode_store`).
fn vacuum_store(db: &Path, proc_root: &Path, force: bool) -> anyhow::Result<()> {
  let writers = count_writers(proc_root)?;
  if writers > 0 && !force {
    bail!("refusing to vacuum {}: {writers} live opencode writer(s); stop them or re-run with --force.", db.display());
  }
  if !db.exists() {
    return Ok(());
  }
  let connection = match open_for_vacuum(db) {
    Ok(connection) => connection,
    Err(error) => {
      tracing::warn!(
        target: "charliebot_agents",
        db = %db.display(),
        error = %error,
        "storage_cool_opencode_connect_failed"
      );
      return Ok(());
    }
  };
  if let Err(error) = vacuum_db(&connection, force) {
    tracing::warn!(
      target: "charliebot_agents",
      db = %db.display(),
      error = %error,
      "storage_cool_opencode_vacuum_failed"
    );
  }
  Ok(())
}

/// The store opened read-write for the vacuum, with the lock wait of `sqlite3.connect(db, timeout=...)`.
fn open_for_vacuum(db: &Path) -> rusqlite::Result<Connection> {
  let connection = Connection::open(db)?;
  connection.busy_timeout(SQLITE_LOCK_WAIT_SECONDS)?;
  Ok(connection)
}

/// Hands the freed pages back. Without `force`, a store with no freelist is left as it is (`_vacuum_opencode_db`).
fn vacuum_db(connection: &Connection, force: bool) -> rusqlite::Result<()> {
  connection.pragma_update(None, "busy_timeout", SQLITE_LOCK_WAIT_MS)?;
  if !force && connection.query_row("PRAGMA freelist_count", [], |row| row.get::<_, i64>(0))? == 0 {
    return Ok(());
  }
  connection.execute_batch("VACUUM")
}

/// The live `opencode serve` processes under `proc_root`, not counting this process (`_count_opencode_writers`).
fn count_writers(proc_root: &Path) -> anyhow::Result<usize> {
  let own_pid = std::process::id();
  let mut count = 0;
  for entry in fs::read_dir(proc_root).with_context(|| format!("listing {}", proc_root.display()))? {
    let entry = entry.with_context(|| format!("listing {}", proc_root.display()))?;
    let name = entry.file_name();
    let Some(pid) = name.to_str().filter(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()))
    else {
      continue;
    };
    if pid.parse::<u32>() == Ok(own_pid) {
      continue;
    }
    let cmdline = match fs::read(entry.path().join("cmdline")) {
      Ok(cmdline) => cmdline,
      Err(error) => {
        // The process vanished, or its cmdline is not readable by this process, so it is no writer this sweep can see.
        tracing::debug!(target: "charliebot_agents", pid, error = %error, "storage_cool_opencode_cmdline_unreadable");
        continue;
      }
    };
    if contains_serve(&cmdline) {
      count += 1;
    }
  }
  Ok(count)
}

/// Whether a command line holds `opencode serve`, with its NUL separators read as spaces.
fn contains_serve(cmdline: &[u8]) -> bool {
  let spaced: Vec<u8> = cmdline.iter().map(|&byte| if byte == 0 { b' ' } else { byte }).collect();
  memmem::find(&spaced, SERVE_CMDLINE).is_some()
}

/// The store's free pages in bytes, which a manual vacuum hands back (`_opencode_freelist_bytes`). A failed read is
/// logged and reads as no free pages.
fn freelist_bytes(db: &Path) -> u64 {
  if !db.exists() {
    return 0;
  }
  let pages = pragma_value(db, "PRAGMA freelist_count");
  let page_size = pragma_value(db, "PRAGMA page_size");
  match (pages, page_size) {
    // SQLite reports both pragmas as non-negative counts.
    (Some(pages), Some(page_size)) => pages as u64 * page_size as u64,
    _ => 0,
  }
}

/// One integer pragma of the store, or `None` after a logged failure (`_opencode_query`).
fn pragma_value(db: &Path, pragma: &str) -> Option<i64> {
  match open_read_only(db).and_then(|connection| connection.query_row(pragma, [], |row| row.get(0))) {
    Ok(value) => Some(value),
    Err(error) => {
      tracing::warn!(
        target: "charliebot_agents",
        db = %db.display(),
        error = %error,
        "storage_cool_opencode_scan_failed"
      );
      None
    }
  }
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;
  use std::path::PathBuf;
  use std::sync::Arc;

  use charliebot_core::config::CharlieBotConfig;
  use charliebot_registry::Registry;
  use charliebot_store::metadata::UtcDateTime;
  use tempfile::TempDir;

  use super::*;
  use crate::backends::opencode::test_store::{Message, clear_messages, create_store};

  /// A store whose rows are all deleted, so its pages sit on the freelist.
  fn store_with_freelist(db: &Path) {
    let body = "x".repeat(1000);
    let ids: Vec<String> = (0..200).map(|index| format!("m{index}")).collect();
    let messages: Vec<Message> = ids
      .iter()
      .enumerate()
      .map(|(index, id)| Message { id, session: "sess-a", role: "assistant", updated: index as i64, data: &body })
      .collect();
    create_store(db, &messages);
    clear_messages(db);
  }

  fn scope(cfg: &Arc<CharlieBotConfig>, vacuum: bool, dry_run: bool, force: bool) -> SweepScope {
    SweepScope {
      cfg: Arc::clone(cfg),
      now: UtcDateTime::parse("2026-10-18T12:00:00Z").unwrap(),
      dry_run,
      session_id: None,
      facts: HashMap::new(),
      references: HashMap::new(),
      orphan_idle_days: 14,
      vacuum,
      force,
    }
  }

  fn freelist(db: &Path, proc_root: &Path, scope: &SweepScope) -> u64 {
    let result = sweep(db, proc_root, scope).unwrap();
    assert!(result.categories.is_empty(), "the opencode sweep reports no category");
    let freelist = result.freelist.unwrap();
    assert_eq!(freelist.name, "opencode-freelist");
    freelist.bytes
  }

  /// A process directory with one live writer, one other process, a non-numeric entry and this process's own entry,
  /// which carries the writer argv and must not count.
  fn proc_tree(root: &Path) -> PathBuf {
    let proc_root = root.join("proc");
    let processes =
      [("4242", "opencode\0serve\0--port\0"), ("4243", "codex\0app-server\0"), ("notes", "opencode\0serve\0")];
    for (name, cmdline) in processes {
      fs::create_dir_all(proc_root.join(name)).unwrap();
      fs::write(proc_root.join(name).join("cmdline"), cmdline).unwrap();
    }
    let own = proc_root.join(std::process::id().to_string());
    fs::create_dir_all(&own).unwrap();
    fs::write(own.join("cmdline"), "opencode\0serve\0").unwrap();
    proc_root
  }

  fn synthetic_config(root: &Path) -> Arc<CharlieBotConfig> {
    Arc::new(Registry::new().load_config(root).unwrap())
  }

  #[test]
  fn default_sweep_never_vacuums() {
    // tests/test_storage_cool.py::test_default_sweep_never_vacuums: VACUUM is opt-in, so force without vacuum has no
    // effect, and a dry run vacuums nothing either.
    let root = TempDir::new().unwrap();
    let db = root.path().join("opencode.db");
    store_with_freelist(&db);
    let cfg = synthetic_config(root.path());
    let proc_root = root.path().join("proc");
    let before = freelist(&db, &proc_root, &scope(&cfg, false, false, false));
    assert!(before > 0);

    assert_eq!(freelist(&db, &proc_root, &scope(&cfg, false, false, true)), before);
    assert_eq!(freelist(&db, &proc_root, &scope(&cfg, true, true, true)), before);
  }

  #[test]
  fn sweep_deletes_no_opencode_row_in_any_mode() {
    // tests/test_storage_cool.py::test_sweep_deletes_no_opencode_row_in_any_mode: the store keeps every row, in a
    // plain sweep and in a forced vacuum alike.
    let root = TempDir::new().unwrap();
    let db = root.path().join("opencode.db");
    create_store(
      &db,
      &[
        Message { id: "m1", session: "sess-a", role: "assistant", updated: 100, data: "{}" },
        Message { id: "m2", session: "sess-b", role: "assistant", updated: 200, data: "{}" },
      ],
    );
    let cfg = synthetic_config(root.path());
    let proc_root = root.path().join("proc");
    fs::create_dir_all(&proc_root).unwrap();
    let rows = || {
      let connection = Connection::open(&db).unwrap();
      let mut statement = connection.prepare("select * from session_message order by id").unwrap();
      let columns = statement.column_count();
      statement
        .query_map([], |row| {
          (0..columns).map(|index| row.get::<_, rusqlite::types::Value>(index)).collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    };
    let rows_before = rows();

    sweep(&db, &proc_root, &scope(&cfg, true, true, false)).unwrap();
    sweep(&db, &proc_root, &scope(&cfg, true, false, true)).unwrap();

    assert_eq!(rows(), rows_before);
  }

  #[test]
  fn vacuum_refuses_past_a_live_writer_without_force() {
    // tests/test_storage_cool.py::test_vacuum_refuses_past_a_live_writer_without_force: a vacuum with a live writer
    // is refused and touches nothing; with force it runs. The writer count skips this process's own entry.
    let root = TempDir::new().unwrap();
    let db = root.path().join("opencode.db");
    store_with_freelist(&db);
    let cfg = synthetic_config(root.path());
    let proc_root = proc_tree(root.path());
    let before = freelist(&db, &proc_root, &scope(&cfg, false, false, false));

    let error = sweep(&db, &proc_root, &scope(&cfg, true, false, false)).unwrap_err();

    assert_eq!(
      error.to_string(),
      format!("refusing to vacuum {}: 1 live opencode writer(s); stop them or re-run with --force.", db.display())
    );
    assert_eq!(freelist(&db, &proc_root, &scope(&cfg, false, false, false)), before);
    assert_eq!(freelist(&db, &proc_root, &scope(&cfg, true, false, true)), 0);
  }
}
