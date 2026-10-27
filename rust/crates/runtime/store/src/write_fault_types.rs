//! Fault-injection state used by the test-only write seam.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

/// What an injected fault did to one write it matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
  /// The fault failed the write with its errno.
  Failed,
  /// The fault had already failed its `times` writes, so this one went through.
  Written,
}

/// The writes a fault matches.
pub(crate) enum Target {
  ChatAppend { event_type: String, session_ids: Vec<String> },
  RunRecord { only_finished: bool },
}

/// One write, as the installed faults see it.
pub(crate) enum Write<'a> {
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
  pub(crate) fn install(self: &Arc<Self>, target: Target, times: usize, errno: i32) -> WriteFault {
    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
    let fault = Fault { id, target, errno, remaining: times, hits: Vec::new() };
    self.faults.lock().expect("write fault list is not poisoned").push(fault);
    WriteFault { faults: Arc::clone(self), id }
  }

  /// The errno that fails `write`, if an installed fault fails it. Each matching fault logs the write.
  pub(crate) fn decide(&self, write: &Write<'_>) -> Option<i32> {
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
