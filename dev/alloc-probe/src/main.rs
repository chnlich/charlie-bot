//! Allocator probe: the server's per-append event-write churn, reduced to a standalone program.
//!
//! Each append re-reads the session's whole `chat_events.jsonl` and parses every line to a `serde_json::Value` tree.
//! This intentionally repeats the transient allocation pattern of aggregator history seeding and fact scans as a
//! stress model; the server does not re-parse the full log on every append. The 64 most recently appended parsed
//! sessions stay in an LRU, as the projection and transcript memos keep them. Eight worker threads share the appends,
//! and each session's appends are serialized as they are by the server.
//!
//! The probe prints, in kB from /proc/self/status:
//!   PEAK_RSS          VmHWM after the workload (the peak, reset before the workload starts)
//!   RSS_END           VmRSS right after the workload
//!   RSS_QUIET_HELD    VmRSS after a quiet period, with the 64-session LRU still held
//!   RSS_QUIET_FREED   VmRSS after a quiet period, once the LRU is dropped (the live data is released)
//!
//! `run.sh` builds every allocator variant and runs each on this workload. One variant by hand:
//!   cargo run --release                              # glibc malloc
//!   cargo run --release --features mimalloc-purge0   # mimalloc with the server's purge delay of zero
//!   cargo run --release --features jemalloc          # jemalloc global allocator

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

#[cfg(any(feature = "mimalloc", feature = "mimalloc-purge0"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const ALLOCATOR: &str = if cfg!(feature = "mimalloc-purge0") {
  "mimalloc-purge0"
} else if cfg!(feature = "mimalloc") {
  "mimalloc"
} else if cfg!(feature = "jemalloc") {
  "jemalloc"
} else {
  "glibc"
};

/// The server's allocator setup (rust/crates/app/server/src/allocator.rs): purge freed pages at once.
#[cfg(feature = "mimalloc-purge0")]
fn configure_allocator() {
  // SAFETY: mi_option_set writes one entry of mimalloc's option table; it takes no pointer and allocates nothing.
  unsafe { libmimalloc_sys::mi_option_set(15, 0) }
}

#[cfg(not(feature = "mimalloc-purge0"))]
fn configure_allocator() {}

const SESSIONS: usize = 200;
const MIN_LINES: usize = 200;
const MAX_LINES: usize = 1600;
const APPENDS: usize = 8000;
const THREADS: usize = 8;
const LRU_SESSIONS: usize = 64;
const QUIET: Duration = Duration::from_secs(30);

type Lru = Mutex<VecDeque<(usize, Arc<Vec<Value>>)>>;

fn status_kib(field: &str) -> u64 {
  let status = fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
  let line = status
    .lines()
    .find(|line| line.starts_with(field))
    .unwrap_or_else(|| panic!("{field} missing from /proc/self/status"));
  line.split_whitespace().nth(1).expect("kB value").parse().expect("numeric kB value")
}

fn event_line(session: usize, seq: usize) -> String {
  let text = format!("session {session} event {seq} ").repeat(20);
  json!({ "seq": seq, "session": session, "kind": "assistant_text", "text": text }).to_string()
}

fn session_path(dir: &Path, session: usize) -> std::path::PathBuf {
  dir.join(format!("session-{session:03}.jsonl"))
}

fn generate_corpus(dir: &Path) {
  for session in 0..SESSIONS {
    let lines = MIN_LINES + (session * 7919) % (MAX_LINES - MIN_LINES);
    let mut body = String::new();
    for seq in 0..lines {
      body.push_str(&event_line(session, seq));
      body.push('\n');
    }
    fs::write(session_path(dir, session), body).expect("write session log");
  }
}

fn load_parsed(path: &Path) -> Vec<Value> {
  fs::read_to_string(path)
    .expect("read session log")
    .lines()
    .map(|line| serde_json::from_str(line).expect("parse event line"))
    .collect()
}

/// One append: re-parse the whole log, write one event line, and keep the parsed list in the LRU.
fn append(dir: &Path, lru: &Lru, session_locks: &[Mutex<()>], session: usize, seq: usize) {
  let _session_guard = session_locks[session].lock().expect("session lock");
  let path = session_path(dir, session);
  let mut events = load_parsed(&path);
  let line = event_line(session, seq);
  let mut file = OpenOptions::new().append(true).open(&path).expect("open session log");
  file.write_all(line.as_bytes()).expect("append event");
  file.write_all(b"\n").expect("append newline");
  events.push(serde_json::from_str(&line).expect("parse new event"));

  let mut lru = lru.lock().expect("lru lock");
  lru.retain(|(s, _)| *s != session);
  lru.push_back((session, Arc::new(events)));
  while lru.len() > LRU_SESSIONS {
    lru.pop_front();
  }
}

fn main() {
  configure_allocator();
  let dir = std::env::temp_dir().join(format!("alloc-probe-{}", std::process::id()));
  fs::create_dir_all(&dir).expect("create scratch directory");
  generate_corpus(&dir);

  // Reset the peak to the current RSS so PEAK_RSS covers the workload only (clear_refs value 5).
  fs::write("/proc/self/clear_refs", "5").expect("reset peak RSS");
  let lru: Lru = Mutex::new(VecDeque::new());
  let session_locks: Vec<_> = (0..SESSIONS).map(|_| Mutex::new(())).collect();
  let next = AtomicUsize::new(0);
  thread::scope(|scope| {
    for _ in 0..THREADS {
      scope.spawn(|| {
        loop {
          let op = next.fetch_add(1, Ordering::Relaxed);
          if op >= APPENDS {
            break;
          }
          let session = op.wrapping_mul(2_654_435_761) % SESSIONS;
          append(&dir, &lru, &session_locks, session, op);
        }
      });
    }
  });
  let peak = status_kib("VmHWM:");
  let end = status_kib("VmRSS:");

  thread::sleep(QUIET);
  let quiet_held = status_kib("VmRSS:");

  lru.lock().expect("lru lock").clear();
  thread::sleep(QUIET);
  let quiet_freed = status_kib("VmRSS:");

  println!(
    "ALLOCATOR={ALLOCATOR} PEAK_RSS={peak} RSS_END={end} RSS_QUIET_HELD={quiet_held} RSS_QUIET_FREED={quiet_freed}"
  );
  fs::remove_dir_all(&dir).expect("remove scratch directory");
}
