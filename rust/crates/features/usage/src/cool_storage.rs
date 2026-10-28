//! The cold-session storage sweep (`src/features/usage/storage_cool.py`) and its scheduled handler `cool_storage`: a real
//! run with no dry run, no session scope and no vacuum.
//!
//! A session is cold when its metadata reads `archived` and its `updated_at` is at least `MIN_IDLE_DAYS` old. The sweep
//! deletes the raw transport files of cold sessions (an allowlist of names inside two directory shapes), then lets each
//! registered usage source delete its own store through `UsageSourceImpl::sweep`. A real run first captures this host's
//! token usage into the usage ledger, and a capture error aborts before any deletion. A path that cannot be read logs
//! or reads as absent, and the run goes on. Metadata that Python's loader or cold rule would raise on fails the run.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use charliebot_agents::transport::final_stdout::STDOUT_LOG_NAME;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::paths::realpath;
use charliebot_core::pyfmt;
use charliebot_core::pystr;
use charliebot_registry::usage::{CategoryResult, SessionFacts, SweepScope};
use charliebot_registry::{FeatureContext, Registry};
use charliebot_store::metadata::{UtcDateTime, read_metadata_python};
use charliebot_store::run_identity::SESSION_METADATA_NAME;
use charliebot_store::run_records::{
  CURSOR_NAME, DATA_DIR_NAME, MASTER_RUNS_DIR_NAME, RAW_LOG_NAME, RUN_METADATA_NAME, RUNS_DIR_NAME, STDERR_LOG_NAME,
  THREADS_DIR_NAME,
};
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::capture::capture_into_default_ledger;
use crate::utc_datetime;

/// `constants.MIN_IDLE_DAYS`: how long an archived session must sit idle to count as cold.
const MIN_IDLE_DAYS: i64 = 14;
/// `storage_cool.ORPHAN_IDLE_DAYS`: how long an unreferenced backend record must sit idle before the sweep takes it.
const ORPHAN_IDLE_DAYS: i64 = 2;
/// `RAW_TRANSPORT_NAMES`: the three agent-pipe names from `runs.py`, and the two tee names the backend writers use.
const RAW_TRANSPORT_NAMES: [&str; 5] = [RAW_LOG_NAME, STDERR_LOG_NAME, CURSOR_NAME, STDOUT_LOG_NAME, "stderr.log"];
/// The run-record keys that name transport evidence a migrated run keeps readable.
const RUN_EVIDENCE_KEYS: [&str; 3] = ["raw_log_ref", "events_ref", "result_ref"];
const SESSION_ARCHIVED: &str = "archived";

