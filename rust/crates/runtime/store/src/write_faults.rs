//! Test-only write faults: an errno injected into the first matching chat-event append or run-record write.
//!
//! The seam compiles only in test builds of this crate or under the `write-faults` feature, which a dev-dependency
//! enables, so production builds carry none of it. A fault belongs to one `Store` value, so parallel tests never see
//! each other's faults. A failed
//! write returns before it writes its event line or record file, so no partial event or record is left behind; a
//! directory the caller created before the write stays, as it would after a real failure. Each injector returns a
//! `WriteFault` guard that logs the writes it matches and removes its fault when the guard drops.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::Value;

use crate::{RunRecord, Store, StoreError, StoreResult};

/// What an injected fault did to one write it matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
  /// The fault failed the write with its errno.
  Failed,
  /// The fault had already failed its `times` writes, so this one went through.
  Written,
}

/// The writes a fault matches.
enum Target {
  ChatAppend { event_type: String, session_ids: Vec<String> },
  RunRecord { only_finished: bool },
}

/// One write, as the installed faults see it.
enum Write<'a> {
  ChatAppend { session_id: &'a str, event_type: Option<&'a str> },
  RunRecord { finished: bool },
}

impl Target {
  fn matches(&self, write: &Write<'_>) -> bool {
    match (self, write) {
      (Target::ChatAppend { event_type, session_ids }, Write::ChatAppend { session_id, event_type: written }) => {
        *written == Some(event_type.as_str()) && session_ids.iter().any(|id| id == *session_id)
      }
      (Target::RunRecord { only_finished }, Write::RunRecord { finished }) => *finished || !*only_finished,
      _ => false,
    }
  }
}

struct Fault {
  id: u64,
  target: Target,
  errno: i32,
  remaining: usize,
  hits: Vec<Hit>,
}

/// The faults installed on one store. Every fault decides each write it matches on its own; a write that any of
/// them fails carries the errno of the first one in installation order.
#[derive(Default)]
pub(crate) struct WriteFaults {
  next_id: AtomicU64,
  faults: StdMutex<Vec<Fault>>,
}

impl WriteFaults {
  fn install(self: &Arc<Self>, target: Target, times: usize, errno: i32) -> WriteFault {
    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
    let fault = Fault { id, target, errno, remaining: times, hits: Vec::new() };
    self.faults.lock().expect("write fault list is not poisoned").push(fault);
    WriteFault { faults: Arc::clone(self), id }
  }

  /// The errno that fails `write`, if an installed fault fails it. Each matching fault logs the write.
  fn decide(&self, write: &Write<'_>) -> Option<i32> {
    let mut faults = self.faults.lock().expect("write fault list is not poisoned");
    let mut failed = None;
    for fault in faults.iter_mut().filter(|fault| fault.target.matches(write)) {
      if fault.remaining > 0 {
        fault.remaining -= 1;
        fault.hits.push(Hit::Failed);
        failed.get_or_insert(fault.errno);
      } else {
        fault.hits.push(Hit::Written);
      }
    }
    failed
  }
}

/// An installed write fault. Dropping the guard removes the fault.
#[must_use = "the fault is removed as soon as the guard drops"]
pub struct WriteFault {
  faults: Arc<WriteFaults>,
  id: u64,
}

impl WriteFault {
  /// One entry per write this fault matched so far, in write order.
  pub fn hits(&self) -> Vec<Hit> {
    let faults = self.faults.faults.lock().expect("write fault list is not poisoned");
    let fault = faults.iter().find(|fault| fault.id == self.id).expect("a fault is listed until its guard drops");
    fault.hits.clone()
  }
}

impl Drop for WriteFault {
  fn drop(&mut self) {
    self.faults.faults.lock().expect("write fault list is not poisoned").retain(|fault| fault.id != self.id);
  }
}

/// Fail the first `times` chat-event appends whose `type` is `event_type` and whose session is in `session_ids`,
/// with `errno`. Every other append writes through.
pub fn inject_chat_append_fault(
  store: &Store,
  event_type: &str,
  session_ids: &[&str],
  times: usize,
  errno: i32,
) -> WriteFault {
  let target = Target::ChatAppend {
    event_type: event_type.to_string(),
    session_ids: session_ids.iter().map(|id| id.to_string()).collect(),
  };
  store.write_faults.install(target, times, errno)
}

/// Fail the first `times` run-record writes with `errno`. With `only_finished`, only a write that sets `ended_at`,
/// a run's finish, is matched; otherwise every record write is.
pub fn inject_run_record_write_fault(store: &Store, only_finished: bool, times: usize, errno: i32) -> WriteFault {
  store.write_faults.install(Target::RunRecord { only_finished }, times, errno)
}

impl Store {
  /// Fail a chat-event append that an installed fault matches, before the event log is touched.
  pub(crate) fn fail_chat_append(&self, session_id: &str, event: &Value, path: &Path) -> StoreResult<()> {
    let write = Write::ChatAppend { session_id, event_type: event.get("type").and_then(Value::as_str) };
    injected(self.write_faults.decide(&write), path)
  }

  /// Fail a run-record write that an installed fault matches, before the record or its directory is touched.
  pub(crate) fn fail_run_record_write(&self, run: &RunRecord, path: &Path) -> StoreResult<()> {
    injected(self.write_faults.decide(&Write::RunRecord { finished: run.ended_at.is_some() }), path)
  }
}

