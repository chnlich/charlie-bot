//! Codex's part of the cold-storage sweep (`src/backends/codex/usage_sweep.py`): the rollout files.
//!
//! A rollout's file name embeds the Codex thread id, which is the backend session id CharlieBot metadata references.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::LazyLock;

use charliebot_registry::usage::{CategoryResult, FreelistResult, SourceSweep, SweepScope};
use regex::Regex;

use super::rollout::walk_files;

const CATEGORY: &str = "codex-rollouts";
const UNIT: &str = "files";

static SESSION_ID: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
    .expect("the session id pattern is valid")
});

/// Deletes the rollouts that the sweep's one rule covers, and reports the files it freed.
pub fn sweep(codex_home: &Path, scope: &SweepScope) -> anyhow::Result<SourceSweep> {
  let mut counter = Counter { count: 0, bytes: 0 };
  let scoped_backends = scope.scoped_backend_sessions();
  let sessions = codex_home.join(super::rollout::CODEX_SESSIONS_DIR_NAME);
  if sessions.is_dir() {
    match walk_files(&sessions, |name| name.starts_with("rollout-") && name.ends_with(".jsonl")) {
      Ok(mut rollouts) => {
        rollouts.sort();
        for path in rollouts {
          sweep_rollout(&path, scope, &scoped_backends, &mut counter);
        }
      }
      Err(error) => tracing::warn!(
        target: "charliebot_agents",
        dir = %sessions.display(),
        error = %error,
        "storage_cool_dir_scan_failed"
      ),
    }
  }
  Ok(SourceSweep {
    categories: vec![CategoryResult { name: CATEGORY, unit: UNIT, count: counter.count, bytes: counter.bytes }],
    freelist: None::<FreelistResult>,
  })
}

/// Applies the sweep rule to one rollout. A scoped run deletes only the named session's own record, and only when it is
/// cold and no live session references it. A whole-host run deletes an unreferenced rollout once its own mtime is past
/// the idle window, and a referenced one once every referencing session is cold.
fn sweep_rollout(
  path: &Path,
  scope: &SweepScope,
  scoped_backends: &std::collections::HashSet<String>,
  counter: &mut Counter,
) {
  let Some(backend_session) = rollout_session_id(path) else {
    return;
  };
  if scope.session_id.is_some() {
    if scoped_backends.contains(&backend_session) && scope.referenced_and_cold(&backend_session) {
      counter.delete_file(path, scope.dry_run);
    }
    return;
  }
  if !scope.references.contains_key(&backend_session) {
    match fs::metadata(path) {
      Ok(metadata) => {
        let idle_since = metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1e9;
        if scope.idle_past(idle_since) {
          counter.delete_file(path, scope.dry_run);
        }
      }
      Err(error) => tracing::warn!(
        target: "charliebot_agents",
        path = %path.display(),
        error = %error,
        "storage_cool_file_stat_failed"
      ),
    }
    return;
  }
  if scope.referenced_and_cold(&backend_session) {
    counter.delete_file(path, scope.dry_run);
  }
}

/// The thread id a rollout name embeds: the last five hyphen-separated segments of its stem, when they form a UUID.
fn rollout_session_id(path: &Path) -> Option<String> {
  let name = path.file_name()?.to_string_lossy().into_owned();
  let stem = name.strip_suffix(".jsonl")?;
  if !stem.starts_with("rollout-") {
    return None;
  }
  let segments: Vec<&str> = stem.split('-').collect();
  let candidate = segments[segments.len().saturating_sub(5)..].join("-");
  SESSION_ID.is_match(&candidate).then_some(candidate)
}

/// The category's count and freed bytes (`usage_sources.SweepCounter` for a file unit).
struct Counter {
  count: u64,
  bytes: u64,
}

impl Counter {
  /// Deletes one file, or only counts it in a dry run. A file that cannot be statted or removed is logged and skipped.
  fn delete_file(&mut self, path: &Path, dry_run: bool) {
    let size = match fs::metadata(path) {
      Ok(metadata) => metadata.len(),
      Err(error) => {
        tracing::warn!(
          target: "charliebot_agents",
          path = %path.display(),
          error = %error,
          "storage_cool_file_stat_failed"
        );
        return;
      }
    };
    if !dry_run && let Err(error) = fs::remove_file(path) {
      tracing::warn!(
        target: "charliebot_agents",
        path = %path.display(),
        error = %error,
        "storage_cool_file_delete_failed"
      );
      return;
    }
    self.count += 1;
    self.bytes += size;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rollout_names_yield_their_thread_id_and_others_yield_none() {
    let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    assert_eq!(
      rollout_session_id(Path::new(&format!("x/rollout-2026-10-18T10-00-00-{id}.jsonl"))),
      Some(id.to_string())
    );
    assert_eq!(rollout_session_id(Path::new("x/rollout-not-a-thread.jsonl")), None);
    assert_eq!(rollout_session_id(Path::new(&format!("x/notes-{id}.jsonl"))), None);
  }
}
