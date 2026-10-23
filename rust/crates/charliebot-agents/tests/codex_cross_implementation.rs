//! Cross-implementation checks of the codex package. `codex_python_reference.py` runs every Python section in one
//! `uv run python` process over the synthetic inputs built here. The Rust port runs the same inputs, and the test fails
//! with every section that differs.
//!
//! Sections: the argv/env/stdin/instructions matrix, built through the registry factory (96 cases); the environment
//! cases (4); one stream of translated events that reaches every item handler; the usage records of one synthetic
//! rollout; the resolver over synthetic rollouts and sessions (9 cases); the quota fetch over three synthetic homes
//! (3); and the cost of a table of usages (45 cases).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use charliebot_agents::backends::codex::CodexHost;
use charliebot_agents::backends::codex::factory::CodexFactory;
use charliebot_agents::backends::codex::pricing;
use charliebot_agents::backends::codex::resolver::{CodexSessionHost, CodexUsageResolver};
use charliebot_agents::backends::codex::usage_logs::CodexLogs;
use charliebot_agents::backends::codex::usage_quota::CodexQuotaAccount;
use charliebot_core::config::{
  BackendOption, BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_registry::backend::{AgentBackend, BackendFactory, Env, LaunchArgs, UsageResolver};
use charliebot_registry::usage::{PANEL_FETCHED_AT, QuotaAccount, RecordKind, UsageRecord, UsageSourceImpl};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use serde_json::{Map, Value, json};
use serde_yaml::Mapping;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

const PROMPT: &str = "Review \"the\" synthetic\nprompt \\ with \u{fc}n\u{ef}code\tand a tab";
const INSTRUCTIONS: &str = "Rules: \"quoted\" ]\n[section] \\ \u{7f} \u{fc}n\u{ef}";
const WORKER_FILE: &str = "/synthetic/instructions/worker.md";
const TRANSLATE_MODEL: &str = "gpt-5.5";
const ACCOUNT: &str = "work (default)";
const READ_ROOT: &str = "eeeeeeee-1111-4222-8333-888888888888";
const READ_THREAD: &str = "dddddddd-1111-4222-8333-777777777777";
const THREAD_A: &str = "aaaaaaaa-1111-4222-8333-444444444444";
const THREAD_B: &str = "bbbbbbbb-1111-4222-8333-555555555555";
const THREAD_C: &str = "cccccccc-1111-4222-8333-666666666666";
const QUOTA_THREAD: &str = "ffffffff-1111-4222-8333-999999999999";
const IDLE_QUOTA_THREAD: &str = "a1a1a1a1-1111-4222-8333-000000000000";

/// The synthetic homes and files of one run, written under one temporary root.
struct Fixture {
  home: PathBuf,
  bin: PathBuf,
  chat_root: PathBuf,
  read_path: PathBuf,
  quota_homes: Vec<(&'static str, PathBuf)>,
  now: OffsetDateTime,
}

impl Fixture {
  fn codex_home(&self) -> PathBuf {
    self.home.join(".codex")
  }

  fn host(&self) -> CodexHost {
    CodexHost { path: path_text(&self.bin), home: self.home.clone() }
  }

  fn env_base(&self) -> Value {
    json!({"HOME": path_text(&self.home), "PATH": "/usr/bin:/bin"})
  }
}

/// The `SessionHost` the resolver reads: fixed compaction limits and the chat-events files under one root.
struct FixedHost {
  limits: BTreeMap<String, i64>,
  chat_root: PathBuf,
}

impl CodexSessionHost for FixedHost {
  fn auto_compact_limit(&self, backend_id: &str) -> anyhow::Result<Option<i64>> {
    Ok(self.limits.get(backend_id).copied())
  }

  fn chat_events_path(&self, session_id: &str) -> PathBuf {
    self.chat_root.join(session_id).join("chat_events.jsonl")
  }
}

#[test]
fn codex_matches_the_python_package_on_synthetic_inputs() {
  let root = tempfile::tempdir().unwrap();
  let now = OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp()).unwrap();
  let fx = write_fixtures(root.path(), now);
  let request = request(&fx);
  let python = run_reference(&request);

  let argv = request["argv_cases"].as_array().unwrap();
  let env = request["env_cases"].as_array().unwrap();
  let translate = request["translate_events"].as_array().unwrap();
  let resolve = request["resolve_cases"].as_array().unwrap();
  let quota = request["quota_cases"].as_array().unwrap();
  let cost = request["cost_cases"].as_array().unwrap();
  assert_eq!(argv.len(), 96);
  assert_eq!(env.len(), 4);
  assert_eq!(resolve.len(), 9);
  assert_eq!(quota.len(), 3);
  assert_eq!(cost.len(), 45);

  let mut failures = Vec::new();
  check_argv(&fx, argv, &python, &mut failures);
  check_env(&fx, env, &python, &mut failures);
  check_translate(&fx, translate, &python, &mut failures);
  check_read(&fx, &python, &mut failures);
  check_resolve(&fx, resolve, &python, &mut failures);
  check_quota(&fx, quota, &python, &mut failures);
  check_cost(cost, &python, &mut failures);
  assert!(failures.is_empty(), "codex sections differ from the Python package:\n{}", failures.join("\n"));
}

fn main_module() -> CharlieBotConfig {
  CharlieBotConfig {
    charliebot_home: PathBuf::from("/synthetic/home"),
    headless_chrome_bin: String::new(),
    server: ServerConfig {
      host: "127.0.0.1".to_string(),
      port: 18499,
      subprocess_buffer_limit_mb: 1024,
      session_memory_max_mb: 0,
      session_swap_max_mb: 0,
      session_tasks_max: 4096,
      min_free_disk_gib: 10,
    },
    paths: PathsConfig {
      workspace_dirs: Vec::new(),
      worktree_dir: "/synthetic/worktrees".to_string(),
      scratch_dir: "/synthetic/scripts".to_string(),
    },
    backends: BackendsConfig { pool: Vec::new(), routing: None, options: Vec::new() },
    ui: UiConfig { home_services: Vec::new() },
    telegram: TelegramConfig { chat_id: None },
    sections: Sections::default(),
  }
}

fn request(fx: &Fixture) -> Value {
  json!({
    "home": path_text(&fx.home),
    "path": path_text(&fx.bin),
    "env_base": fx.env_base(),
    "argv_cases": argv_cases(),
    "env_cases": env_cases(fx),
    "translate_model": TRANSLATE_MODEL,
    "translate_events": translate_stream(),
    "read_path": path_text(&fx.read_path),
    "compact_limits": json!({"codex-main": 150_000}),
    "chat_root": path_text(&fx.chat_root),
    "resolve_cases": resolve_cases(),
    "quota_cases": quota_cases(fx),
    "quota_now_unix": fx.now.unix_timestamp(),
    "cost_cases": cost_cases(),
  })
}

/// The argv/env/stdin/instructions matrix: effort x compaction limit x model x resume x instructions x extra flags.
fn argv_cases() -> Vec<Value> {
  let extras: [Vec<&str>; 2] = [vec![], vec!["--synthetic-flag", "value with spaces"]];
  let instructions: [(Option<&str>, Option<&str>); 3] =
    [(None, None), (None, Some(INSTRUCTIONS)), (Some(WORKER_FILE), Some(INSTRUCTIONS))];
  let mut cases = Vec::new();
  for effort in [None, Some("ultra")] {
    for compact in [None, Some(150_000_i64)] {
      for model in ["gpt-5.5", "gpt-5.4-mini"] {
        for resume in [None, Some("synthetic-thread-0001")] {
          for (file, content) in instructions {
            for extra in &extras {
              let mut type_fields = Map::new();
              if let Some(effort) = effort {
                type_fields.insert("model_reasoning_effort".to_string(), json!(effort));
              }
              if let Some(limit) = compact {
                type_fields.insert("model_auto_compact_token_limit".to_string(), json!(limit));
              }
              cases.push(json!({
                "id": format!("argv-{:03}", cases.len()),
                "model": model,
                "type_fields": type_fields,
                "extra_flags": extra,
                "resume_session_id": resume,
                "instructions_file": file,
                "instructions_content": content,
                "prompt": PROMPT,
              }));
            }
          }
        }
      }
    }
  }
  cases
}

fn env_cases(fx: &Fixture) -> Vec<Value> {
  let home = path_text(&fx.home);
  let user_bin = format!("{home}/.local/bin");
  vec![
    json!({"id": "path_without_user_bin", "env": {"HOME": home, "PATH": "/usr/bin:/bin"}}),
    json!({"id": "path_with_user_bin", "env": {"HOME": home, "PATH": format!("/usr/bin:{user_bin}:/bin")}}),
    json!({"id": "path_unset", "env": {"HOME": home}}),
    json!({"id": "path_user_bin_is_a_prefix_only",
      "env": {"HOME": home, "PATH": format!("{user_bin}-extra:/usr/bin")}}),
  ]
}

/// One stream that reaches every item handler, the turn and error events, and an unknown event type.
fn translate_stream() -> Vec<Value> {
  vec![
    json!({"type": "thread.started", "thread_id": "synthetic-thread-0001"}),
    json!({"type": "turn.started"}),
    json!({"type": "item.started", "item": {"id": "msg-1", "type": "agent_message", "text": "Hello"}}),
    json!({"type": "item.updated", "item": {"id": "msg-1", "type": "agent_message", "text": "Hello, world"}}),
    json!({"type": "item.completed", "item": {"id": "msg-1", "type": "agent_message", "text": "Hello, world!"}}),
    json!({"type": "item.completed", "item": {"id": "msg-2", "type": "agent_message", "content": [
      {"type": "text", "text": "From "}, {"type": "image"}, {"type": "text", "text": "content"}]}}),
    json!({"type": "item.updated", "item": {"id": "msg-2", "type": "agent_message", "content": [
      {"type": "text", "text": "From content"}]}}),
    json!({"type": "item.updated", "item": {"id": "rsn-1", "type": "reasoning", "text": "Think"}}),
    json!({"type": "item.updated", "item": {"id": "rsn-1", "type": "reasoning", "text": "Thinking"}}),
    json!({"type": "item.completed", "item": {"id": "rsn-2", "type": "reasoning", "summary": [
      {"type": "summary_text", "text": "Sum"}, {"type": "other", "text": "x"},
      {"type": "summary_text", "text": "mary"}]}}),
    json!({"type": "item.started", "item": {"id": "cmd-1", "type": "command_execution", "command": "ls -la"}}),
    json!({"type": "item.completed", "item": {
      "id": "cmd-1", "type": "command_execution", "command": "ls -la", "aggregated_output": "total 0\n"}}),
    json!({"type": "item.completed", "item": {
      "id": "cmd-2", "type": "command_execution", "command": "false", "aggregated_output": null,
      "output": "fallback"}}),
    json!({"type": "item.completed", "item": {"id": "cmd-3", "type": "command_execution", "command": "true"}}),
    json!({"type": "item.started", "item": {"id": "web-1", "type": "web_search", "query": "synthetic query"}}),
    json!({"type": "item.completed", "item": {
      "id": "web-1", "type": "web_search", "query": "synthetic query", "output": 42}}),
    json!({"type": "item.completed", "item": {"id": "fc-1", "type": "file_change", "changes": [
      {"kind": "add", "path": "/synthetic/a.txt"}, {"kind": "update", "path": "/synthetic/b.txt"},
      {"kind": "delete", "path": "/synthetic/c.txt"}]}}),
    json!({"type": "item.started", "item": {"id": "mcp-1", "type": "mcp_tool_call", "server_label": "synthetic",
      "name": "lookup", "arguments": "{\"q\": 1}"}}),
    json!({"type": "item.started", "item": {"id": "mcp-2", "type": "mcp_tool_call", "name": "raw",
      "arguments": "{not json"}}),
    json!({"type": "item.started", "item": {"id": "mcp-3", "type": "mcp_tool_call", "server_label": "s",
      "name": "obj", "arguments": {"k": ["v"]}}}),
    json!({"type": "item.completed", "item": {"id": "mcp-1", "type": "mcp_tool_call", "server_label": "synthetic",
      "name": "lookup", "result": {"ok": true}}}),
    json!({"type": "item.completed", "item": {"id": "mcp-2", "type": "mcp_tool_call", "name": "raw",
      "error": "boom"}}),
    json!({"type": "item.started", "item": {"id": "todo-1", "type": "todo_list", "items": todo_items()}}),
    json!({"type": "item.updated", "item": {"id": "todo-1", "type": "todo_list", "items": todo_items()}}),
    json!({"type": "item.completed", "item": {"id": "todo-1", "type": "todo_list", "items": []}}),
    json!({"type": "item.completed", "item": {"id": "todo-2", "type": "todo_list", "items": [
      {"text": "solo", "status": "other"}]}}),
    json!({"type": "item.completed", "item": {"id": "err-1", "type": "error", "message": "item failed"}}),
    json!({"type": "item.completed", "item": {"id": "err-2", "type": "error"}}),
    json!({"type": "item.updated", "item": {"id": "unknown-1", "type": "plan_update"}}),
    json!({"type": "turn.failed", "error": {"message": "turn exploded"}}),
    json!({"type": "error", "error": "plain string error"}),
    json!({"type": "error"}),
    json!({"type": "turn.completed", "usage": {"input_tokens": 1000, "cached_input_tokens": 400,
      "output_tokens": 20}}),
    json!({"type": "mystery.event", "x": 1}),
  ]
}

fn todo_items() -> Value {
  json!([
    {"text": "  first  ", "status": "completed"},
    {"label": "second", "status": "in_progress"},
    {"content": "third", "completed": true},
    {"step": "fourth"},
    {"text": "   "},
    "not-a-dict",
    {"text": 5, "label": "fallback"},
  ])
}

fn resolve_cases() -> Vec<Value> {
  vec![
    json!({"id": "persisted_thread", "session_id": "chat-1", "backend": "codex-main",
      "cc_session_id": THREAD_A, "events": []}),
    json!({"id": "event_thread", "session_id": "chat-2", "backend": "codex-main", "cc_session_id": null,
      "events": [{"type": "session_attached", "session_id": THREAD_A}]}),
    json!({"id": "chat_file_thread", "session_id": "chat-3", "backend": "codex-main", "cc_session_id": null,
      "events": []}),
    json!({"id": "no_thread", "session_id": "chat-4", "backend": "codex-main", "cc_session_id": null,
      "events": []}),
    json!({"id": "no_backend", "session_id": "chat-5", "backend": "", "cc_session_id": THREAD_A, "events": []}),
    json!({"id": "unlimited_backend", "session_id": "chat-6", "backend": "codex-plain",
      "cc_session_id": THREAD_A, "events": []}),
    json!({"id": "unpriced_model", "session_id": "chat-7", "backend": "codex-main", "cc_session_id": THREAD_C,
      "events": []}),
    json!({"id": "missing_rollout", "session_id": "chat-8", "backend": "codex-main", "cc_session_id": THREAD_B,
      "events": []}),
    json!({"id": "empty_persisted", "session_id": "chat-9", "backend": "codex-main", "cc_session_id": "",
      "events": [{"session_id": THREAD_A}]}),
  ]
}

fn quota_cases(fx: &Fixture) -> Vec<Value> {
  fx.quota_homes.iter().map(|(id, home)| json!({"id": id, "codex_home": path_text(home)})).collect()
}

/// Models x usage shapes: priced, unpriced and empty models against cached, zero, empty and large usages.
fn cost_cases() -> Vec<Value> {
  let models = [
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex",
    "unknown-model",
    "",
  ];
  let usages = [
    json!({"input_tokens": 1000, "cached_input_tokens": 400, "output_tokens": 20}),
    json!({"input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0}),
    json!({}),
    json!({"input_tokens": 10, "cached_input_tokens": 10, "output_tokens": 0}),
    json!({"input_tokens": 123_456_789, "cached_input_tokens": 1, "output_tokens": 987_654}),
  ];
  let mut cases = Vec::new();
  for model in models {
    for usage in &usages {
      cases.push(json!({"model": model, "usage": usage}));
    }
  }
  cases
}

fn write_fixtures(root: &Path, now: OffsetDateTime) -> Fixture {
  let home = root.join("home");
  let codex_home = home.join(".codex");
  let bin = root.join("bin");
  fs::create_dir_all(&bin).unwrap();
  symlink(env!("CARGO_BIN_EXE_fake_program"), bin.join("codex")).unwrap();
  write_text(&bin.join("probe.json"), &json!({"codex": [{"exit": 0}]}).to_string());

  let read_path = rollout_path(&codex_home, READ_THREAD, "2026-10-18T09-00-00");
  write_text(&read_path, &read_text());
  write_text(
    &rollout_path(&codex_home, THREAD_A, "2026-10-18T09-30-00"),
    &thread_text(THREAD_A, "gpt-5.5", [500, 400, 20], [510_000, 400_000, 21_000]),
  );
  write_text(
    &rollout_path(&codex_home, THREAD_C, "2026-10-18T09-30-00"),
    &thread_text(THREAD_C, "unknown-model", [10, 0, 1], [10, 0, 1]),
  );
  let chat_root = root.join("chat");
  write_text(
    &chat_root.join("chat-3").join("chat_events.jsonl"),
    &lines_text(&[
      json!({"type": "thinking", "content": "synthetic"}),
      json!({"type": "session_attached", "session_id": THREAD_A}),
    ]),
  );

  let quota_root = root.join("quota");
  let plan_home = quota_root.join("plan_and_spend").join(".codex");
  write_text(&rollout_path(&plan_home, QUOTA_THREAD, "2026-10-18T08-00-00"), &plan_text(QUOTA_THREAD, now));
  let idle_home = quota_root.join("no_plan_reading").join(".codex");
  write_text(&rollout_path(&idle_home, IDLE_QUOTA_THREAD, "2026-10-18T08-00-00"), &idle_text(IDLE_QUOTA_THREAD, now));
  // Never created: the quota account reads an absent sessions tree.
  let missing_home = quota_root.join("no_sessions").join(".codex");
  Fixture {
    home,
    bin,
    chat_root,
    read_path,
    quota_homes: vec![("plan_and_spend", plan_home), ("no_plan_reading", idle_home), ("no_sessions", missing_home)],
    now,
  }
}

/// A rollout file under one codex home, named the way codex names them: a start stamp, then the thread id.
fn rollout_path(codex_home: &Path, thread: &str, started: &str) -> PathBuf {
  codex_home.join("sessions/2026/10/18").join(format!("rollout-{started}-{thread}.jsonl"))
}

/// The read fixture: a root session, two priced totals, a duplicate, a zero restatement, a null-info event, a blank
/// line, a marker line that does not parse, and a trailing fragment with no newline.
fn read_text() -> String {
  let mut text = lines_text(&[
    session_meta("2026-10-18T09:00:00Z", READ_ROOT),
    turn_context("2026-10-18T09:00:01Z", "gpt-5.5"),
    json!({"type": "event_msg", "timestamp": "2026-10-18T09:00:02Z", "payload": {"type": "token_count", "info": null}}),
    token_count("2026-10-18T09:00:03Z", [1000, 400, 20], [1000, 400, 20], Value::Null),
    turn_context("2026-10-18T09:00:04Z", "gpt-5.6-terra"),
    token_count("2026-10-18T09:00:05Z", [5000, 0, 300], [6000, 400, 320], Value::Null),
    token_count("2026-10-18T09:00:06Z", [5000, 0, 300], [6000, 400, 320], Value::Null),
    json!({"type": "event_msg", "timestamp": "2026-10-18T09:00:07Z",
      "payload": {"type": "agent_message", "message": "hi"}}),
    token_count("2026-10-18T09:00:08Z", [0, 0, 0], [6000, 400, 320], Value::Null),
  ]);
  text.push('\n');
  text.push_str("{\"type\": \"event_msg\", \"payload\": {\"type\": \"token_count\", broken\n");
  text.push_str(concat!(
    "{\"type\": \"event_msg\", \"timestamp\": \"2026-10-18T09:00:09Z\", ",
    "\"payload\": {\"type\": \"token_count\"",
  ));
  text
}

/// A resolver rollout: its root session, the model in force, and one token count.
fn thread_text(root: &str, model: &str, last: [i64; 3], total: [i64; 3]) -> String {
  lines_text(&[
    session_meta("2026-10-18T09:30:00Z", root),
    turn_context("2026-10-18T09:30:01Z", model),
    token_count("2026-10-18T09:30:02Z", last, total, Value::Null),
  ])
}

/// The spend rows and the plan-pool reading of one rollout. The spend rows span a day, a week and a ten-day gap; a
/// model-level pool reading and a malformed row sit beside the plan reading the quota fetch must choose.
fn plan_text(thread: &str, now: OffsetDateTime) -> String {
  let at = |seconds: i64| rfc3339(now + Duration::seconds(seconds));
  let epoch = |seconds: i64| (now + Duration::seconds(seconds)).unix_timestamp();
  let plan = json!({
    "limit_id": "codex",
    "plan_type": "plus",
    "primary": {"used_percent": 42.5, "window_minutes": 300, "resets_at": epoch(-3600)},
    "secondary": {"used_percent": 10, "window_minutes": 10080, "resets_at": epoch(172_800)},
    "credits": {"unlimited": false, "balance": "12.50"},
  });
  let model_pool = json!({
    "limit_id": "gpt-5.6-terra-pool",
    "limit_name": "gpt-5.6-terra",
    "primary": {"used_percent": 5, "window_minutes": 300, "resets_at": epoch(3600)},
  });
  let malformed = json!({"type": "event_msg", "timestamp": at(-7200), "payload": {"type": "token_count",
    "rate_limits": {"limit_name": "malformed"},
    "info": {"last_token_usage": {"input_tokens": "x", "cached_input_tokens": 0, "output_tokens": 0}}}});
  lines_text(&[
    session_meta(&at(-864_000), thread),
    turn_context(&at(-864_000), "gpt-5.5"),
    token_count(&at(-864_000), [9, 9, 9], [9, 9, 9], Value::Null),
    turn_context(&at(-259_200), "gpt-5.5"),
    token_count(&at(-259_200), [1_000_000, 0, 100_000], [1_000_000, 0, 100_000], Value::Null),
    turn_context(&at(-7200), "gpt-5.5"),
    malformed,
    token_count(&at(-7200), [2000, 1000, 500], [2000, 1000, 500], plan),
    turn_context(&at(-3600), "gpt-5.6-terra"),
    token_count(&at(-1800), [3000, 0, 700], [3000, 0, 700], model_pool),
  ])
}

/// A rollout with only a model-level pool reading: the quota fetch finds no plan-quota reading.
fn idle_text(thread: &str, now: OffsetDateTime) -> String {
  let at = |seconds: i64| rfc3339(now + Duration::seconds(seconds));
  lines_text(&[
    session_meta(&at(-3600), thread),
    turn_context(&at(-3600), "gpt-5.5"),
    token_count(
      &at(-1800),
      [100, 0, 10],
      [100, 0, 10],
      json!({"limit_id": "gpt-5.5-pool", "limit_name": "gpt-5.5-pool"}),
    ),
  ])
}

fn session_meta(at: &str, session_id: &str) -> Value {
  json!({"type": "session_meta", "timestamp": at, "payload": {"session_id": session_id}})
}

fn turn_context(at: &str, model: &str) -> Value {
  json!({"type": "turn_context", "timestamp": at, "payload": {"model": model}})
}

/// A `token_count` event: the last turn's usage, the cumulative totals, and the rate-limit block (null when absent).
fn token_count(at: &str, last: [i64; 3], total: [i64; 3], rate_limits: Value) -> Value {
  json!({"type": "event_msg", "timestamp": at, "payload": {"type": "token_count", "rate_limits": rate_limits,
    "info": {"last_token_usage": usage(last), "total_token_usage": usage(total), "model_context_window": 258_400}}})
}

fn usage(counts: [i64; 3]) -> Value {
  json!({"input_tokens": counts[0], "cached_input_tokens": counts[1], "output_tokens": counts[2]})
}

fn lines_text(lines: &[Value]) -> String {
  lines.iter().map(|line| format!("{line}\n")).collect()
}

fn write_text(path: &Path, text: &str) {
  fs::create_dir_all(path.parent().unwrap()).unwrap();
  fs::write(path, text).unwrap();
}

fn run_reference(request: &Value) -> Value {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap().to_path_buf();
  let helper = "rust/crates/charliebot-agents/tests/codex_python_reference.py";
  let mut child = Command::new("uv")
    .args(["run", "python", helper])
    .current_dir(repo_root)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  child.stdin.take().unwrap().write_all(request.to_string().as_bytes()).unwrap();
  let output = child.wait_with_output().unwrap();
  assert!(
    output.status.success(),
    "Python codex reference failed:\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );
  let stdout = String::from_utf8(output.stdout).unwrap();
  let last = stdout.lines().last().expect("the Python codex reference printed nothing");
  serde_json::from_str(last).unwrap()
}

fn check_argv(fx: &Fixture, cases: &[Value], python: &Value, failures: &mut Vec<String>) {
  let expected = python["argv"].as_array().expect("the argv section");
  expect_eq(failures, "argv case count", &json!(cases.len()), &json!(expected.len()));
  for (case, python_case) in cases.iter().zip(expected) {
    let backend = build_backend(fx, &option_of(case), launch_of(case));
    let base = env_map(&fx.env_base());
    let prepared = backend.prepare_env(base.clone()).unwrap();
    let rust = json!({
      "id": case["id"],
      "argv": backend.build_command(PROMPT).unwrap(),
      "env_delta": env_delta(&prepared, &base),
      "stdin": backend.stdin_prompt(PROMPT),
      "instructions_target": backend.instructions_target().map(|(file, event)| json!([file, event])),
    });
    expect_eq(failures, &format!("argv {}", text(&case["id"])), &rust, python_case);
  }
}

fn check_env(fx: &Fixture, cases: &[Value], python: &Value, failures: &mut Vec<String>) {
  let option = option_of(&json!({"id": "env", "model": TRANSLATE_MODEL, "type_fields": {}}));
  let expected = python["env"].as_array().expect("the env section");
  expect_eq(failures, "env case count", &json!(cases.len()), &json!(expected.len()));
  for (case, python_case) in cases.iter().zip(expected) {
    let backend = build_backend(fx, &option, LaunchArgs::default());
    let env = backend.prepare_env(env_map(&case["env"])).unwrap();
    let rust = json!({"id": case["id"], "env": env});
    expect_eq(failures, &format!("env {}", text(&case["id"])), &rust, python_case);
  }
}

fn check_translate(fx: &Fixture, events: &[Value], python: &Value, failures: &mut Vec<String>) {
  let option = option_of(&json!({"id": "translate", "model": TRANSLATE_MODEL, "type_fields": {}}));
  let backend = build_backend(fx, &option, LaunchArgs::default());
  let mut translator = backend.translator();
  let rust: Vec<Vec<Value>> = events.iter().map(|event| translator.translate(event.clone()).unwrap()).collect();
  expect_eq(failures, "translate", &json!(rust), &python["translate"]);
}

fn check_read(fx: &Fixture, python: &Value, failures: &mut Vec<String>) {
  let (signature, records) = CodexLogs::new(fx.codex_home()).read(&fx.read_path, ACCOUNT, None).unwrap();
  let rust = json!({"signature": signature, "records": records.iter().map(record_json).collect::<Vec<_>>()});
  expect_eq(failures, "read", &rust, &python["read"]);
}

fn check_resolve(fx: &Fixture, cases: &[Value], python: &Value, failures: &mut Vec<String>) {
  let limits: BTreeMap<String, i64> = serde_json::from_value(json!({"codex-main": 150_000})).unwrap();
  let host: Arc<dyn CodexSessionHost> = Arc::new(FixedHost { limits, chat_root: fx.chat_root.clone() });
  let expected = python["resolve"].as_array().expect("the resolve section");
  expect_eq(failures, "resolve case count", &json!(cases.len()), &json!(expected.len()));
  for (case, python_case) in cases.iter().zip(expected) {
    let mut meta = SessionMetadata::new("synthetic-session", "synthetic", TaskProfile::Manager);
    meta.backend = text(&case["backend"]).to_string();
    meta.cc_session_id = opt_text(&case["cc_session_id"]);
    let events =
      case["events"].as_array().unwrap().iter().cloned().map(|event| Arc::new(event.into())).collect::<Vec<_>>();
    let usage = CodexUsageResolver::new(fx.codex_home(), Arc::clone(&host))
      .resolve(text(&case["session_id"]), &meta, &events)
      .unwrap();
    let rust = json!({"id": case["id"], "usage": usage.map(Value::Object)});
    expect_eq(failures, &format!("resolve {}", text(&case["id"])), &rust, python_case);
  }
}

fn check_quota(fx: &Fixture, cases: &[Value], python: &Value, failures: &mut Vec<String>) {
  let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
  let expected = python["quota"].as_array().expect("the quota section");
  expect_eq(failures, "quota case count", &json!(cases.len()), &json!(expected.len()));
  for (case, python_case) in cases.iter().zip(expected) {
    let account = CodexQuotaAccount::new(Path::new(text(&case["codex_home"])));
    let fetch = runtime.block_on(account.fetch()).expect("the rollout listing succeeds");
    let marked = fetch.as_ref().map(|entry| account.mark_expired(entry, UtcDateTime::parse(&rfc3339(fx.now)).unwrap()));
    let rust = json!({
      "id": case["id"],
      "fetch": without_fetched_at(fetch.as_ref()),
      "marked": without_fetched_at(marked.as_ref()),
      "last_error": account.last_error(),
    });
    expect_eq(failures, &format!("quota {}", text(&case["id"])), &rust, python_case);
  }
}

fn check_cost(cases: &[Value], python: &Value, failures: &mut Vec<String>) {
  let expected = python["cost"].as_array().expect("the cost section");
  expect_eq(failures, "cost case count", &json!(cases.len()), &json!(expected.len()));
  for (case, python_case) in cases.iter().zip(expected) {
    let usage = case["usage"].as_object().unwrap();
    let cost = pricing::calculate_codex_usage_cost_usd(text(&case["model"]), usage).unwrap();
    let rust = json!({"model": case["model"], "usage": case["usage"], "cost": cost});
    expect_eq(failures, &format!("cost {} {}", text(&case["model"]), case["usage"]), &rust, python_case);
  }
}

fn build_backend(fx: &Fixture, option: &BackendOption, launch: LaunchArgs) -> Box<dyn AgentBackend> {
  CodexFactory::new(fx.host()).build(Some(option), &main_module(), launch).unwrap()
}

fn option_of(case: &Value) -> BackendOption {
  BackendOption {
    id: text(&case["id"]).to_string(),
    label: "Codex".to_string(),
    model: text(&case["model"]).to_string(),
    prompt_overlay: None,
    backend_type: "codex".to_string(),
    type_fields: serde_yaml::from_value::<Mapping>(serde_yaml::to_value(&case["type_fields"]).unwrap()).unwrap(),
  }
}

fn launch_of(case: &Value) -> LaunchArgs {
  LaunchArgs {
    extra_flags: strings(&case["extra_flags"]),
    resume_session_id: opt_text(&case["resume_session_id"]),
    instructions_content: opt_text(&case["instructions_content"]),
    instructions_file: opt_text(&case["instructions_file"]).map(PathBuf::from),
    ..LaunchArgs::default()
  }
}

fn record_json(record: &UsageRecord) -> Value {
  let kind = match record.kind {
    RecordKind::Native => "native",
    RecordKind::Fallback => "fallback",
  };
  json!({
    "record_id": record.record_id,
    "kind": kind,
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

fn without_fetched_at(entry: Option<&Map<String, Value>>) -> Value {
  match entry {
    None => Value::Null,
    Some(entry) => Value::Object(
      entry.iter().filter(|(key, _)| key.as_str() != PANEL_FETCHED_AT).map(|(k, v)| (k.clone(), v.clone())).collect(),
    ),
  }
}

/// The entries of `prepared` that differ from `base` (new or changed), as the Python helper reports them.
fn env_delta(prepared: &Env, base: &Env) -> Value {
  let changed: BTreeMap<&str, &str> = prepared
    .iter()
    .filter(|(name, value)| base.get(*name) != Some(*value))
    .map(|(name, value)| (name.as_str(), value.as_str()))
    .collect();
  json!(changed)
}

fn expect_eq(failures: &mut Vec<String>, label: &str, rust: &Value, python: &Value) {
  if rust != python {
    failures.push(format!("{label}\n  rust:   {rust}\n  python: {python}"));
  }
}

fn text(value: &Value) -> &str {
  value.as_str().expect("a string")
}

fn opt_text(value: &Value) -> Option<String> {
  value.as_str().map(str::to_string)
}

fn strings(value: &Value) -> Vec<String> {
  value.as_array().unwrap().iter().map(|item| text(item).to_string()).collect()
}

fn env_map(value: &Value) -> Env {
  value.as_object().unwrap().iter().map(|(name, val)| (name.clone(), text(val).to_string())).collect()
}

fn path_text(path: &Path) -> String {
  path.to_string_lossy().into_owned()
}

fn rfc3339(at: OffsetDateTime) -> String {
  at.format(&Rfc3339).unwrap()
}
