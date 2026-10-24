use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use charliebot_agents::transport::cgroup::SessionCgroupConfig;
use charliebot_agents::transport::cursor::{RawCursorWriter, read_raw_cursor};
use charliebot_agents::transport::follow::{DEFAULT_BUFFER_LIMIT, tail_follow_events};
use charliebot_agents::transport::identity::pin_process_identity;
use charliebot_agents::transport::kill::terminate_child;
use charliebot_agents::transport::spawn::{SpawnSpec, pin_identity_and_on_spawn, spawn_piped};
use serde_json::Value;

const RAW_LINES: [&str; 4] = [
  "{\"type\":\"assistant\",\"seq\":1,\"timestamp\":\"2026-01-01T00:00:00Z\"}\n",
  "{\"type\":\"tool_use\",\"id\":\"tool-1\",\"timestamp\":\"2026-01-01T00:00:01Z\"}\n",
  "{\"type\":\"tool_result\",\"tool_use_id\":\"tool-1\",\"timestamp\":\"2026-01-01T00:00:02Z\"}\n",
  "{\"type\":\"result\",\"subtype\":\"success\",\"timestamp\":\"2026-01-01T00:00:03Z\"}\n",
];

#[tokio::test]
async fn python_and_rust_share_cursors_follow_offsets_and_pid_start() {
  let root = tempfile::tempdir().unwrap();
  let cursor = root.path().join("agent.raw.cursor");
  run_python(&["cursor_write", path_arg(&cursor), "12345"]);
  assert_eq!(read_raw_cursor(&cursor), 12345);
  RawCursorWriter::new(&cursor).write(67890).unwrap();
  assert_eq!(run_python(&["cursor_read", path_arg(&cursor)]), "67890");

  let split = 2;
  let first = RAW_LINES[..split].concat();
  let rest = RAW_LINES[split..].concat();
  let python_raw = root.path().join("python.raw.ndjson");
  let python_cursor = root.path().join("python.cursor");
  fs::write(&python_raw, &first).unwrap();
  let python_events = parse_json(&run_python(&["follow", path_arg(&python_raw), path_arg(&python_cursor), "0", "2"]));
  let resume_offset = read_raw_cursor(&python_cursor);
  assert_eq!(resume_offset, first.len() as u64);
  append(&python_raw, &rest);
  let rust_events = rust_follow(&python_raw, &python_cursor, resume_offset, 0).await;
  let uninterrupted = python_reference_follow(root.path(), &RAW_LINES.concat());
  assert_eq!(join_events(python_events, rust_events), uninterrupted);

  let rust_raw = root.path().join("rust.raw.ndjson");
  let rust_cursor = root.path().join("rust.cursor");
  fs::write(&rust_raw, &first).unwrap();
  let rust_prefix = rust_follow(&rust_raw, &rust_cursor, 0, 2).await;
  let rust_offset = read_raw_cursor(&rust_cursor);
  assert_eq!(rust_offset, first.len() as u64);
  append(&rust_raw, &rest);
  let python_suffix = parse_json(&run_python(&["follow", path_arg(&rust_raw), path_arg(&rust_cursor), "cursor", "0"]));
  assert_eq!(join_events(rust_prefix, python_suffix), uninterrupted);

  let base = root.path().join("app.slice");
  fs::create_dir(&base).unwrap();
  let spec = SpawnSpec::new(
    "/bin/sleep",
    vec!["30".into()],
    None,
    HashMap::new(),
    None,
    SessionCgroupConfig { base_dir: base, ..SessionCgroupConfig::disabled() },
  );
  let spawned = spawn_piped(&spec).await.unwrap();
  let (mut spawned, pinned) = pin_identity_and_on_spawn(spawned, |_| async { Ok::<(), ()>(()) }).await.unwrap();
  assert_eq!(pinned, pin_process_identity(spawned.child.id().unwrap()));
  assert_eq!(run_python(&["pid_start", &pinned.pid.to_string()]), pinned.pid_start.as_deref().unwrap());
  terminate_child(&mut spawned.child).await.unwrap();
}

async fn rust_follow(raw: &Path, cursor: &Path, start_offset: u64, stop_after: usize) -> Vec<Value> {
  let events = Arc::new(Mutex::new(Vec::new()));
  let events_for_handler = Arc::clone(&events);
  let events_for_liveness = Arc::clone(&events);
  tail_follow_events(
    raw,
    |event| Ok(vec![event]),
    |event| {
      events_for_handler.lock().unwrap().push(event);
      async { Ok(()) }
    },
    || stop_after > 0 && events_for_liveness.lock().unwrap().len() < stop_after,
    Some(cursor),
    start_offset,
    Duration::from_secs(10),
    DEFAULT_BUFFER_LIMIT,
    Duration::from_millis(1),
    None,
    Duration::from_secs(3600),
  )
  .await
  .unwrap();
  events.lock().unwrap().clone()
}

fn python_reference_follow(root: &Path, raw_bytes: &str) -> Vec<Value> {
  let raw = root.join("reference.raw.ndjson");
  let cursor = root.join("reference.cursor");
  fs::write(&raw, raw_bytes).unwrap();
  parse_json(&run_python(&["follow", path_arg(&raw), path_arg(&cursor), "0", "0"]))
}

fn append(path: &Path, data: &str) {
  use std::io::Write;

  fs::OpenOptions::new().append(true).open(path).unwrap().write_all(data.as_bytes()).unwrap();
}

fn join_events(first: Vec<Value>, second: Vec<Value>) -> Vec<Value> {
  first.into_iter().chain(second).collect()
}

fn parse_json(text: &str) -> Vec<Value> {
  serde_json::from_str(text).unwrap()
}

fn run_python(args: &[&str]) -> String {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap().to_path_buf();
  let helper = "rust/crates/charliebot-agents/tests/python_transport.py";
  let output = Command::new("uv").args(["run", "python", helper]).args(args).current_dir(repo_root).output().unwrap();
  assert!(
    output.status.success(),
    "Python transport helper failed:\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );
  String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn path_arg(path: &Path) -> &str {
  path.to_str().expect("test path is UTF-8")
}