/// The `cool_storage` scheduled handler: the real sweep at the wall clock, off the async runtime.
pub fn run_handler(context: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<String>> {
  async move {
    let context_for_sweep = Arc::clone(&context);
    let categories = tokio::task::spawn_blocking(move || {
      let config = context_for_sweep.config.get()?;
      run_cool_sweep(config, &context_for_sweep.registry, UtcDateTime::now())
    })
    .await??;
    tracing::info!(total_bytes = total_bytes(&categories), "cool_storage_handler_done");
    Ok(format_sweep_line(&categories))
  }
  .boxed()
}

/// One real sweep at `now` (`storage_cool.run_cool_sweep`). Returns the categories in report order: the raw transport
/// files, then each source's categories in registration order.
pub fn run_cool_sweep(
  cfg: Arc<CharlieBotConfig>,
  registry: &Registry,
  now: UtcDateTime,
) -> anyhow::Result<Vec<CategoryResult>> {
  // The capture guards every deletion: it runs before the scan, and its failure propagates so nothing is deleted.
  capture_into_default_ledger(&cfg, registry)?;
  let facts = scan_sessions(&cfg.sessions_dir(), now)?;
  let references = scan_references(&facts);
  let mut transport = TransportCounter::default();
  let mut cold: Vec<&str> = facts.values().filter(|owner| owner.cold).map(|owner| owner.id.as_str()).collect();
  cold.sort_unstable();
  for session_id in cold {
    sweep_raw_transport(&cfg.sessions_dir().join(session_id), &mut transport)?;
  }
  let scope = SweepScope {
    cfg: Arc::clone(&cfg),
    now,
    dry_run: false,
    session_id: None,
    facts,
    references,
    orphan_idle_days: ORPHAN_IDLE_DAYS,
    vacuum: false,
    force: false,
  };
  let mut categories = vec![transport.result()];
  for swept in registry.sweep_all(&scope)? {
    categories.extend(swept.categories);
  }
  // Python passes each category as a keyword named for it. A tracing field name is fixed at compile time, so each category
  // that the registered sources report has a field here, and a category without one is missing from the log line.
  tracing::info!(
    dry_run = false,
    session_id = ?scope.session_id,
    total_bytes = total_bytes(&categories),
    raw_transport = count_of(&categories, "raw-transport"),
    claude_transcripts = count_of(&categories, "claude-transcripts"),
    codex_rollouts = count_of(&categories, "codex-rollouts"),
    "storage_cool_sweep_done"
  );
  Ok(categories)
}

/// The one-line summary the handler returns (`format_sweep_line`): each category, then the total, in GiB to two places.
pub fn format_sweep_line(categories: &[CategoryResult]) -> String {
  let parts: Vec<String> = categories
    .iter()
    .map(|category| format!("{} {} {} {:.2} GiB", category.name, category.count, category.unit, gib(category.bytes)))
    .collect();
  format!("total {:.2} GiB ({})", gib(total_bytes(categories)), parts.join("; "))
}

fn total_bytes(categories: &[CategoryResult]) -> u64 {
  categories.iter().map(|category| category.bytes).sum()
}

fn count_of(categories: &[CategoryResult], name: &str) -> Option<u64> {
  categories.iter().find(|category| category.name == name).map(|category| category.count)
}

fn gib(bytes: u64) -> f64 {
  bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/// Every session's cold verdict and backend reference (`_scan_sessions`). A session whose metadata cannot be read is
/// skipped; a document that is not a JSON object fails the run.
fn scan_sessions(sessions_dir: &Path, now: UtcDateTime) -> anyhow::Result<HashMap<String, SessionFacts>> {
  let mut facts = HashMap::new();
  if !is_dir(sessions_dir) {
    return Ok(facts);
  }
  let mut session_dirs = fs::read_dir(sessions_dir)
    .and_then(|entries| entries.map(|entry| entry.map(|entry| entry.path())).collect::<Result<Vec<_>, _>>())
    .with_context(|| format!("list {}", sessions_dir.display()))?;
  session_dirs.sort();
  for session_dir in session_dirs {
    if !is_dir(&session_dir) {
      continue;
    }
    let id = session_dir.file_name().expect("a directory entry has a name").to_string_lossy().into_owned();
    let Some(meta) = read_metadata_python(&session_dir.join(SESSION_METADATA_NAME), "storage_cool_meta_read_failed")?
    else {
      continue;
    };
    let Value::Object(meta) = meta else {
      // The cold rule calls `.get` on the document, which fails for every JSON value that is not an object.
      bail!("session {id} metadata is not a JSON object");
    };
    facts.insert(
      id.clone(),
      SessionFacts { id, cold: is_cold_session(&meta, now), cc_session_id: optional_text(meta.get("cc_session_id")) },
    );
  }
  Ok(facts)
}

/// The one cold rule (`storage_cool.is_cold_session`): archived, and idle at least `MIN_IDLE_DAYS`. A field that is
/// missing or unusable never qualifies.
fn is_cold_session(meta: &Map<String, Value>, now: UtcDateTime) -> bool {
  // `str(status)` equals "archived" only for that string, so a status of any other JSON type never qualifies.
  if !matches!(meta.get("status"), Some(Value::String(status)) if *status == SESSION_ARCHIVED) {
    return false;
  }
  // `if not raw_updated` drops the falsy values before `str()` reads the rest.
  let Some(raw_updated) = meta.get("updated_at").filter(|value| is_truthy(value)) else {
    return false;
  };
  let Some(updated_at) = utc_datetime::parse(&pyfmt::python_str(raw_updated)) else {
    return false;
  };
  now.into_inner() - updated_at >= time::Duration::days(MIN_IDLE_DAYS)
}

/// `storage_cool._optional_str`: the Python `str()` text of a truthy value, and none for a falsy one.
fn optional_text(value: Option<&Value>) -> Option<String> {
  value.filter(|value| is_truthy(value)).map(pyfmt::python_str)
}

/// Python's truth value of a JSON value: null, false, zero, and the empty string, array and object are false.
fn is_truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(flag) => *flag,
    Value::Number(number) => number.as_f64().is_some_and(|float| float != 0.0),
    Value::String(text) => !text.is_empty(),
    Value::Array(items) => !items.is_empty(),
    Value::Object(members) => !members.is_empty(),
  }
}

/// Backend session ids that metadata references, each with the sessions that reference it (`_scan_references`).
fn scan_references(facts: &HashMap<String, SessionFacts>) -> HashMap<String, Vec<SessionFacts>> {
  let mut references: HashMap<String, Vec<SessionFacts>> = HashMap::new();
  for owner in facts.values() {
    if let Some(backend_session) = &owner.cc_session_id {
      references.entry(backend_session.clone()).or_default().push(owner.clone());
    }
  }
  references
}

/// Deletes the reserved transport names inside one cold session's managed run directories (`_sweep_raw_transport`).
/// A file that a v2 run record references is kept.
fn sweep_raw_transport(session_dir: &Path, counter: &mut TransportCounter) -> anyhow::Result<()> {
  let referenced = run_referenced_transport(session_dir)?;
  for root in managed_transport_dirs(session_dir) {
    for entry in sorted_scan(&root).unwrap_or_default() {
      if !is_file(&entry) || !is_transport_name(&entry) {
        continue;
      }
      if referenced.contains(&realpath(&entry)) {
        continue;
      }
      counter.delete_file(&entry);
    }
  }
  Ok(())
}

