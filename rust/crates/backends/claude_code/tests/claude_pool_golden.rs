use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use charliebot_claude_code::{claude_accounts, claude_metadata, claude_relay, master_cc_relay, turn_contribution};
use charliebot_registry::Registry;
use charliebot_registry::backend::LaunchWatch;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use serde_json::{Value, json};
use tempfile::TempDir;

const PYTHON_FIXTURE: &str = include_str!("fixtures/claude_pool_golden.json");

const NOW: &str = "2026-09-06T20:00:00Z";
const ONE_HOUR_AGO: &str = "2026-09-06T18:30:00Z";
const FIFTY_MINUTES_AGO: &str = "2026-09-06T19:10:00Z";
const FUTURE_RESET: f64 = 1_798_848_000.0;

fn account_data(root: &Path) -> (Value, Vec<PathBuf>) {
  let home = root.join("config-home");
  fs::create_dir_all(&home).unwrap();
  let mut accounts = Vec::new();
  let mut directories = Vec::new();
  for label in ["a", "b", "c"] {
    let directory = root.join(format!("claude-{label}"));
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join(".credentials.json"), r#"{"claudeAiOauth":{"accessToken":"synthetic-token"}}"#).unwrap();
    accounts.push(json!({"label": label, "config_dir": directory}));
    directories.push(directory);
  }
  (
    json!({
      "home": home,
      "accounts": accounts,
      "pools": {"alpha": ["a", "b"], "beta": ["b", "c"]},
    }),
    directories,
  )
}

fn rust_config(data: &Value) -> charliebot_core::config::CharlieBotConfig {
  let home = Path::new(data["home"].as_str().unwrap());
  let mut account_rows = String::new();
  for account in data["accounts"].as_array().unwrap() {
    account_rows.push_str(&format!(
      "    - label: {}\n      config_dir: '{}'\n",
      account["label"].as_str().unwrap(),
      account["config_dir"].as_str().unwrap(),
    ));
  }
  let mut pool_rows = String::new();
  for (pool, members) in data["pools"].as_object().unwrap() {
    let labels = members.as_array().unwrap().iter().map(Value::as_str).collect::<Option<Vec<_>>>().unwrap();
    pool_rows.push_str(&format!("    {pool}: [{}]\n", labels.join(", ")));
  }
  fs::write(
    home.join("config.yaml"),
    format!(
      "server:\n  session_memory_max_mb: 0\nbackends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n      account_pool: alpha\naccounts:\n  claude:\n{account_rows}  claude_pools:\n{pool_rows}"
    ),
  )
  .unwrap();
  let mut registry = Registry::new();
  registry.register_package("src.backends.claude_code", charliebot_claude_code::register).unwrap();
  registry.load_config(home).unwrap()
}

fn python_reference(mode: &str, _data: &Value, _temporary: &TempDir) -> Value {
  let fixture: Value = serde_json::from_str(PYTHON_FIXTURE).unwrap();
  assert_eq!(fixture["source"], "rust/crates/runtime/agents/tests/claude_pool_reference.py");
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  fixture["expected_by_mode"][mode].clone()
}

fn timestamp(value: &str) -> UtcDateTime {
  UtcDateTime::parse(value).unwrap()
}

fn rust_observe_readings(state: &claude_accounts::ClaudeAccountState, case: &Value, now: UtcDateTime) {
  for reading in case["readings"].as_array().unwrap() {
    claude_accounts::observe_rate_limit(state, reading["label"].as_str().unwrap(), &reading["info"], Some(now));
  }
  for label in case["auth_failures"].as_array().unwrap() {
    claude_accounts::record_auth_failure(state, label.as_str().unwrap(), Some(now));
  }
}

