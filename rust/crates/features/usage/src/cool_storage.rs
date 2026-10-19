//! The cold-session storage sweep (`src/features/usage/storage_cool.py`) and its scheduled handler `cool_storage`: a real
//! run with no dry run, no session scope and no vacuum.
//!
//! A session is cold when its metadata reads `archived` and its `updated_at` is at least `MIN_IDLE_DAYS` old. The sweep
//! deletes the raw transport files of cold sessions (an allowlist of names inside two directory shapes), then lets each
//! registered usage source delete its own store through `UsageSourceImpl::sweep`. A real run first captures this host's
//! token usage into the usage ledger, and a capture error aborts before any deletion.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::paths::realpath;
use charliebot_registry::usage::{CategoryResult, SessionFacts, SweepScope};
use charliebot_registry::{FeatureContext, Registry};
use charliebot_store::Store;
use charliebot_store::metadata::UtcDateTime;
use charliebot_store::run_records::{
  CURSOR_NAME, DATA_DIR_NAME, MASTER_RUNS_DIR_NAME, RAW_LOG_NAME, RUN_METADATA_NAME, RUNS_DIR_NAME, STDERR_LOG_NAME,
  THREADS_DIR_NAME,
};
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::capture_into_default_ledger;

/// `constants.MIN_IDLE_DAYS`: how long an archived session must sit idle to count as cold.
const MIN_IDLE_DAYS: i64 = 14;
/// `storage_cool.ORPHAN_IDLE_DAYS`: how long an unreferenced backend record must sit idle before the sweep takes it.
const ORPHAN_IDLE_DAYS: i64 = 2;
/// `RAW_TRANSPORT_NAMES`: the three agent-pipe names from `runs.py`, and the two tee names the backend writers use.
const RAW_TRANSPORT_NAMES: [&str; 5] = [RAW_LOG_NAME, STDERR_LOG_NAME, CURSOR_NAME, "stdout.log", "stderr.log"];
/// The run-record keys that name transport evidence a migrated run keeps readable.
const RUN_EVIDENCE_KEYS: [&str; 3] = ["raw_log_ref", "events_ref", "result_ref"];
const SESSION_ARCHIVED: &str = "archived";