/// The error a failed write reports, shaped as the store's I/O errors are: the path, then the OS error.
fn injected(errno: Option<i32>, path: &Path) -> StoreResult<()> {
  match errno {
    Some(errno) => Err(StoreError::new(format!("{}: {}", path.display(), io::Error::from_raw_os_error(errno)))),
    None => Ok(()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::store::test_support::make_test_store;
  use crate::{RunKind, RunOutcome, RunOutcomeKind};
  use nix::errno::Errno;
  use serde_json::json;

  fn os_error_text(errno: i32) -> String {
    io::Error::from_raw_os_error(errno).to_string()
  }

  fn run_record(session_id: &str, run_id: &str) -> RunRecord {
    RunRecord {
      id: run_id.to_string(),
      session_id: session_id.to_string(),
      kind: RunKind::Work,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: None,
      model: None,
      native_session_id: None,
      pid: None,
      pid_start: None,
      started_at: None,
      ended_at: None,
      exit_code: None,
      repo_path: None,
      base_branch: None,
      branch_name: None,
      worktree_path: None,
      sequence_ref: None,
      raw_log_ref: None,
      events_ref: None,
      result_ref: None,
    }
  }

  #[tokio::test]
  async fn chat_append_fault_fails_the_first_matching_appends_then_writes_through() {
    let (_home, store) = make_test_store();
    let errno = Errno::ENOSPC as i32;
    let fault = inject_chat_append_fault(&store, "run_finished", &["worker"], 2, errno);
    for _ in 0..2 {
      let error = store.append_chat_event("worker", json!({"type": "run_finished"})).await.unwrap_err();
      assert!(error.to_string().contains(&os_error_text(errno)), "{error}");
    }
    assert!(!store.chat_events_path("worker").exists());

    let written = store.append_chat_event("worker", json!({"type": "run_finished"})).await.unwrap();
    assert_eq!(store.load_chat_events_sync("worker").unwrap(), vec![written]);
    assert_eq!(fault.hits(), [Hit::Failed, Hit::Failed, Hit::Written]);
  }

  #[tokio::test]
  async fn chat_append_fault_leaves_other_types_sessions_and_stores_alone() {
    let (_home, store) = make_test_store();
    let (_other_home, other) = make_test_store();
    let fault = inject_chat_append_fault(&store, "run_finished", &["worker"], 5, Errno::EIO as i32);
    store.append_chat_event("worker", json!({"type": "user"})).await.unwrap();
    store.append_chat_event("manager", json!({"type": "run_finished"})).await.unwrap();
    other.append_chat_event("worker", json!({"type": "run_finished"})).await.unwrap();
    assert!(fault.hits().is_empty());
  }

  #[tokio::test]
  async fn dropping_the_guards_removes_both_kinds_of_fault() {
    let (_home, store) = make_test_store();
    let errno = Errno::ENOSPC as i32;
    let chat = inject_chat_append_fault(&store, "run_finished", &["worker"], 10, errno);
    let record = inject_run_record_write_fault(&store, false, 10, errno);
    assert!(store.append_chat_event("worker", json!({"type": "run_finished"})).await.is_err());
    drop(chat);
    drop(record);

    let guard = store.control_lock().await;
    store.register_run_locked(&guard, "worker", run_record("worker", "run-a"), None).await.unwrap();
    store.append_chat_event("worker", json!({"type": "run_finished"})).await.unwrap();
  }

  #[tokio::test]
  async fn run_record_fault_passes_non_finish_writes_and_fails_the_finish_once() {
    let (_home, store) = make_test_store();
    let errno = Errno::EIO as i32;
    let fault = inject_run_record_write_fault(&store, true, 1, errno);
    let guard = store.control_lock().await;
    store.register_run_locked(&guard, "worker", run_record("worker", "run-a"), None).await.unwrap();
    assert!(fault.hits().is_empty());

    let error =
      store.record_finish_locked(&guard, "worker", "run-a", RunOutcome::new(RunOutcomeKind::Failed)).await.unwrap_err();
    assert!(error.to_string().contains(&os_error_text(errno)), "{error}");
    assert!(store.read_run_sync("worker", "run-a").unwrap().unwrap().ended_at.is_none());

    let finished =
      store.record_finish_locked(&guard, "worker", "run-a", RunOutcome::new(RunOutcomeKind::Failed)).await.unwrap();
    assert!(finished.ended_at.is_some());
    assert_eq!(fault.hits(), [Hit::Failed, Hit::Written]);
  }

  #[tokio::test]
  async fn run_record_fault_without_the_finish_gate_fails_the_first_record_write() {
    let (_home, store) = make_test_store();
    let errno = Errno::ENOSPC as i32;
    let fault = inject_run_record_write_fault(&store, false, 1, errno);
    let guard = store.control_lock().await;
    let error = store.register_run_locked(&guard, "worker", run_record("worker", "run-a"), None).await.unwrap_err();
    assert!(error.to_string().contains(&os_error_text(errno)), "{error}");
    assert!(store.read_run_sync("worker", "run-a").unwrap().is_none());

    store.register_run_locked(&guard, "worker", run_record("worker", "run-a"), None).await.unwrap();
    assert_eq!(fault.hits(), [Hit::Failed, Hit::Written]);
  }
}