fn selection_matrix() -> Value {
  let recent_rejection = json!({"status":"rejected", "resetsAt":FUTURE_RESET});
  let reading = |label, utilization| {
    json!({
      "label": label,
      "info": {"unifiedWindows":{"five_hour":{"utilization":utilization}}}
    })
  };
  let rejected = |label| json!({"label":label, "info":recent_rejection});
  json!([
    {"now":NOW, "last_request":null, "current":null, "busy":[], "pool":"alpha", "readings":[], "auth_failures":[]},
    {"now":NOW, "last_request":FIFTY_MINUTES_AGO, "current":"a", "busy":[], "pool":"alpha", "readings":[reading("a",0.2),reading("b",0.6)], "auth_failures":[]},
    {"now":NOW, "last_request":FIFTY_MINUTES_AGO, "current":"a", "busy":["b"], "pool":"alpha", "readings":[reading("a",0.95),reading("b",0.1)], "auth_failures":[]},
    {"now":NOW, "last_request":FIFTY_MINUTES_AGO, "current":"a", "busy":["a"], "pool":"alpha", "readings":[reading("a",0.2),reading("b",0.6)], "auth_failures":[]},
    {"now":NOW, "last_request":FIFTY_MINUTES_AGO, "current":"a", "busy":[], "pool":"alpha", "readings":[rejected("a")], "auth_failures":[]},
    {"now":NOW, "last_request":ONE_HOUR_AGO, "current":null, "busy":["a","b","c"], "pool":"alpha", "readings":[reading("a",0.2),reading("b",0.4),reading("c",0.8)], "auth_failures":[]},
    {"now":NOW, "last_request":FIFTY_MINUTES_AGO, "current":"a", "busy":[], "pool":"beta", "readings":[reading("a",0.0),reading("b",0.8),reading("c",0.2)], "auth_failures":[]},
    {"now":NOW, "last_request":ONE_HOUR_AGO, "current":null, "busy":[], "pool":"alpha", "readings":[rejected("a"),reading("b",0.95)], "auth_failures":["b"]},
  ])
}

#[test]
fn account_selection_matrix_matches_python() {
  let temporary = tempfile::tempdir().unwrap();
  let (mut data, _directories) = account_data(temporary.path());
  data["cases"] = selection_matrix();
  let expected = python_reference("selection", &data, &temporary);
  let cfg = rust_config(&data);
  let now = timestamp(NOW);
  let mut actual = Vec::new();
  for case in data["cases"].as_array().unwrap() {
    let state = claude_accounts::ClaudeAccountState::new();
    rust_observe_readings(&state, case, now);
    let mut metadata = SessionMetadata::new("observer", "Observer", TaskProfile::Manager);
    claude_metadata::set_account(&mut metadata, case["current"].as_str());
    let running = case["busy"]
      .as_array()
      .unwrap()
      .iter()
      .enumerate()
      .map(|(index, label)| {
        let mut item = SessionMetadata::new(format!("busy-{index}"), "Busy", TaskProfile::Manager);
        claude_metadata::set_account(&mut item, label.as_str());
        item
      })
      .collect::<Vec<_>>();
    let last_request = case["last_request"].as_str().map(timestamp);
    let (chosen, cold) = master_cc_relay::choose_turn_account(
      &state,
      &cfg,
      &metadata,
      Some("claude-fable-5-1"),
      last_request,
      Some(now),
      case["pool"].as_str(),
      &running,
    )
    .unwrap();
    let unavailable = claude_accounts::unavailable_detail(&state, &cfg, Some(now), case["pool"].as_str()).unwrap();
    actual.push(json!({"chosen":chosen.map(|account| account.label), "cold":cold, "unavailable":unavailable}));
  }
  assert_eq!(Value::Array(actual), expected, "selection matrix differs from the Python baseline");
}

#[test]
fn login_required_renderer_matches_python_without_exposing_login_paths() {
  let temporary = tempfile::tempdir().unwrap();
  let (mut data, _) = account_data(temporary.path());
  let event = json!({
    "type":"claude_account_login_required",
    "account":"synthetic-account",
    "config_dir":"/synthetic/login",
    "reason":"auth_failed",
  });
  data["event"] = event.clone();
  let expected = python_reference("renderer", &data, &temporary);
  assert_eq!(turn_contribution::render_login_required(&event), expected);
}