/// The `cool_storage` scheduled handler: the real sweep at the wall clock, off the async runtime.
pub fn run_handler(context: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<String>> {
  async move {
    let context_for_sweep = Arc::clone(&context);
    let categories = tokio::task::spawn_blocking(move || {
      let config = context_for_sweep.config.get()?;
      run_cool_sweep(config, &context_for_sweep.store, &context_for_sweep.registry, UtcDateTime::now())
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
  store: &Store,
  registry: &Registry,
  now: UtcDateTime,
) -> anyhow::Result<Vec<CategoryResult>> {
  // The capture guards every deletion: it runs before the scan, and its failure propagates so nothing is deleted.
  capture_into_default_ledger(&cfg, registry)?;
  let facts = scan_sessions(store, now)?;
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
  let counts: Map<String, Value> =
    categories.iter().map(|category| (category.name.replace('-', "_"), Value::from(category.count))).collect();
  tracing::info!(
    dry_run = false,
    total_bytes = total_bytes(&categories),
    categories = ?counts,
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

fn gib(bytes: u64) -> f64 {
  bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/// Every session's cold verdict and backend reference, from the store's raw metadata scan (`_scan_sessions`).
fn scan_sessions(store: &Store, now: UtcDateTime) -> anyhow::Result<HashMap<String, SessionFacts>> {
  let mut facts = HashMap::new();
  for row in store.raw_metadata_scan()? {
    let meta = match row.value {
      // `load_json_meta` reads a JSON null as absent.
      Value::Null => continue,
      Value::Object(meta) => meta,
      // Python's cold rule calls `.get` on the value, so a metadata file that is not an object fails the sweep.
      _ => bail!("session {} metadata is not a JSON object", row.session_id),
    };
    let cold = is_cold_session(&meta, now);
    let cc_session_id = optional_text(&meta, &row.session_id)?;
    facts.insert(row.session_id.clone(), SessionFacts { id: row.session_id, cold, cc_session_id });
  }
  Ok(facts)
}

/// The one cold rule: archived, and idle at least `MIN_IDLE_DAYS`. A field that is missing or unreadable never qualifies.
fn is_cold_session(meta: &Map<String, Value>, now: UtcDateTime) -> bool {
  if meta.get("status").and_then(Value::as_str) != Some(SESSION_ARCHIVED) {
    return false;
  }
  let Some(updated_text) = meta.get("updated_at").and_then(Value::as_str) else {
    return false;
  };
  let Ok(updated_at) = UtcDateTime::parse(updated_text) else {
    return false;
  };
  now.into_inner() - updated_at.into_inner() >= time::Duration::days(MIN_IDLE_DAYS)
}

/// `_optional_str(meta.get("cc_session_id"))`: an absent or empty reference is none.
fn optional_text(meta: &Map<String, Value>, session_id: &str) -> anyhow::Result<Option<String>> {
  match meta.get("cc_session_id") {
    None | Some(Value::Null) => Ok(None),
    Some(Value::String(text)) => Ok((!text.is_empty()).then(|| text.clone())),
    Some(_) => bail!("session {session_id}: cc_session_id is not a string"),
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
  for root in managed_transport_dirs(session_dir)? {
    let Some(entries) = sorted_scan(&root) else {
      continue;
    };
    for entry in entries {
      if !is_file(&entry)? || !is_transport_name(&entry) {
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
fn managed_transport_dirs(session_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
  let mut managed = Vec::new();
  let data_root = session_dir.join(DATA_DIR_NAME);
  let master_runs = data_root.join(MASTER_RUNS_DIR_NAME);
  if is_real_dir(&data_root)? && is_real_dir(&master_runs)? {
    for run_dir in sorted_scan(&master_runs).unwrap_or_default() {
      if is_real_dir(&run_dir)? {
        managed.push(run_dir);
      }
    }
  }
  let threads_dir = session_dir.join(THREADS_DIR_NAME);
  if is_real_dir(&threads_dir)? {
    for thread_dir in sorted_scan(&threads_dir).unwrap_or_default() {
      if !is_real_dir(&thread_dir)? {
        continue;
      }
      let data_dir = thread_dir.join(DATA_DIR_NAME);
      if is_real_dir(&data_dir)? {
        managed.push(data_dir);
      }
    }
  }
  Ok(managed)
}

/// The transport files that a v2 run record of this session references (`_run_referenced_transport`). Migrated runs
/// point at their original evidence, which must stay readable, so the sweep keeps it.
fn run_referenced_transport(session_dir: &Path) -> anyhow::Result<HashSet<PathBuf>> {
  let runs_dir = session_dir.join(DATA_DIR_NAME).join(RUNS_DIR_NAME);
  let mut referenced = HashSet::new();
  if !is_real_dir(&runs_dir)? {
    return Ok(referenced);
  }
  for run_dir in sorted_scan(&runs_dir).unwrap_or_default() {
    if !is_real_dir(&run_dir)? {
      continue;
    }
    let Some(meta) = read_run_metadata(&run_dir.join(RUN_METADATA_NAME))? else {
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

/// `json_utils.load_json_meta` for a run record: a missing, unreadable or malformed file reads as absent (the last two
/// are logged), and invalid UTF-8 fails the sweep as Python's text read does.
fn read_run_metadata(path: &Path) -> anyhow::Result<Option<Map<String, Value>>> {
  let bytes = match fs::read(path) {
    Ok(bytes) => bytes,
    Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
    Err(error) => {
      tracing::debug!(path = %path.display(), error = %error, "storage_cool_run_meta_read_failed");
      return Ok(None);
    }
  };
  let text = String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;
  match serde_json::from_str::<Value>(&text) {
    Ok(Value::Object(meta)) => Ok(Some(meta)),
    // A JSON null is absent; any other value fails the dict check in `_run_referenced_transport`.
    Ok(_) => Ok(None),
    Err(error) => {
      tracing::debug!(path = %path.display(), error = %error, "storage_cool_run_meta_read_failed");
      Ok(None)
    }
  }
}

/// One of the five reserved transport names, or its `<name>.<N>` rotation variant. The suffix is ASCII digits: the
/// rotation writes ASCII, and a name Python's `isdigit` alone would accept stays in place.
fn is_transport_name(path: &Path) -> bool {
  let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
    return false;
  };
  if RAW_TRANSPORT_NAMES.contains(&name) {
    return true;
  }
  name.rsplit_once('.').is_some_and(|(base, suffix)| {
    RAW_TRANSPORT_NAMES.contains(&base) && !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
  })
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

/// `Path.stat()` for a sweep path: follows links, and a missing path reads as None.
fn stat_followed(path: &Path) -> anyhow::Result<Option<fs::Metadata>> {
  match fs::metadata(path) {
    Ok(metadata) => Ok(Some(metadata)),
    Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error).with_context(|| format!("stat {}", path.display())),
  }
}

/// `Path.is_file()`: follows links, and a missing path is not a file.
fn is_file(path: &Path) -> anyhow::Result<bool> {
  Ok(stat_followed(path)?.is_some_and(|metadata| metadata.is_file()))
}

/// `Path.is_dir() and not Path.is_symlink()`: the directory shapes the sweep descends into.
fn is_real_dir(path: &Path) -> anyhow::Result<bool> {
  let Some(metadata) = stat_followed(path)? else {
    return Ok(false);
  };
  Ok(metadata.is_dir() && !fs::symlink_metadata(path)?.file_type().is_symlink())
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
