//! The Claude Code usage source checked against its Python reference: the transcript
//! read, the usage-panel transform, and the cold-storage sweep. Each side runs on synthetic inputs under its own
//! temporary root, and the two results must be equal.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use charliebot_claude_code::claude_accounts::ClaudeAccountState;
use charliebot_claude_code::usage_logs::ClaudeLogs;
use charliebot_claude_code::usage_quota::{self, QuotaHttp, QuotaState};
use charliebot_claude_code::{ClaudeHost, usage_sweep};
use charliebot_compat::http_client::{self, Settings};
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::Registry;
use charliebot_registry::usage::{RecordKind, SessionFacts, SourceSweep, SweepScope, UsageRecord, UsageSourceImpl};
use charliebot_store::metadata::UtcDateTime;
use serde_json::{Map, Value, json};
use tempfile::TempDir;

const GOLDEN_FIXTURE: &str = include_str!("fixtures/claude_usage_golden.json");

const ACCOUNT: &str = "work (default)";

const ASSISTANT_A: &str = r#"{"type":"assistant","uuid":"u1","timestamp":"2026-10-01T00:00:01Z","message":{"id":"msg_a","model":"claude-fable-5-1","usage":{"input_tokens":10,"cache_creation_input_tokens":5,"cache_read_input_tokens":100,"output_tokens":7}}}"#;
const ASSISTANT_F: &str = r#"{"uuid":"u9","timestamp":"2026-10-01T00:00:09Z","message":{"id":"msg_f","model":"claude-fable-5-1","usage":{"input_tokens":6,"output_tokens":2}}}"#;
const USER_LINE: &str = r#"{"type":"user","uuid":"u2","message":{"role":"user","content":"usage of the tool"}}"#;
const SYNTHETIC: &str = r#"{"uuid":"u3","timestamp":"2026-10-01T00:00:02Z","message":{"id":"msg_b","model":"<synthetic>","usage":{"input_tokens":1}}}"#;
const NO_MODEL: &str = r#"{"uuid":"u4","message":{"id":"msg_c","usage":{"input_tokens":3}}}"#;
const NOT_JSON: &str = "not json, usage here";
const REQUEST_ONLY: &str = r#"{"requestId":"req_d","uuid":"u5","timestamp":"2026-10-01T00:00:03Z","message":{"model":"claude-fable-5-1","usage":{"input_tokens":2,"output_tokens":1}}}"#;
const UUID_ONLY: &str = r#"{"uuid":"u6","message":{"model":"claude-fable-5-1","usage":{"output_tokens":4}}}"#;
const NO_KEY: &str = r#"{"message":{"model":"claude-fable-5-1","usage":{"output_tokens":9}}}"#;
const NON_OBJECT_MESSAGE: &str = r#"{"uuid":"u7","message":"usage"}"#;
const NON_OBJECT_USAGE: &str = r#"{"uuid":"u8","message":{"id":"msg_g","model":"claude-fable-5-1","usage":"usage"}}"#;
/// A line cut off before its newline: the scan skips it in both implementations.
const TRAILING_FRAGMENT: &str =
  r#"{"uuid":"u10","message":{"id":"msg_h","model":"claude-fable-5-1","usage":{"output_tokens":2}"#;

const SID_COLD: &str = "11111111-2222-4333-8444-555555555555";
const SID_LIVE: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const SID_NO_METADATA: &str = "99999999-8888-4777-8666-555555555555";
const SID_ORPHAN: &str = "22222222-3333-4444-8555-666666666666";
const SID_FRESH: &str = "33333333-4444-4555-8666-777777777777";
const TID: &str = "f0e1d2c3-a4b5-4c6d-8e7f-0a1b2c3d4e5f";

fn golden_reference(mode: &str, data: &Value) -> Value {
  let fixture: Value = serde_json::from_str(GOLDEN_FIXTURE).unwrap();
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  let mut expected = fixture["expected_by_mode"][mode].clone();
  if mode == "read" {
    for (index, case) in data["cases"].as_array().unwrap().iter().enumerate() {
      let path = Path::new(case["path"].as_str().unwrap());
      let metadata = fs::metadata(path).unwrap();
      let modified_ns = i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec());
      expected["cases"][index]["signature"] = json!(format!("{modified_ns}:{}", metadata.len()));
    }
  }
  if mode == "transform" {
    for case in expected["cases"].as_array_mut().unwrap() {
      case["fetched_at"] = json!("2026-01-01T00:00:00+00:00");
    }
  }
  expected
}

fn quota_http() -> QuotaHttp {
  let client = http_client::client(&Settings::default()).unwrap();
  QuotaHttp::with_endpoints(client, usage_quota::USAGE_URL, usage_quota::TOKEN_REFRESH_URL)
}