fn move_matrix() -> Value {
  json!([
    {"name":"forward", "src":"a", "dst":"b", "id":"forward", "src_content":"first row\n", "src_sidecar":"tool payload", "dst_only":"keep destination-only"},
    {"name":"reverse", "src":"b", "dst":"a", "id":"reverse", "src_content":"second row\n", "src_sidecar":"reverse payload"},
    {"name":"newer-destination", "src":"a", "dst":"b", "id":"newer", "src_content":"old", "dst_content":"newer copy", "src_mtime":1600000000000000000_i64, "dst_mtime":1700000000000000000_i64},
    {"name":"missing-source", "src":"a", "dst":"c", "id":"missing"},
  ])
}

fn snapshot(root: &Path) -> Value {
  fn collect(root: &Path, current: &Path, files: &mut serde_json::Map<String, Value>) {
    for entry in fs::read_dir(current).unwrap() {
      let entry = entry.unwrap();
      let path = entry.path();
      if path.is_dir() {
        collect(root, &path, files);
      } else {
        let relative = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        files.insert(relative, json!(fs::read_to_string(path).unwrap()));
      }
    }
  }
  let mut files = serde_json::Map::new();
  collect(root, root, &mut files);
  Value::Object(files)
}

fn rust_move_case(root: &Path, case: &Value) -> Value {
  let case_root = root.join(case["name"].as_str().unwrap());
  let src_root = case_root.join(case["src"].as_str().unwrap());
  let dst_root = case_root.join(case["dst"].as_str().unwrap());
  let session_id = case["id"].as_str().unwrap();
  let src = src_root.join("projects/work").join(format!("{session_id}.jsonl"));
  let dst = dst_root.join("projects/work").join(format!("{session_id}.jsonl"));
  fs::create_dir_all(src.parent().unwrap()).unwrap();
  if let Some(content) = case["src_content"].as_str() {
    fs::write(&src, content).unwrap();
    if let Some(mtime) = case["src_mtime"].as_i64() {
      fs::File::open(&src)
        .unwrap()
        .set_times(
          fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(mtime as u64)),
        )
        .unwrap();
    }
    if let Some(content) = case["src_sidecar"].as_str() {
      let sidecar = src.with_extension("").join("tool-results/result.txt");
      fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
      fs::write(sidecar, content).unwrap();
    }
  }
  if let Some(content) = case["dst_content"].as_str() {
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    fs::write(&dst, content).unwrap();
    if let Some(mtime) = case["dst_mtime"].as_i64() {
      fs::File::open(&dst)
        .unwrap()
        .set_times(
          fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(mtime as u64)),
        )
        .unwrap();
    }
    if let Some(content) = case["dst_only"].as_str() {
      let sidecar = dst.with_extension("").join("tool-results/retained.txt");
      fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
      fs::write(sidecar, content).unwrap();
    }
  }
  let error = claude_accounts::move_transcript(session_id, &src_root, &dst_root)
    .err()
    .map(|error| error.to_string().replace(case_root.to_str().unwrap(), "<root>"));
  json!({"error":error, "files":snapshot(&case_root)})
}

#[test]
fn transcript_move_matrix_matches_python_in_both_directions() {
  let temporary = tempfile::tempdir().unwrap();
  let (mut data, _) = account_data(temporary.path());
  data["root"] = json!(temporary.path().join("python-moves"));
  data["cases"] = move_matrix();
  let expected = python_reference("moves", &data, &temporary);
  let actual = data["cases"]
    .as_array()
    .unwrap()
    .iter()
    .map(|case| rust_move_case(temporary.path().join("rust-moves").as_path(), case))
    .collect::<Vec<_>>();
  assert_eq!(Value::Array(actual), expected, "transcript move matrix differs from the Python baseline");
}

