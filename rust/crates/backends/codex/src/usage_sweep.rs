//! Codex's part of the cold-storage sweep: the rollout files.
//!
//! A rollout's file name embeds the Codex thread id, which is the backend session id CharlieBot metadata references.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::LazyLock;

use charliebot_core::timefmt;
use charliebot_registry::usage::{
  FreelistResult, SWEEP_DIR_SCAN_FAILED_EVENT, SWEEP_FILE_STAT_FAILED_EVENT, SourceSweep, SweepScope,
};
use charliebot_registry::usage_sweep::SweepCounter;
use regex::Regex;

use super::rollout::{is_rollout_file, rollout_thread_id, walk_files};

const CATEGORY: &str = "codex-rollouts";
const UNIT: &str = "files";

static SESSION_ID: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
    .expect("the session id pattern is valid")
});

/// Deletes the rollouts that the sweep's one rule covers, and reports the files it freed.
pub fn sweep(codex_home: &Path, scope: &SweepScope) -> anyhow::Result<SourceSweep> {
  let mut counter = SweepCounter::default();
  let scoped_backends = scope.scoped_backend_sessions();
  let sessions = codex_home.join(super::rollout::CODEX_SESSIONS_DIR_NAME);
  if sessions.is_dir() {
    match walk_files(&sessions, is_rollout_file) {
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
        "{}",
        SWEEP_DIR_SCAN_FAILED_EVENT
      ),
    }
  }
  Ok(SourceSweep { categories: vec![counter.category(CATEGORY, UNIT)], freelist: None::<FreelistResult> })
}

/// Applies the sweep rule to one rollout. A scoped run deletes only the named session's own record, and only when it is
/// cold and no live session references it. A whole-host run deletes an unreferenced rollout once its own mtime is past
/// the idle window, and a referenced one once every referencing session is cold.
fn sweep_rollout(
  path: &Path,
  scope: &SweepScope,
  scoped_backends: &std::collections::HashSet<String>,
  counter: &mut SweepCounter,
) {
  let Some(backend_session) = rollout_session_id(path) else {
    return;
  };
  if scope.session_id.is_some() {
    if scoped_backends.contains(&backend_session) && scope.referenced_and_cold(&backend_session) {
      counter.delete_file(path, scope.dry_run, true);
    }
    return;
  }
  if !scope.references.contains_key(&backend_session) {
    match fs::metadata(path) {
      Ok(metadata) => {
        if scope.idle_past(timefmt::float_seconds(metadata.mtime(), metadata.mtime_nsec())) {
          counter.delete_file(path, scope.dry_run, true);
        }
      }
      Err(error) => tracing::warn!(
        target: "charliebot_agents",
        path = %path.display(),
        error = %error,
        "{}",
        SWEEP_FILE_STAT_FAILED_EVENT
      ),
    }
    return;
  }
  if scope.referenced_and_cold(&backend_session) {
    counter.delete_file(path, scope.dry_run, true);
  }
}

/// The thread id a rollout file's name embeds, when it forms a UUID.
fn rollout_session_id(path: &Path) -> Option<String> {
  let name = path.file_name()?.to_string_lossy();
  rollout_thread_id(&name).filter(|thread_id| SESSION_ID.is_match(thread_id))
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