fn record_json(record: &UsageRecord) -> Value {
  json!({
    "record_id": record.record_id,
    "kind": match record.kind {
      RecordKind::Native => "native",
      RecordKind::Fallback => "fallback",
    },
    "source": record.source,
    "model": record.model,
    "account": record.account,
    "ts": record.ts,
    "in_fresh": record.in_fresh,
    "cache_write": record.cache_write,
    "cache_read": record.cache_read,
    "output": record.output,
    "in_unsplit": record.in_unsplit,
    "sessions": record.sessions,
  })
}

fn transcript_text(lines: &[&str]) -> String {
  lines.iter().map(|line| format!("{line}\n")).collect()
}

#[test]
fn read_matches_the_usage_log_on_synthetic_transcripts() {
  let root = TempDir::new().unwrap();
  let projects = root.path().join("claude/projects/-synthetic-project");
  fs::create_dir_all(&projects).unwrap();

  let plain = projects.join("plain.jsonl");
  let mut plain_text = transcript_text(&[
    ASSISTANT_A,
    USER_LINE,
    SYNTHETIC,
    NO_MODEL,
    NOT_JSON,
    REQUEST_ONLY,
    UUID_ONLY,
    NO_KEY,
    ASSISTANT_A,
    NON_OBJECT_MESSAGE,
    NON_OBJECT_USAGE,
  ]);
  plain_text.push_str(TRAILING_FRAGMENT);
  fs::write(&plain, plain_text).unwrap();

  let empty = projects.join("empty.jsonl");
  fs::write(&empty, "").unwrap();

  // A resumed transcript: the first read's signature is the stored one, and the append after it moves the signature.
  let resumed = projects.join("resumed.jsonl");
  fs::write(&resumed, transcript_text(&[ASSISTANT_A, REQUEST_ONLY])).unwrap();
  let source = ClaudeLogs::new(
    ClaudeHost { home: root.path().to_path_buf(), claude_config_dir: root.path().join("claude") },
    Arc::new(ClaudeAccountState::default()),
    Arc::new(quota_http()),
  );
  let (stored, _) = source.read(&resumed, ACCOUNT, None).unwrap();
  fs::write(&resumed, transcript_text(&[ASSISTANT_A, REQUEST_ONLY, ASSISTANT_A, ASSISTANT_F])).unwrap();

  let cases = [(&plain, None), (&empty, None), (&resumed, Some(stored.clone()))];
  let mut rust_cases = Vec::new();
  for (path, previous) in &cases {
    let (signature, records) = source.read(path, ACCOUNT, previous.as_deref()).unwrap();
    rust_cases.push(json!({"signature": signature, "records": records.iter().map(record_json).collect::<Vec<_>>()}));
  }

  let python = golden_reference(
    "read",
    &json!({
      "root": root.path(),
      "cases": cases
        .iter()
        .map(|(path, previous)| json!({"path": path, "account": ACCOUNT, "previous": previous}))
        .collect::<Vec<_>>(),
    }),
  );

  assert_ne!(rust_cases[2]["signature"], json!(stored), "the resumed append must move the signature");
  assert_eq!(rust_cases.len(), python["cases"].as_array().unwrap().len());
  for (index, rust_case) in rust_cases.iter().enumerate() {
    assert_eq!(rust_case, &python["cases"][index], "transcript case {index} differs from the Python read");
  }
  let records: usize = rust_cases.iter().map(|case| case["records"].as_array().unwrap().len()).sum();
  eprintln!("claude usage read matrix: {} transcripts, {records} records", cases.len());
}