/// The two directory shapes whose direct children the transport rule governs (`_managed_transport_dirs`): each master
/// run directory, and each thread's data directory. Symlinks are not followed.
fn managed_transport_dirs(session_dir: &Path) -> Vec<PathBuf> {
  let mut managed = Vec::new();
  let data_root = session_dir.join(DATA_DIR_NAME);
  let master_runs = data_root.join(MASTER_RUNS_DIR_NAME);
  if is_real_dir(&data_root) && is_real_dir(&master_runs) {
    for run_dir in sorted_scan(&master_runs).unwrap_or_default() {
      if is_real_dir(&run_dir) {
        managed.push(run_dir);
      }
    }
  }
  let threads_dir = session_dir.join(THREADS_DIR_NAME);
  if is_real_dir(&threads_dir) {
    for thread_dir in sorted_scan(&threads_dir).unwrap_or_default() {
      let data_dir = thread_dir.join(DATA_DIR_NAME);
      if is_real_dir(&thread_dir) && is_real_dir(&data_dir) {
        managed.push(data_dir);
      }
    }
  }
  managed
}

/// The transport files that a v2 run record of this session references (`_run_referenced_transport`). Migrated runs
/// point at their original evidence, which must stay readable, so the sweep keeps it.
fn run_referenced_transport(session_dir: &Path) -> anyhow::Result<HashSet<PathBuf>> {
  let runs_dir = session_dir.join(DATA_DIR_NAME).join(RUNS_DIR_NAME);
  let mut referenced = HashSet::new();
  if !is_real_dir(&runs_dir) {
    return Ok(referenced);
  }
  for run_dir in sorted_scan(&runs_dir).unwrap_or_default() {
    if !is_real_dir(&run_dir) {
      continue;
    }
    // A record that is not a JSON object references nothing, as `isinstance(meta, dict)` skips it.
    let Some(Value::Object(meta)) =
      read_metadata_python(&run_dir.join(RUN_METADATA_NAME), "storage_cool_run_meta_read_failed")?
    else {
      continue;
    };
    for key in RUN_EVIDENCE_KEYS {
      if let Some(path) = meta.get(key).and_then(Value::as_str).filter(|path| !path.is_empty()) {
        referenced.insert(realpath(Path::new(path)));
      }
    }
  }
  Ok(referenced)
}

/// One of the five reserved transport names, or its `<name>.<N>` rotation variant (`_is_transport_name`).
fn is_transport_name(path: &Path) -> bool {
  let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
    return false;
  };
  if RAW_TRANSPORT_NAMES.contains(&name) {
    return true;
  }
  name.rsplit_once('.').is_some_and(|(base, suffix)| RAW_TRANSPORT_NAMES.contains(&base) && pystr::isdigit(suffix))
}

/// One directory's entries, sorted. A failed listing logs and yields None, so one unreadable directory never stops the
/// run (`usage_sources.sorted_scan`).
fn sorted_scan(root: &Path) -> Option<Vec<PathBuf>> {
  let listing = fs::read_dir(root)
    .and_then(|entries| entries.map(|entry| entry.map(|entry| entry.path())).collect::<Result<Vec<_>, _>>());
  match listing {
    Ok(mut entries) => {
      entries.sort();
      Some(entries)
    }
    Err(error) => {
      tracing::warn!(dir = %root.display(), error = %error, "storage_cool_dir_scan_failed");
      None
    }
  }
}

/// `Path.is_dir()` and `Path.is_file()`: each follows links, and a failed stat reads as false, as `os.path.isdir` does.
fn is_dir(path: &Path) -> bool {
  fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn is_file(path: &Path) -> bool {
  fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// `Path.is_dir() and not Path.is_symlink()`: a directory the path names itself, not through a link. The link's own
/// metadata is a directory only when the path is not a link, and a failed lstat reads as false.
fn is_real_dir(path: &Path) -> bool {
  fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// The raw-transport category: files deleted, and their bytes (`usage_sources.SweepCounter` with the `files` unit).
#[derive(Default)]
struct TransportCounter {
  count: u64,
  bytes: u64,
}

impl TransportCounter {
  /// Deletes one file, best effort: a failed stat or unlink logs, and the run goes on.
  fn delete_file(&mut self, path: &Path) {
    let size = match fs::metadata(path) {
      Ok(metadata) => metadata.len(),
      Err(error) => {
        tracing::warn!(path = %path.display(), error = %error, "storage_cool_file_stat_failed");
        return;
      }
    };
    if let Err(error) = fs::remove_file(path) {
      tracing::warn!(path = %path.display(), error = %error, "storage_cool_file_delete_failed");
      return;
    }
    self.count += 1;
    self.bytes += size;
  }

  fn result(&self) -> CategoryResult {
    CategoryResult { name: "raw-transport", unit: "files", count: self.count, bytes: self.bytes }
  }
}

#[cfg(test)]
#[path = "cool_storage_tests.rs"]
mod tests;