fn relay_matrix() -> Value {
  let tool_result = json!({"type":"user", "message":{"content":[{"type":"tool_result", "content":"done"}]}});
  let rejected = json!({"type":"rate_limit_event", "rate_limit_info":{"status":"rejected", "resetsAt":FUTURE_RESET}});
  let warning = json!({"type":"rate_limit_event", "rate_limit_info":{"status":"allowed_warning", "resetsAt":FUTURE_RESET, "unifiedWindows":{"five_hour":{"utilization":0.95}}}});
  let reading =
    |label, utilization| json!({"label":label, "info":{"unifiedWindows":{"five_hour":{"utilization":utilization}}}});
  let auth = json!({"type":"assistant", "message":{"content":[{"type":"text", "text":"Failed to authenticate"}]}});
  json!([
    {"now":NOW,"events":[rejected],"readings":[],"auth_failures":[],"exit_code":1,"stderr":"","session_id":"relay-0"},
    {"now":NOW,"events":[warning,tool_result],"readings":[reading("b",0.2)],"auth_failures":[],"exit_code":0,"stderr":"","session_id":"relay-1"},
    {"now":NOW,"events":[warning,tool_result],"readings":[reading("b",0.95)],"auth_failures":[],"exit_code":0,"stderr":"","session_id":"relay-2"},
    {"now":NOW,"events":[auth],"readings":[],"auth_failures":[],"exit_code":1,"stderr":"","session_id":null},
    {"now":NOW,"events":[auth],"readings":[],"auth_failures":[],"exit_code":0,"stderr":"","session_id":"relay-4"},
    {"now":NOW,"events":[{"type":"rate_limit_event"}],"readings":[],"auth_failures":[],"exit_code":1,"stderr":"ordinary failure","session_id":"relay-5"},
    {"now":NOW,"events":[{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","unifiedWindows":{"five_hour":{"utilization":0.89}}}},tool_result],"readings":[],"auth_failures":[],"exit_code":0,"stderr":"","session_id":"relay-6"},
    {"now":NOW,"events":[rejected,{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","unifiedWindows":{"five_hour":{"utilization":0.95}}}}],"readings":[],"auth_failures":[],"exit_code":1,"stderr":"","session_id":"relay-7"},
    {"now":NOW,"events":[rejected],"readings":[],"auth_failures":["b"],"exit_code":1,"stderr":"","session_id":"relay-8"},
  ])
}

#[test]
fn relay_watch_matrix_matches_python_decisions_and_moves() {
  let temporary = tempfile::tempdir().unwrap();
  let (mut data, directories) = account_data(temporary.path());
  data["root"] = json!(temporary.path());
  data["cases"] = relay_matrix();
  let expected = python_reference("relay", &data, &temporary);
  let cfg = Arc::new(rust_config(&data));
  let mut actual = Vec::new();
  for case in data["cases"].as_array().unwrap() {
    let now = timestamp(case["now"].as_str().unwrap());
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    rust_observe_readings(&state, case, now);
    let decision = Arc::new(Mutex::new(None));
    let mut watch = claude_relay::RelayWatch::new(
      "a".to_string(),
      Some("claude-fable-5-1".to_string()),
      Arc::clone(&cfg),
      Arc::clone(&state),
      Some("alpha".to_string()),
      Arc::clone(&decision),
    );
    let stop = case["events"].as_array().unwrap().iter().map(|event| watch.observe(event)).collect::<Vec<_>>();
    let wants_next = watch.wants_next(case["exit_code"].as_i64().unwrap() as i32, case["stderr"].as_str().unwrap());
    let moved = if wants_next {
      let session_id = case["session_id"].as_str();
      if let Some(session_id) = session_id {
        let source = directories[0].join("projects/work").join(format!("{session_id}.jsonl"));
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "synthetic transcript\n").unwrap();
      }
      let current = claude_accounts::account_by_label(&cfg, Some("a")).unwrap().unwrap();
      let outcome = claude_relay::relay_move(
        &state,
        &cfg,
        Some("claude-fable-5-1"),
        &current,
        session_id,
        Some(now),
        Some("alpha"),
      )
      .unwrap();
      json!({
        "account":outcome.account.map(|account| account.label),
        "error":outcome.error,
        "refused_holder":outcome.refused_holder.map(|account| account.label),
        "pool_exhausted":outcome.pool_exhausted,
      })
    } else {
      Value::Null
    };
    actual.push(json!({"stop":stop,"decision":decision.lock().unwrap().clone(),"move":moved}));
  }
  assert_eq!(Value::Array(actual), expected, "relay watch matrix differs from the Python baseline");
}