#[test]
fn transform_matches_the_usage_panel_on_synthetic_responses() {
  let cases = [
    json!({"fiveHour": {"utilization": 12.5, "resetsAt": "2026-10-18T15:00:00Z"},
           "sevenDay": {"utilization": 40, "resetsAt": "2026-10-20T00:00:00Z"}}),
    json!({"five_hour": {"utilization": 3, "resets_at": "2026-10-18T14:00:00Z"}, "seven_day": {"utilization": 0.5}}),
    json!({}),
    json!({"fiveHour": {"utilization": 9, "resetsAt": null}}),
    json!({"fiveHour": {"utilization": 1},
    "limits": [
      {"group": "session", "scope": {"model": {"display_name": "Opus"}}, "percent": 30,
       "resets_at": "2026-10-18T15:00:00Z"},
      {"group": "weekly", "scope": {"model": {"display_name": "Sonnet"}}, "percent": 5,
       "resets_at": "2026-10-20T00:00:00Z"},
      {"group": "session", "scope": null, "percent": 1},
      {"group": "session", "scope": {"model": {"display_name": "Haiku"}}, "percent": null},
      {"group": "monthly", "scope": {"model": {"display_name": "Opus"}}, "percent": 2},
      {"group": "weekly", "scope": {"model": {}}, "percent": 4},
      "not-an-object",
      {"kind": "x", "group": "session", "scope": {"model": {"display_name": "Opus"}}, "percent": 7}
    ]}),
    json!({"limits": "session"}),
    json!({"limits": null, "fiveHour": {"utilization": 1}}),
    json!({"fiveHour": {"utilization": 1}, "monthly": {"utilization": 50, "resetsAt": "2026-10-18T15:00:00Z"}}),
  ];

  let quota = QuotaState::default();
  let rust_cases: Vec<Value> = cases
    .iter()
    .map(|raw| {
      let mut panel = usage_quota::transform_response(raw, ACCOUNT, &quota).unwrap();
      assert!(panel.remove("fetched_at").is_some_and(|stamp| stamp.is_string()), "the panel carries fetched_at");
      Value::Object(panel)
    })
    .collect();

  let python_root = TempDir::new().unwrap();
  let python = golden_reference(
    "transform",
    &json!({
      "root": python_root.path(),
      "cases": cases.iter().map(|raw| json!({"raw": raw, "account": ACCOUNT})).collect::<Vec<_>>(),
    }),
  );
  let python_cases: Vec<Value> = python["cases"]
    .as_array()
    .unwrap()
    .iter()
    .map(|panel| {
      let mut panel = panel.clone();
      assert!(panel.as_object_mut().unwrap().remove("fetched_at").is_some_and(|stamp| stamp.is_string()));
      panel
    })
    .collect();

  assert_eq!(rust_cases.len(), python_cases.len());
  for (index, rust_case) in rust_cases.iter().enumerate() {
    assert_eq!(rust_case, &python_cases[index], "panel case {index} differs from the Python transform");
  }
  eprintln!("claude usage transform matrix: {} responses", cases.len());
}

const SWEEP_NOW: &str = "2026-10-18T12:00:00Z";

/// One synthetic tree, as the reference builds it: every transcript directory by name, with its sessions, worktrees
/// and trashed worktrees. Each transcript carries `age_days` of idleness.
fn sweep_spec() -> Value {
  json!({
    "now": SWEEP_NOW,
    "orphan_idle_days": 14,
    "sessions": [
      {"id": SID_COLD, "cold": true},
      {"id": SID_LIVE, "cold": false},
    ],
    "session_dirs": [SID_COLD, SID_LIVE, SID_NO_METADATA],
    "worktrees": ["wt-live"],
    "trashed": ["wt-trashed"],
    "transcripts": [
      {"name": "cold", "tree": "default", "cwd": format!("home/sessions/{SID_COLD}"), "age_days": 1},
      {"name": "cold-thread", "tree": "config", "cwd": format!("home/sessions/{SID_COLD}/threads/{TID}"), "age_days": 1},
      {"name": "live", "tree": "default", "cwd": format!("home/sessions/{SID_LIVE}"), "age_days": 30},
      {"name": "orphan-idle", "tree": "config", "cwd": format!("home/sessions/{SID_ORPHAN}"), "age_days": 30},
      {"name": "orphan-fresh", "tree": "default", "cwd": format!("home/sessions/{SID_FRESH}"), "age_days": 1},
      {"name": "no-metadata", "tree": "default", "cwd": format!("home/sessions/{SID_NO_METADATA}"), "age_days": 30},
      {"name": "worktree-live", "tree": "default", "cwd": "worktrees/wt-live", "age_days": 30},
      {"name": "worktree-gone", "tree": "config", "cwd": "worktrees/wt-gone", "age_days": 30},
      {"name": "worktree-gone-fresh", "tree": "config", "cwd": "worktrees/wt-fresh", "age_days": 1},
      {"name": "trash", "tree": "default", "cwd": "worktrees/.trash/wt-trashed", "age_days": 30},
      {"name": "user-cwd", "tree": "default", "cwd": "user/dev", "age_days": 30},
      {"name": "user-cwd-sessions-suffix", "tree": "config", "cwd": "user/sessions-x", "age_days": 30},
    ],
  })
}

/// Builds the spec's tree under `root`: returns each transcript directory by entry name.
fn build_sweep_tree(root: &Path, spec: &Value) -> BTreeMap<String, PathBuf> {
  let now = UtcDateTime::parse(spec["now"].as_str().unwrap()).unwrap().into_inner().unix_timestamp();
  for sid in spec["session_dirs"].as_array().unwrap() {
    fs::create_dir_all(root.join("home/sessions").join(sid.as_str().unwrap())).unwrap();
  }
  for name in spec["worktrees"].as_array().unwrap() {
    fs::create_dir_all(root.join("worktrees").join(name.as_str().unwrap())).unwrap();
  }
  for name in spec["trashed"].as_array().unwrap() {
    fs::create_dir_all(root.join("worktrees/.trash").join(name.as_str().unwrap())).unwrap();
  }
  let mut directories = BTreeMap::new();
  for entry in spec["transcripts"].as_array().unwrap() {
    let projects = match entry["tree"].as_str().unwrap() {
      "default" => root.join(".claude/projects"),
      "config" => root.join("claude-home/projects"),
      other => panic!("unknown transcript tree {other:?}"),
    };
    let cwd = root.join(entry["cwd"].as_str().unwrap());
    let directory = projects.join(usage_sweep::claude_project_dir_name(&cwd));
    let stamp = UNIX_EPOCH + Duration::from_secs((now - entry["age_days"].as_i64().unwrap() * 86400) as u64);
    for (relative, body) in [("transcript.jsonl", "claude-transcript"), ("subagents/agent.jsonl", "sub!")] {
      let path = directory.join(relative);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(&path, body).unwrap();
      fs::File::options().write(true).open(&path).unwrap().set_modified(stamp).unwrap();
    }
    directories.insert(entry["name"].as_str().unwrap().to_string(), directory);
  }
  directories
}

fn tree_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
  let mut tree = BTreeMap::new();
  let mut stack = vec![root.to_path_buf()];
  while let Some(dir) = stack.pop() {
    for entry in fs::read_dir(&dir).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        stack.push(path);
      } else {
        tree.insert(path.strip_prefix(root).unwrap().to_path_buf(), fs::read(&path).unwrap());
      }
    }
  }
  tree
}

fn load_config(root: &Path) -> CharlieBotConfig {
  let home = root.join("home");
  fs::create_dir_all(&home).unwrap();
  fs::write(home.join("config.yaml"), format!("paths:\n  worktree_dir: '{}'\n", root.join("worktrees").display()))
    .unwrap();
  let mut registry = Registry::new();
  registry.register_package("src.backends.claude_code", charliebot_claude_code::register).unwrap();
  registry.load_config(&home).unwrap()
}

fn sweep_scope(cfg: &Arc<CharlieBotConfig>, spec: &Value, dry_run: bool) -> SweepScope {
  let facts = spec["sessions"]
    .as_array()
    .unwrap()
    .iter()
    .map(|session| {
      let id = session["id"].as_str().unwrap().to_string();
      let facts = SessionFacts { id: id.clone(), cold: session["cold"].as_bool().unwrap(), cc_session_id: None };
      (id, facts)
    })
    .collect::<HashMap<_, _>>();
  SweepScope {
    cfg: Arc::clone(cfg),
    now: UtcDateTime::parse(spec["now"].as_str().unwrap()).unwrap(),
    dry_run,
    session_id: None,
    facts,
    references: HashMap::new(),
    orphan_idle_days: spec["orphan_idle_days"].as_i64().unwrap(),
    vacuum: false,
    force: false,
  }
}

fn sweep_summary(sweep: &SourceSweep, directories: &BTreeMap<String, PathBuf>) -> Value {
  json!({
    "categories": sweep
      .categories
      .iter()
      .map(|category| json!({"name": category.name, "unit": category.unit, "count": category.count, "bytes": category.bytes}))
      .collect::<Vec<_>>(),
    "exists": directories.iter().map(|(name, directory)| (name.clone(), json!(directory.is_dir()))).collect::<Map<_, _>>(),
  })
}

#[test]
fn sweep_matches_the_sweep_dry_and_real_on_the_same_spec() {
  let spec = sweep_spec();
  let rust_root = TempDir::new().unwrap();
  let host =
    ClaudeHost { home: rust_root.path().to_path_buf(), claude_config_dir: rust_root.path().join("claude-home") };
  let directories = build_sweep_tree(rust_root.path(), &spec);
  let cfg = Arc::new(load_config(rust_root.path()));

  let before = tree_bytes(rust_root.path());
  let dry = usage_sweep::sweep(&host, &sweep_scope(&cfg, &spec, true)).unwrap();
  let dry_summary = sweep_summary(&dry, &directories);
  let dry_untouched = tree_bytes(rust_root.path()) == before;
  let real = usage_sweep::sweep(&host, &sweep_scope(&cfg, &spec, false)).unwrap();
  let rust = json!({"dry_untouched": dry_untouched, "dry": dry_summary, "real": sweep_summary(&real, &directories)});

  let python_root = TempDir::new().unwrap();
  let python = golden_reference("sweep", &json!({"root": python_root.path(), "spec": spec}));

  assert_eq!(rust, python);
  assert_eq!(rust["dry_untouched"], json!(true));
  let deleted = rust["real"]["categories"][0]["count"].as_u64().unwrap();
  eprintln!(
    "claude usage sweep matrix: {} transcript directories, {deleted} deleted in the real run",
    spec["transcripts"].as_array().unwrap().len()
  );
}
