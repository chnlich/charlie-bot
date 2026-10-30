use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::ledger::LedgerRow;
use charliebot_core::config::{self, CharlieBotConfig, ConfigSchema};
use charliebot_registry::usage::{RecordKind, UsageOps, UsageRecord, UsageSource, UsageSourceImpl};
use charliebot_registry::{Registry, RegistryError};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value, json};

const HOST: &str = "fixture-host";
const NAME: &str = "claude-model";
static EXTERNAL_READS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
  // The directory whose `.json` stems name the Codex sessions on disk, set by each fixture.
  static CODEX_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
  // Every file a CLI stand-in read, in order.
  static READS: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
  // The ledger a CLI stand-in opens on each read, to see what the running capture has committed.
  static PROBE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
  static PROBED: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// A source whose one log is the plain text file `cli/external.log`, read as a single call.
struct External;

impl UsageSourceImpl for External {
  fn ops(&self) -> UsageOps {
    UsageOps { logs: true, read: true, ..UsageOps::default() }
  }

  fn logs(&self, config: &CharlieBotConfig) -> anyhow::Result<Vec<(PathBuf, String)>> {
    let path = config.charliebot_home.join("cli/external.log");
    Ok(path.exists().then_some((path, "fixture-account".into())).into_iter().collect())
  }

  fn read(&self, path: &Path, _account: &str, _previous: Option<&str>) -> anyhow::Result<(String, Vec<UsageRecord>)> {
    EXTERNAL_READS.fetch_add(1, Ordering::SeqCst);
    let metadata = fs::metadata(path)?;
    let signature = format!("{}:{}", mtime_ns(&metadata), metadata.len());
    Ok((
      signature,
      vec![UsageRecord {
        record_id: "external-call".into(),
        kind: RecordKind::Native,
        source: "External".into(),
        model: "external-model".into(),
        account: "fixture-account".into(),
        ts: TS_A.into(),
        in_fresh: 4,
        cache_write: 0,
        cache_read: 1,
        in_unsplit: 0,
        output: 3,
        sessions: vec!["external-session".into()],
      }],
    ))
  }

  fn live_cli_sessions(&self) -> anyhow::Result<HashSet<String>> {
    bail!("fixture source does not declare live_cli_sessions");
  }
}

/// Stands in for a CLI reader: `<home>/cli/<dir>/<file>.json` holds the call records that file would yield, and a
/// record with `"fail": true` makes its file fail the capture. The Codex stand-in declares live sessions.
struct CliStub {
  name: &'static str,
  dir: &'static str,
  account: &'static str,
  codex: bool,
}

impl UsageSourceImpl for CliStub {
  fn ops(&self) -> UsageOps {
    UsageOps { logs: true, read: true, live_cli_sessions: self.codex, ..UsageOps::default() }
  }

  fn logs(&self, config: &CharlieBotConfig) -> anyhow::Result<Vec<(PathBuf, String)>> {
    let directory = config.charliebot_home.join("cli").join(self.dir);
    let entries = match fs::read_dir(&directory) {
      Ok(entries) => entries,
      Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
      Err(error) => return Err(error).with_context(|| format!("listing {}", directory.display())),
    };
    let mut paths = entries.map(|entry| entry.map(|entry| entry.path())).collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    Ok(
      paths
        .into_iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .map(|path| (path, self.account.to_string()))
        .collect(),
    )
  }

  fn read(&self, path: &Path, account: &str, _previous: Option<&str>) -> anyhow::Result<(String, Vec<UsageRecord>)> {
    READS.with(|reads| reads.borrow_mut().push(path.to_path_buf()));
    if let Some(ledger_path) = PROBE.with(|probe| probe.borrow().clone()) {
      let reader = UsageLedger::open(&ledger_path)?;
      let committed = reader.captured_sigs(HOST)?.len();
      PROBED.with(|probed| probed.borrow_mut().push(committed));
    }
    let items: Vec<Value> = serde_json::from_slice(&fs::read(path)?)?;
    if items.iter().any(|item| item["fail"] == true) {
      bail!("{} is a failing fixture", path.display());
    }
    let records = items.iter().map(|item| stub_record(item, self.name, account)).collect::<anyhow::Result<Vec<_>>>()?;
    let metadata = fs::metadata(path)?;
    Ok((format!("{}:{}", mtime_ns(&metadata), metadata.len()), records))
  }

  fn live_cli_sessions(&self) -> anyhow::Result<HashSet<String>> {
    if !self.codex {
      bail!("fixture source does not declare live_cli_sessions");
    }
    let Some(root) = CODEX_ROOT.with(|root| root.borrow().clone()) else {
      bail!("the Codex stand-in has no root for this fixture");
    };
    let entries = match fs::read_dir(&root) {
      Ok(entries) => entries,
      Err(error) if error.kind() == ErrorKind::NotFound => return Ok(HashSet::new()),
      Err(error) => return Err(error).with_context(|| format!("listing {}", root.display())),
    };
    let mut live = HashSet::new();
    for entry in entries {
      let path = entry?.path();
      if path.extension().is_some_and(|extension| extension == "json")
        && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
      {
        live.insert(stem.to_string());
      }
    }
    Ok(live)
  }
}

fn stub_record(item: &Value, source: &str, account: &str) -> anyhow::Result<UsageRecord> {
  let text =
    |key: &str| item[key].as_str().map(str::to_string).with_context(|| format!("record field {key} is not a string"));
  let count = |key: &str| item[key].as_i64().with_context(|| format!("record field {key} is not an integer"));
  let kind = match item["kind"].as_str() {
    Some("native") => RecordKind::Native,
    Some("fallback") => RecordKind::Fallback,
    _ => bail!("record kind is neither native nor fallback"),
  };
  let sessions = item["sessions"]
    .as_array()
    .context("record field sessions is not an array")?
    .iter()
    .map(|session| session.as_str().map(str::to_string).context("session is not a string"))
    .collect::<anyhow::Result<Vec<_>>>()?;
  Ok(UsageRecord {
    record_id: text("record_id")?,
    kind,
    source: source.to_string(),
    model: text("model")?,
    account: account.to_string(),
    ts: text("ts")?,
    in_fresh: count("in_fresh")?,
    cache_write: count("cache_write")?,
    cache_read: count("cache_read")?,
    in_unsplit: 0,
    output: count("output")?,
    sessions,
  })
}

fn register_sources(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.usage_source(UsageSource {
    name: "CLC",
    id_prefixes: &["charlie-code-", "clc-"],
    run_logs_only: true,
    implementation: None,
  })?;
  registry.attribute_backend_type("charlie-code", "CLC")?;
  registry.usage_source(UsageSource {
    name: "External",
    id_prefixes: &["external-"],
    run_logs_only: false,
    implementation: Some(Arc::new(External)),
  })?;
  registry.usage_source(UsageSource {
    name: "Codex",
    id_prefixes: &["codex-"],
    run_logs_only: false,
    implementation: Some(Arc::new(CliStub { name: "Codex", dir: "codex", account: "codex-main", codex: true })),
  })?;
  registry.attribute_backend_type("codex", "Codex")?;
  registry.usage_source(UsageSource {
    name: "Claude Code",
    id_prefixes: &["claude-"],
    run_logs_only: false,
    implementation: Some(Arc::new(CliStub {
      name: "Claude Code",
      dir: "claude",
      account: "claude-main",
      codex: false,
    })),
  })?;
  registry.attribute_backend_type("cc-claude", "Claude Code")?;
  registry.usage_source(UsageSource {
    name: "opencode",
    id_prefixes: &["opencode-", "oc-"],
    run_logs_only: false,
    implementation: Some(Arc::new(CliStub {
      name: "opencode",
      dir: "opencode",
      account: "opencode-main",
      codex: false,
    })),
  })?;
  registry.attribute_backend_type("opencode", "opencode")?;
  Ok(())
}

const BASE_OPTIONS: &str = r#"backends:
  options:
    - id: charlie-code-native
      label: Native
      type: charlie-code
      model: vendor/native-model
    - id: codex-main
      label: Codex
      type: codex
      model: vendor/codex-model
    - id: claude-main
      label: Claude
      type: cc-claude
      model: vendor/claude-model
"#;

fn fixture(home: &Path) -> (CharlieBotConfig, Registry) {
  fixture_with(home, "")
}

/// The fixture config with `options` appended to its `backends.options` list, each in the file's own shape.
fn fixture_with(home: &Path, options: &str) -> (CharlieBotConfig, Registry) {
  fs::create_dir_all(home).unwrap();
  fs::write(home.join("config.yaml"), format!("{BASE_OPTIONS}{options}")).unwrap();
  let schema =
    ConfigSchema { backend_types: vec!["charlie-code", "codex", "cc-claude", "opencode"], ..ConfigSchema::default() };
  let config = config::load_config(home, &schema).unwrap();
  CODEX_ROOT.with(|root| *root.borrow_mut() = Some(home.join("cli/codex")));
  READS.with(|reads| reads.borrow_mut().clear());
  PROBE.with(|probe| *probe.borrow_mut() = None);
  PROBED.with(|probed| probed.borrow_mut().clear());
  let mut registry = Registry::new();
  registry.register_package("usage-fixture", register_sources).unwrap();
  (config, registry)
}

/// One `backends.options` entry in the config file's shape.
fn option(id: &str, backend_type: &str, model: &str) -> String {
  format!("    - id: {id}\n      label: {id}\n      type: {backend_type}\n      model: {model}\n")
}

fn write_thread(
  home: &Path,
  session: &str,
  thread: &str,
  metadata: Option<(&str, Option<&str>)>,
  events: &[u8],
) -> PathBuf {
  let metadata = metadata.map(|(backend, model)| {
    let model = model.map_or(String::new(), |model| format!(", \"model\": \"{model}\""));
    format!("{{\"backend\": \"{backend}\"{model}}}")
  });
  write_thread_json(home, session, thread, metadata.as_deref(), events)
}

fn write_thread_json(home: &Path, session: &str, thread: &str, metadata: Option<&str>, events: &[u8]) -> PathBuf {
  let directory = home.join("sessions").join(session).join("threads").join(thread);
  fs::create_dir_all(directory.join("data")).unwrap();
  if let Some(metadata) = metadata {
    fs::write(directory.join("metadata.json"), metadata).unwrap();
  }
  let path = directory.join("data/events.jsonl");
  fs::write(&path, events).unwrap();
  path
}

fn write_master(home: &Path, session: &str, started: &str, events: &[u8]) -> PathBuf {
  let path = home.join("sessions").join(session).join("data/master_runs").join(started).join("agent.raw.ndjson");
  fs::create_dir_all(path.parent().unwrap()).unwrap();
  fs::write(&path, events).unwrap();
  path
}

fn write_run(
  home: &Path,
  session: &str,
  id: &str,
  backend: &str,
  model: Option<&str>,
  native_session_id: Option<&str>,
  events: &[u8],
) -> PathBuf {
  let model = model.map_or(String::new(), |model| format!(", \"model\": \"{model}\""));
  let native = native_session_id.map_or(String::new(), |id| format!(", \"native_session_id\": \"{id}\""));
  let metadata = format!("{{\"backend\": \"{backend}\", \"started_at\": \"{TS_A}\"{model}{native}}}");
  write_run_json(home, session, id, &metadata, events)
}

fn write_run_json(home: &Path, session: &str, id: &str, metadata: &str, events: &[u8]) -> PathBuf {
  let directory = home.join("sessions").join(session).join("data/runs").join(id);
  fs::create_dir_all(&directory).unwrap();
  fs::write(directory.join("metadata.json"), metadata).unwrap();
  let path = directory.join("agent.raw.ndjson");
  fs::write(&path, events).unwrap();
  path
}

fn result_line(timestamp: &str, input: i64, cache_write: i64, cache_read: i64, output: i64) -> String {
  format!(
    "{{\"type\": \"result\", \"timestamp\": \"{timestamp}\", \"usage\": {{\"input_tokens\": {input}, \"cache_creation_input_tokens\": {cache_write}, \"cache_read_input_tokens\": {cache_read}, \"output_tokens\": {output}}}}}\n"
  )
}

fn session_line(id: &str) -> String {
  format!("{{\"session_id\": \"{id}\"}}\n")
}

/// Newline-terminated event lines, the shape the raw logs carry.
fn events(lines: &[&str]) -> Vec<u8> {
  let mut text = lines.join("\n");
  text.push('\n');
  text.into_bytes()
}

/// One stand-in CLI file: `<home>/cli/<dir>/<name>.json`, holding the records that file would yield.
fn cli_file(home: &Path, dir: &str, name: &str, records: &[Value]) -> PathBuf {
  let directory = home.join("cli").join(dir);
  fs::create_dir_all(&directory).unwrap();
  let path = directory.join(format!("{name}.json"));
  fs::write(&path, serde_json::to_vec(records).unwrap()).unwrap();
  path
}

/// A native call record, with `tokens` as (in_fresh, cache_write, cache_read, output).
fn call(record_id: &str, model: &str, ts: &str, tokens: [i64; 4], sessions: &[&str]) -> Value {
  json!({
    "record_id": record_id,
    "kind": "native",
    "model": model,
    "ts": ts,
    "in_fresh": tokens[0],
    "cache_write": tokens[1],
    "cache_read": tokens[2],
    "output": tokens[3],
    "sessions": sessions,
  })
}

/// The Claude and Codex corpus: an original response, its replay in a second file, an empty session file, a
/// subagent response, and one Codex rollout.
fn claude_codex_corpus(home: &Path) {
  let usage = [100, 20, 40, 30];
  let original = call("m1-id", NAME, "2024-01-01T00:00:00Z", usage, &["sess1"]);
  cli_file(home, "claude", "sess1", std::slice::from_ref(&original));
  cli_file(home, "claude", "replay", &[original]);
  cli_file(home, "claude", "sess2", &[]);
  cli_file(home, "claude", "subagent", &[call("sub-id", NAME, "2024-01-02T00:00:00Z", [50, 10, 0, 5], &["sess2"])]);
  cli_file(
    home,
    "codex",
    "rolloutroot1",
    &[call("codex-some", "codex-some", "2024-01-03T00:00:00Z", [40, 0, 20, 7], &["rolloutroot1"])],
  );
}

fn capture_round(ledger: &mut UsageLedger, config: &CharlieBotConfig, registry: &Registry) -> Vec<(String, usize)> {
  capture_usage(ledger, HOST, &config.sessions_dir(), config, registry).unwrap()
}

fn count_of(counts: &[(String, usize)], source: &str) -> usize {
  counts.iter().find(|(name, _)| name == source).map(|(_, count)| *count).unwrap_or_else(|| panic!("no {source} count"))
}

fn page_rows(ledger: &mut UsageLedger) -> Vec<LedgerRow> {
  ledger.model_rows().unwrap()
}

fn row<'a>(rows: &'a [LedgerRow], source: &str, model: &str) -> &'a LedgerRow {
  rows
    .iter()
    .find(|row| row.source == source && row.model == model)
    .unwrap_or_else(|| panic!("no {source} row for {model}"))
}

fn fallback_sessions(ledger: &UsageLedger, record_id: &str) -> Vec<String> {
  let mut statement =
    ledger.connection().prepare("SELECT session FROM fallback_sessions WHERE record_id = ? ORDER BY session").unwrap();
  let sessions = statement.query_map([record_id], |row| row.get::<_, String>(0)).unwrap();
  sessions.collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

fn reads() -> Vec<PathBuf> {
  READS.with(|reads| reads.borrow().clone())
}

#[test]
fn capture_runs_sources_in_order_skips_unchanged_files_and_keeps_recorded_rows() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  let external = home.join("cli/external.log");
  fs::create_dir_all(external.parent().unwrap()).unwrap();
  fs::write(&external, "source log\n").unwrap();
  EXTERNAL_READS.store(0, Ordering::SeqCst);
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let first = capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &registry).unwrap();
  assert_eq!(
    first,
    [
      ("External".into(), 1),
      ("Codex".into(), 0),
      ("Claude Code".into(), 0),
      ("opencode".into(), 0),
      ("charlie-bot".into(), 0)
    ]
  );
  assert_eq!(EXTERNAL_READS.load(Ordering::SeqCst), 1);
  let second = capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &registry).unwrap();
  assert_eq!(
    second,
    [
      ("External".into(), 0),
      ("Codex".into(), 0),
      ("Claude Code".into(), 0),
      ("opencode".into(), 0),
      ("charlie-bot".into(), 0)
    ]
  );
  assert_eq!(EXTERNAL_READS.load(Ordering::SeqCst), 1);
  assert!(ledger.last_capture_at(HOST).unwrap().is_some());
}

#[test]
fn charliebot_threads_masters_and_runs_match_their_verdicts_and_marker_rules() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  // The Codex stand-in's live sessions: rollouts on disk named for the ids the threads below carry.
  cli_file(&home, "codex", "codex-live", &[]);
  cli_file(&home, "codex", "live-run", &[]);
  let mut native = result_line(TS_A, 20, 4, 6, 7);
  native.push_str("{\"session_id\": \"native-id\"}\n");
  native.push_str("{\"type\": \"result\", broken}\n");
  native.push_str("{\"type\": \"result\", \"usage\": {\"input_tokens\": 900}");
  write_thread(
    &home,
    "session-native",
    "native",
    Some(("charlie-code-native", Some("vendor/recorded-model"))),
    native.as_bytes(),
  );
  let no_id = result_line(TS_A, 12, 2, 3, 4);
  write_thread(&home, "session-native", "native-no-id", Some(("charlie-code-native", None)), no_id.as_bytes());
  let mut codex_offline = result_line(TS_A, 100, 0, 30, 14);
  codex_offline.push_str("{\"session_id\": \"codex-offline\"}\n");
  write_thread(
    &home,
    "session-codex",
    "codex-offline",
    Some(("codex-main", Some("vendor/gpt-offline"))),
    codex_offline.as_bytes(),
  );
  let mut codex_live = result_line(TS_A, 50, 0, 10, 8);
  codex_live.push_str("{\"session_id\": \"codex-live\"}\n");
  write_thread(&home, "session-codex", "codex-live", Some(("codex-main", None)), codex_live.as_bytes());
  write_thread(
    &home,
    "session-codex",
    "codex-no-id",
    Some(("codex-main", None)),
    result_line(TS_A, 50, 0, 10, 8).as_bytes(),
  );
  let mut fallback = result_line(TS_A, 40, 8, 12, 9);
  fallback.push_str("{\"session_id\": \"claude-kept\"}\n");
  write_thread(
    &home,
    "session-claude",
    "claude-kept",
    Some(("claude-main", Some("vendor/claude-模型"))),
    fallback.as_bytes(),
  );
  write_thread(
    &home,
    "session-claude",
    "claude-no-id",
    Some(("claude-main", None)),
    result_line(TS_A, 2, 0, 0, 1).as_bytes(),
  );
  let mut retired = result_line(TS_A, 8, 0, 2, 3);
  retired.push_str("{\"session_id\": \"codex-retired-id\"}\n");
  write_thread(&home, "session-retired", "retired", Some(("codex-retired-gpt-legacy", None)), retired.as_bytes());
  write_thread(
    &home,
    "session-unknown",
    "unknown",
    Some(("no-source-backend", None)),
    result_line(TS_A, 3, 0, 0, 1).as_bytes(),
  );
  write_thread(&home, "session-missing-meta", "thread", None, result_line(TS_A, 3, 0, 0, 1).as_bytes());
  write_thread(
    &home,
    "session-unicode",
    "utf",
    Some(("charlie-code-native", Some("vendor/模型-🚀"))),
    result_line(TS_A, 9, 0, 0, 2).as_bytes(),
  );

  let master_time = "2026-09-01T00:00:00Z";
  let master_events = concat!(
    "{\"type\": \"context\", \"model\": \"vendor/native-model\"}\n",
    "{\"type\": \"result\", \"usage\": {\"input_tokens\": 20, \"cached_tokens\": 4, \"output_tokens\": 7}}\n",
    "{\"type\": \"result\", \"usage\": {\"input_tokens\": 30, \"cached_tokens\": 9, \"output_tokens\": 11}}\n",
  );
  write_master(&home, "session-master", master_time, master_events.as_bytes());
  write_master(
    &home,
    "session-master",
    "2026-09-02T00:00:00Z",
    b"{\"type\": \"result\", \"usage\": {\"input_tokens\": 8}}\n",
  );
  write_master(
    &home,
    "session-master",
    "2026-09-03T00:00:00Z",
    b"{\"type\": \"context\", \"model\": \"vendor/native-model\"}\n",
  );

  let native_run = concat!(
    "{\"type\": \"result\", \"usage\": {\"input_tokens\": 18, \"cached_tokens\": 4, \"output_tokens\": 7}}\n",
    "{\"type\": \"result\", broken}\n",
    "{\"type\": \"result\", \"usage\": {\"input_tokens\": 900}"
  );
  let native_run_path =
    write_run(&home, "run-session", "run-native", "charlie-code-native", None, None, native_run.as_bytes());
  let codex_run = concat!(
    "{\"type\": \"turn.completed\", \"usage\": {\"input_tokens\": 60, \"cached_input_tokens\": 20, \"output_tokens\": 8}}\n",
    "{\"type\": \"turn.completed\", \"usage\": {\"input_tokens\": 40, \"cached_input_tokens\": 10, \"output_tokens\": 6}}\n",
    "{\"type\": \"thread.started\", \"thread_id\": \"codex-offline-run\"}\n"
  );
  write_run(&home, "run-session", "run-codex", "codex-main", Some("vendor/gpt-run"), None, codex_run.as_bytes());
  let live_run = concat!(
    "{\"type\": \"turn.completed\", \"usage\": {\"input_tokens\": 8, \"cached_input_tokens\": 2, \"output_tokens\": 1}}\n",
    "{\"type\": \"thread.started\", \"thread_id\": \"live-run\"}\n"
  );
  write_run(&home, "run-session", "run-codex-live", "codex-main", None, None, live_run.as_bytes());
  write_run(
    &home,
    "run-session",
    "run-codex-no-id",
    "codex-main",
    None,
    None,
    b"{\"type\": \"turn.completed\", \"usage\": {\"input_tokens\": 8, \"cached_input_tokens\": 2, \"output_tokens\": 1}}\n",
  );
  write_run(
    &home,
    "run-session",
    "run-claude",
    "claude-main",
    None,
    Some("claude-run"),
    b"{\"session_id\": \"claude-run\"}\n{\"type\": \"result\", \"usage\": {\"input_tokens\": 13, \"cache_creation_input_tokens\": 3, \"cache_read_input_tokens\": 2, \"output_tokens\": 6}}\n",
  );
  write_run(
    &home,
    "run-session",
    "run-claude-no-id",
    "claude-main",
    None,
    None,
    b"{\"type\": \"result\", \"usage\": {\"input_tokens\": 13, \"output_tokens\": 6}}\n",
  );
  write_run(
    &home,
    "run-session",
    "run-unknown",
    "unknown-backend",
    None,
    Some("unknown"),
    b"{\"type\": \"result\", \"usage\": {\"input_tokens\": 13, \"output_tokens\": 6}}\n",
  );
  write_run(
    &home,
    "run-session",
    "run-resultless",
    "charlie-code-native",
    None,
    None,
    b"{\"type\": \"thread.started\", \"thread_id\": \"unused\"}\n",
  );

  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let counts = capture_round(&mut ledger, &config, &registry);
  assert_eq!(
    counts.iter().map(|(source, _)| source.as_str()).collect::<Vec<_>>(),
    ["External", "Codex", "Claude Code", "opencode", "charlie-bot"]
  );
  assert_eq!(count_of(&counts, "charlie-bot"), 10);
  let usage = ledger.connection().prepare("SELECT record_id, kind, model, account, in_fresh, cache_write, cache_read, in_unsplit, output FROM usage ORDER BY record_id")
    .unwrap()
    .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, i64>(4)?, row.get::<_, i64>(5)?, row.get::<_, i64>(6)?, row.get::<_, i64>(7)?, row.get::<_, i64>(8)?)))
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap();
  let by_id = usage.into_iter().map(|row| (row.0.clone(), row)).collect::<std::collections::BTreeMap<_, _>>();
  assert_eq!(by_id.len(), 10);
  assert_eq!(by_id["thread:session-native/native/0"].2, "recorded-model");
  assert_eq!((by_id["thread:session-native/native/0"].7, by_id["thread:session-native/native/0"].8), (20, 7));
  assert_eq!(by_id["thread:session-native/native-no-id/0"].2, "native-model");
  assert_eq!(
    (by_id["thread:session-codex/codex-offline/0"].4, by_id["thread:session-codex/codex-offline/0"].6),
    (70, 30)
  );
  assert_eq!(by_id["thread:session-claude/claude-kept/0"].2, "claude-模型");
  assert_eq!(by_id["thread:session-retired/retired/0"].2, "retired-gpt-legacy");
  assert_eq!(by_id["thread:session-unicode/utf/0"].2, "模型-🚀");
  assert_eq!(by_id["master:session-master/2026-09-01T00:00:00Z"].3, "charlie-code-native");
  assert_eq!(
    (by_id["master:session-master/2026-09-01T00:00:00Z"].4, by_id["master:session-master/2026-09-01T00:00:00Z"].6),
    (21, 9)
  );
  assert_eq!((by_id["run:run-native"].4, by_id["run:run-native"].6, by_id["run:run-native"].8), (14, 4, 7));
  assert_eq!((by_id["run:run-codex"].4, by_id["run:run-codex"].6, by_id["run:run-codex"].8), (70, 30, 14));
  assert_eq!(
    (by_id["run:run-claude"].4, by_id["run:run-claude"].5, by_id["run:run-claude"].6, by_id["run:run-claude"].8),
    (13, 3, 2, 6)
  );
  assert!(!by_id.contains_key("thread:session-codex/codex-live/0"));
  assert!(!by_id.contains_key("thread:session-codex/codex-no-id/0"));
  assert!(!by_id.contains_key("thread:session-claude/claude-no-id/0"));
  assert!(!by_id.contains_key("run:run-codex-live"));
  assert!(!by_id.contains_key("run:run-codex-no-id"));
  assert!(!by_id.contains_key("run:run-claude-no-id"));
  assert!(!by_id.contains_key("run:run-resultless"));
  assert_eq!(ledger.captured_sigs(HOST).unwrap().values().filter(|sig| sig.starts_with("p2:")).count(), 14);
  let second = capture_round(&mut ledger, &config, &registry);
  assert!(second.iter().all(|(_, count)| *count == 0));
  assert!(ledger.last_capture_at(HOST).unwrap().is_some());

  fs::write(&native_run_path, format!("{native_run}\n{}", result_line(TS_A, 19, 0, 0, 8))).unwrap();
  let appended = capture_round(&mut ledger, &config, &registry);
  assert_eq!(count_of(&appended, "charlie-bot"), 1);
  let output: i64 = ledger
    .connection()
    .query_row("SELECT output FROM usage WHERE record_id = 'run:run-native'", [], |row| row.get(0))
    .unwrap();
  assert_eq!(output, 8);
}

#[test]
fn charliebot_capture_replaces_a_bare_stat_signature_with_the_p2_signature() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  let path = write_thread(
    &home,
    "session-native",
    "thread",
    Some(("charlie-code-native", None)),
    result_line(TS_A, 10, 0, 0, 2).as_bytes(),
  );
  let metadata = fs::metadata(&path).unwrap();
  let bare_signature = format!("{}:{}", mtime_ns(&metadata), metadata.len());
  let captured = BTreeMap::from([(path.to_string_lossy().into_owned(), bare_signature)]);
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(capture_charliebot(&mut ledger, HOST, &config.sessions_dir(), &config, &registry, &captured).unwrap(), 1);
  let stored = ledger.captured_sigs(HOST).unwrap();
  assert!(stored[path.to_str().unwrap()].starts_with("p2:"));
  assert_eq!(capture_charliebot(&mut ledger, HOST, &config.sessions_dir(), &config, &registry, &stored).unwrap(), 0);
}

#[test]
fn capture_without_a_registered_source_raises() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let empty = Registry::new();
  let error = capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &empty).unwrap_err();
  assert_eq!(
    error.to_string(),
    "no usage source is registered: the entry point must call registrations.register_all()"
  );
  assert!(ledger.last_capture_at(HOST).unwrap().is_none());
  drop(registry);
}

// The cases of tests/test_token_tally.py, in the file's order. The Claude, Codex and opencode cases run on the
// CliStub stand-ins, so their values are the records the stand-ins yield; the readers that parse those logs live in
// the agents crate.

#[test]
fn capture_usage_rows_are_absolutely_correct() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  claude_codex_corpus(&home);
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let counts = capture_round(&mut ledger, &config, &registry);
  // Every parsed file's records, replays included: the replay and the subagent response land too.
  assert_eq!((count_of(&counts, "Claude Code"), count_of(&counts, "Codex")), (3, 1));
  let rows = page_rows(&mut ledger);
  let claude = row(&rows, "Claude Code", NAME);
  assert_eq!(claude.calls, 2); // one original + one subagent; the replay is deduped
  assert_eq!(
    (claude.in_fresh.int(), claude.cache_write.int(), claude.cache_read.int(), claude.output.int()),
    (150, 30, 40, 35)
  );
  assert_eq!(
    claude.total.int(),
    claude.in_fresh.int() + claude.cache_write.int() + claude.cache_read.int() + claude.output.int()
  );
  assert_eq!((claude.first.as_str(), claude.last.as_str()), ("2024-01-01", "2024-01-02"));
  assert_eq!(claude.accounts.iter().map(|account| account.total.int()).sum::<i64>(), claude.total.int());
  let codex = row(&rows, "Codex", "codex-some");
  assert_eq!((codex.in_fresh.int(), codex.cache_read.int(), codex.output.int()), (40, 20, 7));
  assert_eq!((codex.first.as_str(), codex.last.as_str()), ("2024-01-03", "2024-01-03"));
  assert_eq!(codex.total.int(), 40 + 20 + 7);
}

#[test]
fn appends_are_visible() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  cli_file(&home, "claude", "sess1", &[call("m1", NAME, "2024-01-01T00:00:00Z", [10, 0, 0, 5], &["sess1"])]);
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  capture_round(&mut ledger, &config, &registry);
  let before = row(&page_rows(&mut ledger), "Claude Code", NAME).clone();

  // A later session file records the same model: its total rises by exactly those tokens.
  cli_file(&home, "claude", "sess2", &[call("m2", NAME, "2024-01-02T00:00:00Z", [1000, 0, 0, 2], &["sess2"])]);
  capture_round(&mut ledger, &config, &registry);
  let after = row(&page_rows(&mut ledger), "Claude Code", NAME).clone();
  assert_eq!(after.total.int(), before.total.int() + 1002);
  assert_eq!(after.calls, before.calls + 1);
}

#[test]
fn replays_are_not_double_counted() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  cli_file(&home, "claude", "sess1", &[call("m1", NAME, "2024-01-01T00:00:00Z", [100, 0, 0, 10], &["sess1"])]);
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  capture_round(&mut ledger, &config, &registry);
  let before = row(&page_rows(&mut ledger), "Claude Code", NAME).clone();

  // Copy the session file verbatim to a new session id (resume/fork behaviour).
  fs::copy(home.join("cli/claude/sess1.json"), home.join("cli/claude/sess2.json")).unwrap();
  capture_round(&mut ledger, &config, &registry);
  let after = row(&page_rows(&mut ledger), "Claude Code", NAME).clone();
  assert_eq!(after.total.int(), before.total.int());
  assert_eq!(after.calls, before.calls);
}

#[test]
fn a_rewritten_file_reads_whole() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  cli_file(&home, "claude", "sess1", &[call("m1", NAME, "2024-01-01T00:00:00Z", [10, 0, 0, 5], &["sess1"])]);
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  capture_round(&mut ledger, &config, &registry);

  // Whole-file rewrite: early content replaced, last record preserved.
  cli_file(
    &home,
    "claude",
    "sess1",
    &[
      call("m9", NAME, "2024-01-03T00:00:00Z", [7, 0, 0, 7], &["sess1"]),
      call("m1", NAME, "2024-01-01T00:00:00Z", [10, 0, 0, 5], &["sess1"]),
    ],
  );
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "Claude Code"), 2);
  let row_now = row(&page_rows(&mut ledger), "Claude Code", NAME).clone();
  assert_eq!((row_now.calls, row_now.total.int()), (2, 29));

  cli_file(&home, "claude", "sess1", &[call("m2", NAME, "2024-01-02T00:00:00Z", [3, 0, 0, 3], &["sess1"])]);
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "Claude Code"), 1);
  let row_now = row(&page_rows(&mut ledger), "Claude Code", NAME).clone();
  assert_eq!((row_now.calls, row_now.total.int()), (3, 35)); // m2 joins the records the ledger keeps
}

#[test]
fn backend_page_source_attributes_by_type_prefix_and_master_account() {
  // The attribution reads a registered id's config type first (the type wins over the id's prefix), an off-config
  // id's type prefix next, and the master-run account as CLC through its clc- prefix. The gemini type has no usage
  // source in this fixture; the agents crate registers the Antigravity and kimi types.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let options = [
    option("charlie-code-glm", "charlie-code", "openai/zai-org/GLM-5.3-Flash"),
    option("codex-gpt", "codex", "openai/gpt-5"),
    option("claude-opus", "cc-claude", "anthropic/claude-opus-5-5"),
    option("claude-mislabeled", "codex", "openai/gpt-5"),
    option("oc-qwen", "opencode", "qwen/qwen3"),
  ]
  .concat();
  let (config, registry) = fixture_with(&home, &options);
  let source_of = |backend: &str| backend_verdict(backend, &config, &registry).unwrap().map(|(_, source)| source.name);
  assert_eq!(source_of("charlie-code-glm"), Some("CLC"));
  assert_eq!(source_of("codex-gpt"), Some("Codex"));
  assert_eq!(source_of("claude-opus"), Some("Claude Code"));
  assert_eq!(source_of("claude-mislabeled"), Some("Codex"));
  assert_eq!(source_of("oc-qwen"), Some("opencode"));
  assert_eq!(source_of("charlie-code-retired"), Some("CLC"));
  assert_eq!(source_of("codex-retired"), Some("Codex"));
  assert_eq!(source_of("claude-retired"), Some("Claude Code"));
  assert_eq!(source_of("opencode-retired"), Some("opencode"));
  assert_eq!(source_of("oc-kimi-k3"), Some("opencode"));
  assert_eq!(source_of("clc-kimi-k3"), Some("CLC"));
  assert_eq!(source_of("clc-master"), Some("CLC"));
  assert_eq!(source_of("mystery-retired"), None);
}

#[test]
fn a_retired_id_names_its_row_by_the_prefix_its_source_registers() {
  let dir = tempfile::tempdir().unwrap();
  let (config, registry) = fixture(&dir.path().join("home"));
  let metadata = Map::new();
  let model_of = |backend: &str| thread_row_model(&metadata, &config, &registry, backend).unwrap();
  assert_eq!(model_of("oc-kimi-k3"), "kimi-k3");
  assert_eq!(model_of("clc-kimi-k3"), "kimi-k3");
  assert_eq!(model_of("opencode-retired"), "retired");
  assert_eq!(model_of("charlie-code-retired"), "retired");
}

#[test]
fn charliebot_thread_row_takes_the_recorded_model_first() {
  // A thread's row names the model its metadata recorded even when config maps its id to a later version; a thread
  // without a recorded model takes the config model, and a retired id without one takes the id minus its prefix.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) =
    fixture_with(&home, &option("charlie-code-glm-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"));
  write_thread(
    &home,
    "s1",
    "t1",
    Some(("charlie-code-glm-flash", Some("openai/zai-org/GLM-5.3-Flash"))),
    result_line("2026-09-11T20:00:00+00:00", 100, 0, 0, 5).as_bytes(),
  );
  write_thread(
    &home,
    "s1",
    "t2",
    Some(("charlie-code-glm-flash", None)),
    result_line("2026-09-12T20:00:00+00:00", 200, 0, 0, 5).as_bytes(),
  );
  write_thread(
    &home,
    "s1",
    "t3",
    Some(("charlie-code-kimi-k3", None)),
    result_line("2026-09-13T20:00:00+00:00", 300, 0, 0, 5).as_bytes(),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 3);
  let rows = page_rows(&mut ledger);
  let recorded = row(&rows, "charlie-bot", "GLM-5.3-Flash");
  assert_eq!((recorded.calls, recorded.total.int()), (1, 105));
  assert_eq!(
    recorded.accounts.iter().map(|account| account.name.as_str()).collect::<Vec<_>>(),
    ["charlie-code-glm-flash"]
  );
  assert_eq!(row(&rows, "charlie-bot", "GLM-5.4-Flash").total.int(), 205);
  let retired = row(&rows, "charlie-bot", "kimi-k3");
  assert_eq!(retired.total.int(), 305);
  assert_eq!(
    retired.accounts.iter().map(|account| account.name.as_str()).collect::<Vec<_>>(),
    ["charlie-code-kimi-k3"]
  );
}

#[test]
fn captured_rows_survive_deleting_the_source_files() {
  // The ledger contains no delete, so its rows outlive the logs they were parsed from.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  claude_codex_corpus(&home);
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  capture_round(&mut ledger, &config, &registry);
  let before = page_rows(&mut ledger);
  fs::remove_dir_all(home.join("cli")).unwrap();
  capture_round(&mut ledger, &config, &registry);
  assert_eq!(page_rows(&mut ledger), before);
}

#[test]
fn second_capture_without_changes_writes_nothing() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  claude_codex_corpus(&home);
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let first = capture_round(&mut ledger, &config, &registry);
  assert_eq!((count_of(&first, "Claude Code"), count_of(&first, "Codex")), (3, 1));
  assert!(capture_round(&mut ledger, &config, &registry).iter().all(|(_, count)| *count == 0));
}

#[test]
fn appended_line_is_captured_and_second_dir_replay_counts_once() {
  // An appended record reaches the ledger on the next capture (the file rewrites whole: its captured records
  // re-upsert beside the new one), and a message id replayed into a second file is captured yet still counted once.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  let original = call("m1", NAME, "2024-01-01T00:00:00Z", [10, 0, 0, 5], &["sess1"]);
  cli_file(&home, "claude", "sess1", std::slice::from_ref(&original));
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  capture_round(&mut ledger, &config, &registry);
  let before = row(&page_rows(&mut ledger), "Claude Code", NAME).clone();

  cli_file(&home, "claude", "replay", std::slice::from_ref(&original));
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "Claude Code"), 1);
  let replayed = row(&page_rows(&mut ledger), "Claude Code", NAME).clone();
  assert_eq!(replayed.calls, before.calls);
  assert_eq!(replayed.calls, 1); // ... yet its message id counts once

  let appended = call("m2", NAME, "2024-01-02T00:00:00Z", [100, 0, 0, 2], &["sess1"]);
  cli_file(&home, "claude", "sess1", &[original, appended]);
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "Claude Code"), 2);
  let after = row(&page_rows(&mut ledger), "Claude Code", NAME).clone();
  assert_eq!((after.calls, after.output.int()), (2, 7));
  assert_eq!((after.first.as_str(), after.last.as_str()), ("2024-01-01", "2024-01-02"));
}

#[test]
fn codex_p2_signature_rereads_the_unchanged_rollout_once() {
  // A rollout stored under the previous release's p2: signature never matches the bare stat pair, so the unchanged
  // file reads again once and then skips on the bare pair.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  let path = cli_file(
    &home,
    "codex",
    "sigthread1",
    &[call("codex-m1", "codex-m1", "2026-09-10T00:00:00Z", [40, 0, 20, 7], &["sigthread1"])],
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let metadata = fs::metadata(&path).unwrap();
  ledger
    .record_file(HOST, path.to_str().unwrap(), &format!("p2:{}:{}", mtime_ns(&metadata), metadata.len()), &[])
    .unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "Codex"), 1);
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "Codex"), 0);
}

#[test]
fn charliebot_capture_rows() {
  // CLC threads plus a CLC master capture give the ledger one row per model: the recorded model naming the first
  // thread's row, the config model and the master's context model merging into the second. An unchanged corpus's
  // second capture writes nothing.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) =
    fixture_with(&home, &option("charlie-code-glm-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"));
  write_thread(
    &home,
    "s1",
    "t1",
    Some(("charlie-code-glm-flash", Some("openai/zai-org/GLM-5.3-Flash"))),
    result_line("2026-09-11T20:00:00+00:00", 100, 0, 0, 5).as_bytes(),
  );
  write_thread(
    &home,
    "s1",
    "t2",
    Some(("charlie-code-glm-flash", None)),
    result_line("2026-09-12T20:00:00+00:00", 200, 0, 0, 6).as_bytes(),
  );
  write_master(
    &home,
    "s1",
    "20260913T000000Z",
    &events(&[
      r#"{"type": "context", "model": "openai/zai-org/GLM-5.4-Flash"}"#,
      r#"{"type": "result", "usage": {"input_tokens": 300, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 7}}"#,
    ]),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 3);
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 0);
  let rows = page_rows(&mut ledger);
  let charlie: Vec<&LedgerRow> = rows.iter().filter(|row| row.source == "charlie-bot").collect();
  assert_eq!(charlie.len(), 2);
  // A CLC thread's usage logs no cache fields: the whole input lands in in_unsplit.
  let recorded = row(&rows, "charlie-bot", "GLM-5.3-Flash");
  assert_eq!(
    (
      recorded.calls,
      recorded.in_fresh.int(),
      recorded.cache_write.int(),
      recorded.cache_read.int(),
      recorded.in_unsplit.int(),
      recorded.output.int()
    ),
    (1, 0, 0, 0, 100, 5)
  );
  // t2's input is unsplit like t1's; the master's carries no cached reads.
  let merged = row(&rows, "charlie-bot", "GLM-5.4-Flash");
  assert_eq!((merged.calls, merged.in_fresh.int(), merged.in_unsplit.int(), merged.output.int()), (2, 300, 200, 6 + 7));
}

#[test]
fn charliebot_codex_thread_counts_its_cached_reads_once() {
  // A codex-type thread result's input_tokens already includes its cached reads, so in_fresh keeps only the uncached
  // part and the reads land in cache_read, counted once rather than once per column.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture_with(&home, &option("codex-gpt", "codex", "openai/gpt-5"));
  let mut thread = result_line("2026-09-11T20:00:00+00:00", 906453, 0, 846080, 4104);
  thread.push_str(&session_line("sid-1"));
  write_thread(&home, "s1", "t1", Some(("codex-gpt", None)), thread.as_bytes());
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 1);
  let rows = page_rows(&mut ledger);
  let row = row(&rows, "charlie-bot", "gpt-5");
  assert_eq!(
    (row.calls, row.in_fresh.int(), row.cache_write.int(), row.cache_read.int(), row.output.int()),
    (1, 60373, 0, 846080, 4104)
  );
  assert_eq!(row.total.int(), 906453 + 4104); // the envelope's input counted once, cached part included
}

#[test]
fn charliebot_clc_thread_lands_whole_in_unsplit() {
  // A CLC thread result's usage logs no cache fields, so the hit/miss split is unknowable: the whole input counts in
  // in_unsplit instead of reading as a fresh miss in in_fresh.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) =
    fixture_with(&home, &option("charlie-code-glm-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"));
  write_thread(
    &home,
    "s1",
    "t1",
    Some(("charlie-code-glm-flash", Some("openai/zai-org/GLM-5.3-Flash"))),
    events(&[r#"{"type": "result", "timestamp": "2026-09-11T20:00:00+00:00", "usage": {"input_tokens": 4677324, "output_tokens": 39171}}"#]).as_slice(),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 1);
  let rows = page_rows(&mut ledger);
  let row = row(&rows, "charlie-bot", "GLM-5.3-Flash");
  assert_eq!(
    (
      row.calls,
      row.in_fresh.int(),
      row.cache_write.int(),
      row.cache_read.int(),
      row.in_unsplit.int(),
      row.output.int()
    ),
    (1, 0, 0, 0, 4677324, 39171)
  );
  assert_eq!(row.total.int(), 4677324 + 39171);
}

#[test]
fn charliebot_cc_claude_thread_keeps_the_claude_split() {
  // A cc-claude thread result's usage is in the Claude envelope keys, which carry the split themselves: the counts
  // stay the envelope's own, whatever the codex and CLC envelopes needed.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture_with(&home, &option("claude-opus-5", "cc-claude", "anthropic/claude-opus-5-5"));
  let mut thread = result_line("2026-09-11T20:00:00+00:00", 100, 20, 40, 5);
  thread.push_str(&session_line("cl-sess"));
  write_thread(&home, "s1", "t1", Some(("claude-opus-5", Some("claude-opus-5-5"))), thread.as_bytes());
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 1);
  let rows = page_rows(&mut ledger);
  let row = row(&rows, "charlie-bot", "claude-opus-5-5");
  assert_eq!(
    (
      row.calls,
      row.in_fresh.int(),
      row.cache_write.int(),
      row.cache_read.int(),
      row.in_unsplit.int(),
      row.output.int()
    ),
    (1, 100, 20, 40, 0, 5)
  );
}

#[test]
fn charliebot_master_counts_its_cached_reads_once() {
  // A master capture's trailing result carries the run's usage with the cached reads inside the one input field; they
  // land in cache_read, not in in_fresh beside them.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) =
    fixture_with(&home, &option("charlie-code-glm53-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"));
  write_master(
    &home,
    "s1",
    "20260913T000000Z",
    &events(&[
      r#"{"type": "context", "model": "openai/zai-org/GLM-5.4-Flash"}"#,
      r#"{"type": "result", "usage": {"input_tokens": 1293011, "cached_tokens": 1283200, "output_tokens": 8712}}"#,
    ]),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 1);
  let rows = page_rows(&mut ledger);
  let row = row(&rows, "charlie-bot", "GLM-5.4-Flash");
  assert_eq!(
    (
      row.calls,
      row.in_fresh.int(),
      row.cache_write.int(),
      row.cache_read.int(),
      row.in_unsplit.int(),
      row.output.int()
    ),
    (1, 9811, 0, 1283200, 0, 8712)
  );
  assert_eq!(row.total.int(), 1293011 + 8712);
}

#[test]
fn charliebot_codex_thread_is_skipped_while_a_rollout_of_it_exists() {
  // The Codex rule: a codex-type thread any of whose session ids names a rollout still on disk contributes nothing;
  // the rollout's own native rows are the usage, and a partly pruned multi-session thread must not count twice. A
  // thread none of whose ids has a rollout counts as a fallback row. Deleting the rollout afterwards changes no row.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture_with(&home, &option("codex-gpt", "codex", "openai/gpt-5"));
  let sid = "rolloutsid1";
  cli_file(&home, "codex", sid, &[call(sid, "codex-gpt5", "2026-09-10T00:00:00Z", [40, 0, 20, 7], &[sid])]);
  for (thread, ids) in [("t1", vec![sid]), ("t2", vec![sid, "pruned-sid"]), ("t3", vec!["orphan-sid"])] {
    let mut events_text = result_line("2026-09-11T20:00:00+00:00", 100, 0, 0, 5);
    for id in ids {
      events_text.push_str(&session_line(id));
    }
    write_thread(&home, "s1", thread, Some(("codex-gpt", None)), events_text.as_bytes());
  }
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let written = capture_round(&mut ledger, &config, &registry);
  assert_eq!((count_of(&written, "Codex"), count_of(&written, "charlie-bot")), (1, 1)); // only t3's thread is stored
  let rows = page_rows(&mut ledger);
  assert_eq!(row(&rows, "Codex", "codex-gpt5").calls, 1);
  let fallback = row(&rows, "charlie-bot", "gpt-5");
  assert_eq!((fallback.calls, fallback.fallback_calls), (1, 1));
  assert_eq!(ledger.captured_sigs(HOST).unwrap().len(), 4); // the skipped threads are recorded too: never read again

  fs::remove_dir_all(home.join("cli/codex")).unwrap();
  assert!(capture_round(&mut ledger, &config, &registry).iter().all(|(_, count)| *count == 0));
  assert_eq!(page_rows(&mut ledger), rows); // the native rows survive the deletion
}

#[test]
fn charliebot_fallback_rows_count_until_the_cli_log_is_captured() {
  // A codex-type thread with no rollout anywhere and a claude-type thread with no Claude transcript are counted as
  // fallback rows; capturing the Claude transcript whose session id the thread names retires that row, while the
  // codex thread's row stays.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture_with(
    &home,
    &[option("codex-gpt", "codex", "openai/gpt-5"), option("claude-sonnet", "cc-claude", "anthropic/claude-sonnet-4")]
      .concat(),
  );
  let mut codex_thread = result_line("2026-09-11T20:00:00+00:00", 100, 0, 0, 5);
  codex_thread.push_str(&session_line("orphan-sid"));
  write_thread(&home, "s1", "t1", Some(("codex-gpt", None)), codex_thread.as_bytes());
  let mut claude_thread = result_line("2026-09-12T20:00:00+00:00", 200, 0, 0, 6);
  claude_thread.push_str(&session_line("cl-sess"));
  write_thread(&home, "s1", "t2", Some(("claude-sonnet", None)), claude_thread.as_bytes());
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 2);
  let rows = page_rows(&mut ledger);
  let codex_fallback = row(&rows, "charlie-bot", "gpt-5");
  assert_eq!(
    (codex_fallback.calls, codex_fallback.fallback_calls, codex_fallback.in_fresh.int(), codex_fallback.output.int()),
    (1, 1, 100, 5)
  );
  let claude_fallback = row(&rows, "charlie-bot", "claude-sonnet-4");
  assert_eq!((claude_fallback.calls, claude_fallback.fallback_calls), (1, 1));

  cli_file(&home, "claude", "cl-sess", &[call("m1", NAME, "2026-09-12T20:00:00Z", [200, 0, 0, 6], &["cl-sess"])]);
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "Claude Code"), 1);
  let rows = page_rows(&mut ledger);
  assert!(!rows.iter().any(|row| row.source == "charlie-bot" && row.model == "claude-sonnet-4")); // the transcript is captured
  assert_eq!(row(&rows, "charlie-bot", "gpt-5").fallback_calls, 1); // the codex thread stays counted
}

#[test]
fn charliebot_capture_skips_unkeyable_threads() {
  // A codex-type thread with no session ids (nothing to key the any-match exclusion on) and a thread whose backend id
  // neither the registry nor a type prefix can classify contribute no records; both files are still recorded as
  // captured so they are never re-parsed.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture_with(&home, &option("codex-gpt", "codex", "openai/gpt-5"));
  write_thread(
    &home,
    "s1",
    "t1",
    Some(("codex-gpt", None)),
    result_line("2026-09-11T20:00:00+00:00", 100, 0, 0, 5).as_bytes(),
  );
  let mut mystery = result_line("2026-09-12T20:00:00+00:00", 200, 0, 0, 6);
  mystery.push_str(&session_line("sid-x"));
  write_thread(&home, "s1", "t2", Some(("mystery-backend", None)), mystery.as_bytes());
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 0);
  assert!(page_rows(&mut ledger).is_empty());
  assert_eq!(ledger.captured_sigs(HOST).unwrap().len(), 2);
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 0);
}

#[test]
fn charliebot_capture_reparses_a_file_the_bare_signature_recorded() {
  // A thread file the older parser recorded under the bare stat pair re-parses once and rewrites its rows onto the
  // same record ids; a file whose p2 signature the ledger holds is skipped.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) =
    fixture_with(&home, &option("charlie-code-glm-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"));
  let path = write_thread(
    &home,
    "s1",
    "t1",
    Some(("charlie-code-glm-flash", Some("openai/zai-org/GLM-5.3-Flash"))),
    result_line("2026-09-11T20:00:00+00:00", 100, 0, 0, 5).as_bytes(),
  );
  let metadata = fs::metadata(&path).unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  // What the older parser wrote: the whole input in in_fresh, under the bare stat pair.
  let stale = UsageRecord {
    record_id: "thread:s1/t1/0".into(),
    kind: RecordKind::Native,
    source: "charlie-bot".into(),
    model: "GLM-5.3-Flash".into(),
    account: "charlie-code-glm-flash".into(),
    ts: "2026-09-11T20:00:00+00:00".into(),
    in_fresh: 100,
    cache_write: 0,
    cache_read: 0,
    in_unsplit: 0,
    output: 5,
    sessions: Vec::new(),
  };
  let bare = format!("{}:{}", mtime_ns(&metadata), metadata.len());
  assert_eq!(ledger.record_file(HOST, path.to_str().unwrap(), &bare, &[stale]).unwrap(), 1);
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 1); // the bare pair is no current signature
  let rows = page_rows(&mut ledger);
  let row = row(&rows, "charlie-bot", "GLM-5.3-Flash");
  // same id, corrected values
  assert_eq!((row.calls, row.in_fresh.int(), row.in_unsplit.int(), row.output.int()), (1, 0, 100, 5));
  assert!(ledger.captured_sigs(HOST).unwrap()[path.to_str().unwrap()].starts_with("p2:"));
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 0); // the p2 signature serves
}

#[test]
fn capture_runs_clc_run_is_native_with_the_cached_split() {
  // A CLC run records natively on the model its metadata names, its cached reads split out of the result's one input
  // field, and an unchanged run file is never re-parsed: a re-parse would count its record again.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) =
    fixture_with(&home, &option("charlie-code-glm53-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"));
  write_run(
    &home,
    "s1",
    "r1",
    "charlie-code-glm53-flash",
    Some("openai/zai-org/GLM-5.3-Flash"),
    None,
    b"{\"type\": \"result\", \"usage\": {\"input_tokens\": 100, \"cached_tokens\": 40, \"output_tokens\": 5}}\n",
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 1);
  let rows = page_rows(&mut ledger);
  let row = row(&rows, "charlie-bot", "GLM-5.3-Flash");
  assert_eq!((row.calls, row.in_fresh.int(), row.cache_read.int(), row.output.int()), (1, 60, 40, 5));
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 0); // the unchanged run file is skipped
}

#[test]
fn capture_runs_cc_claude_fallback_retires_when_the_transcript_is_captured() {
  // A cc-claude run is a fallback row keyed on the session ids its metadata and its raw stream's init line carry,
  // counted until the Claude transcript whose session one of those ids names is captured; either id source retires
  // the run.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture_with(&home, &option("claude-opus-5", "cc-claude", "anthropic/claude-opus-5-5"));
  write_run(
    &home,
    "s1",
    "r1",
    "claude-opus-5",
    Some("claude-opus-5-5"),
    Some("cl-sess-1"),
    events(&[
      r#"{"session_id": "cl-sess-1", "type": "result", "usage": {"input_tokens": 4, "cache_creation_input_tokens": 10, "cache_read_input_tokens": 100, "output_tokens": 7}}"#,
    ])
    .as_slice(),
  );
  write_run(
    &home,
    "s1",
    "r2",
    "claude-opus-5",
    None,
    None,
    events(&[
      r#"{"type": "system", "subtype": "init", "session_id": "cl-sess-2", "model": "claude-opus-5-5"}"#,
      r#"{"session_id": "cl-sess-2", "type": "result", "usage": {"input_tokens": 4, "cache_creation_input_tokens": 10, "cache_read_input_tokens": 100, "output_tokens": 7}}"#,
    ])
    .as_slice(),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 2); // no transcript yet: both count
  let rows = page_rows(&mut ledger);
  let first = row(&rows, "charlie-bot", "claude-opus-5-5");
  assert_eq!(
    (
      first.calls,
      first.fallback_calls,
      first.in_fresh.int(),
      first.cache_write.int(),
      first.cache_read.int(),
      first.output.int()
    ),
    (2, 2, 8, 20, 200, 14)
  );

  cli_file(&home, "claude", "cl-sess-1", &[call("m1", NAME, "2026-09-27T17:50:39Z", [4, 0, 0, 404], &["cl-sess-1"])]);
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "Claude Code"), 1);
  let rows = page_rows(&mut ledger);
  let stored = row(&rows, "charlie-bot", "claude-opus-5-5");
  assert_eq!((stored.calls, stored.fallback_calls), (1, 1)); // r1 retired, the stream-id run r2 stays

  cli_file(&home, "claude", "cl-sess-2", &[call("m2", NAME, "2026-09-28T12:46:23Z", [4, 0, 0, 404], &["cl-sess-2"])]);
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "Claude Code"), 1);
  assert!(!page_rows(&mut ledger).iter().any(|row| row.source == "charlie-bot" && row.model == "claude-opus-5-5")); // r2 retired too
}

#[test]
fn capture_runs_codex_sums_turns_and_retires_on_the_captured_rollout() {
  // A codex run sums its turn.completed lines into one fallback row keyed on the codex session id its metadata
  // carries; capturing the rollout whose session id matches retires it, and a run with no turn.completed line
  // contributes no record at all.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture_with(&home, &option("codex-gpt", "codex", "openai/gpt-5"));
  write_run(
    &home,
    "s1",
    "r1",
    "codex-gpt",
    Some("openai/gpt-5"),
    Some("codex-sid-1"),
    events(&[
      r#"{"type": "thread.started", "thread_id": "th-1"}"#,
      r#"{"type": "turn.completed", "usage": {"input_tokens": 100, "cached_input_tokens": 20, "output_tokens": 5}}"#,
      r#"{"type": "turn.completed", "usage": {"input_tokens": 50, "cached_input_tokens": 10, "output_tokens": 3}}"#,
    ])
    .as_slice(),
  );
  write_run(
    &home,
    "s1",
    "r2",
    "codex-gpt",
    Some("openai/gpt-5"),
    None,
    events(&[r#"{"type": "thread.started", "thread_id": "th-2"}"#]).as_slice(),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 1); // r2 has no turn.completed line
  let rows = page_rows(&mut ledger);
  let row = row(&rows, "charlie-bot", "gpt-5");
  assert_eq!(
    (row.calls, row.fallback_calls, row.in_fresh.int(), row.cache_read.int(), row.output.int()),
    (1, 1, 120, 30, 8)
  );

  cli_file(
    &home,
    "codex",
    "codex-sid-1",
    &[call("codex-sid-1", "gpt-5", "2026-09-27T18:00:00Z", [120, 0, 30, 8], &["codex-sid-1"])],
  );
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "Codex"), 1);
  assert!(!page_rows(&mut ledger).iter().any(|row| row.source == "charlie-bot" && row.model == "gpt-5")); // excluded: its session has a native record
}

#[test]
fn capture_runs_writes_no_record_for_a_resultless_run_and_recaptures_nothing() {
  // A run killed before its result line contributes no record yet still records as captured (so it is never
  // re-parsed), and an unchanged corpus's second capture writes nothing.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) =
    fixture_with(&home, &option("charlie-code-glm53-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"));
  let lost = write_run_json(
    &home,
    "s1",
    "r-lost",
    r#"{"backend": "charlie-code-glm53-flash", "model": "openai/zai-org/GLM-5.3-Flash", "started_at": "2026-09-29T01:18:11.966557Z"}"#,
    events(&[r#"{"type": "system", "subtype": "init", "session_id": "cl-sess-9"}"#]).as_slice(),
  );
  let done = write_run_json(
    &home,
    "s1",
    "r-done",
    r#"{"backend": "charlie-code-glm53-flash", "model": "openai/zai-org/GLM-5.3-Flash", "started_at": "2026-09-29T01:20:00.000000Z"}"#,
    events(&[r#"{"type": "result", "usage": {"input_tokens": 30, "cached_tokens": 10, "output_tokens": 2}}"#])
      .as_slice(),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 1); // only the result-bearing run records
  let rows = page_rows(&mut ledger);
  let row = row(&rows, "charlie-bot", "GLM-5.3-Flash");
  assert_eq!((row.calls, row.in_fresh.int(), row.output.int()), (1, 20, 2));
  let captured = ledger.captured_sigs(HOST).unwrap().into_keys().collect::<BTreeSet<_>>();
  let expected = [lost, done].iter().map(|path| path.to_string_lossy().into_owned()).collect::<BTreeSet<_>>();
  assert_eq!(captured, expected); // the resultless file is marked captured too: never re-parsed
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 0);
}

#[test]
fn capture_runs_second_capture_skips_by_stat_and_recaptures_an_append() {
  // An unchanged run file's second capture decides on its stat pair alone: a re-parse would count its record again,
  // so a zero count means the file was skipped. An appended result line moves the stat, so the capture re-parses and
  // upserts the run's record on the new usage.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) =
    fixture_with(&home, &option("charlie-code-glm53-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"));
  let log = write_run(
    &home,
    "s1",
    "r1",
    "charlie-code-glm53-flash",
    Some("openai/zai-org/GLM-5.3-Flash"),
    None,
    b"{\"type\": \"result\", \"usage\": {\"input_tokens\": 100, \"cached_tokens\": 40, \"output_tokens\": 5}}\n",
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 1);
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 0); // the stat pair alone decides the skip

  let mut text = fs::read_to_string(&log).unwrap();
  text.push_str(r#"{"type": "result", "usage": {"input_tokens": 200, "cached_tokens": 10, "output_tokens": 9}}"#);
  text.push('\n');
  fs::write(&log, text).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 1); // the append moved the stat: re-parsed
  let rows = page_rows(&mut ledger);
  let row = row(&rows, "charlie-bot", "GLM-5.3-Flash");
  assert_eq!((row.calls, row.in_fresh.int(), row.cache_read.int(), row.output.int()), (1, 190, 10, 9));
}

#[test]
fn capture_usage_covers_every_source_and_zeroes_on_the_second_round() {
  // One call over Claude, Codex, opencode and a charlie-bot sessions dir writes each source's records and reports the
  // per-source counts; a second call over the unchanged corpus writes all zeros. CLC has no log of its own, so only
  // the charlie-bot label reports its records.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) =
    fixture_with(&home, &option("charlie-code-glm-flash", "charlie-code", "openai/zai-org/GLM-5.3-Flash"));
  cli_file(&home, "claude", "sess1", &[call("m1-id", NAME, "2024-01-01T00:00:00Z", [100, 0, 0, 30], &["sess1"])]);
  cli_file(
    &home,
    "codex",
    "codex-sid-1",
    &[call("codex-solo", "gpt-5", "2024-01-03T00:00:00Z", [40, 0, 20, 8], &["codex-sid-1"])],
  );
  cli_file(&home, "opencode", "m1", &[call("oc-m1", NAME, "2024-01-01T00:00:00Z", [50, 0, 0, 5], &["sess-a"])]);
  write_thread(
    &home,
    "s1",
    "t1",
    Some(("charlie-code-glm-flash", Some("openai/zai-org/GLM-5.3-Flash"))),
    result_line("2026-09-11T20:00:00+00:00", 200, 0, 0, 6).as_bytes(),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let first = capture_round(&mut ledger, &config, &registry);
  assert_eq!(
    (
      count_of(&first, "Claude Code"),
      count_of(&first, "Codex"),
      count_of(&first, "opencode"),
      count_of(&first, "charlie-bot")
    ),
    (1, 1, 1, 1)
  );
  assert!(capture_round(&mut ledger, &config, &registry).iter().all(|(_, count)| *count == 0));
}

#[test]
fn a_file_at_its_stored_signature_is_never_read() {
  // A file whose stat pair equals the stored signature skips its source's read() altogether, so an unchanged corpus
  // costs one stat per file; a file that moved reads again.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  claude_codex_corpus(&home);
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  capture_round(&mut ledger, &config, &registry);
  READS.with(|reads| reads.borrow_mut().clear());
  assert!(capture_round(&mut ledger, &config, &registry).iter().all(|(_, count)| *count == 0)); // every file skipped before its read
  assert!(reads().is_empty());

  cli_file(
    &home,
    "claude",
    "sess1",
    &[
      call("m1-id", NAME, "2024-01-01T00:00:00Z", [100, 20, 40, 30], &["sess1"]),
      call("m2", NAME, "2024-01-04T00:00:00Z", [1, 0, 0, 1], &["sess1"]),
    ],
  );
  let counts = capture_round(&mut ledger, &config, &registry);
  assert_eq!(count_of(&counts, "Claude Code"), 2); // the moved file reads again; only it does
  assert_eq!(count_of(&counts, "Codex"), 0);
  assert_eq!(reads(), vec![home.join("cli/claude/sess1.json")]);
}

#[test]
fn a_finished_capture_stamps_last_capture_at_and_a_failed_one_does_not() {
  // last_capture_at is the page's "as of": a capture that returns stamps its host, and a capture that raises leaves
  // the stamp alone.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert!(ledger.last_capture_at(HOST).unwrap().is_none());
  capture_round(&mut ledger, &config, &registry);
  let stamped = ledger.last_capture_at(HOST).unwrap();
  assert!(stamped.is_some());
  assert!(ledger.last_capture_at("host-b").unwrap().is_none());

  // A file that fails the read: sorted after the clean files, so the failure comes after the stamp would have.
  cli_file(&home, "claude", "zz-broken", &[json!({"fail": true})]);
  let error = capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &registry).unwrap_err();
  assert!(error.to_string().contains("zz-broken"), "{error}");
  assert_eq!(ledger.last_capture_at(HOST).unwrap(), stamped);
}

#[test]
fn capture_runs_opencode_run_is_a_fallback_through_the_claude_envelope_keys() {
  // Every backend whose source is neither run-logs-only nor under the Codex rule reads its run's trailing result
  // through the Claude envelope keys: an opencode-type run is a fallback row that retires once the opencode session
  // its metadata names is captured.
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture_with(&home, &option("oc-qwen", "opencode", "qwen/qwen3"));
  write_run(
    &home,
    "s1",
    "r1",
    "oc-qwen",
    Some("qwen/qwen3"),
    Some("oc-sess-1"),
    events(&[
      r#"{"session_id": "oc-sess-1", "type": "result", "usage": {"input_tokens": 4, "cache_creation_input_tokens": 10, "cache_read_input_tokens": 100, "output_tokens": 7}}"#,
    ])
    .as_slice(),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 1);
  let rows = page_rows(&mut ledger);
  let row = row(&rows, "charlie-bot", "qwen3");
  assert_eq!(
    (row.calls, row.fallback_calls, row.in_fresh.int(), row.cache_write.int(), row.cache_read.int(), row.output.int()),
    (1, 1, 4, 10, 100, 7)
  );

  cli_file(&home, "opencode", "m1", &[call("oc-m1", NAME, "2026-09-27T17:50:39Z", [4, 0, 0, 7], &["oc-sess-1"])]);
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "opencode"), 1);
  assert!(!page_rows(&mut ledger).iter().any(|row| row.source == "charlie-bot" && row.model == "qwen3")); // the opencode session is captured: retired
}

#[test]
fn a_capture_commits_once_and_a_failed_one_keeps_its_finished_files() {
  // No file of a capture is visible to another connection until the capture ends, the stamp lands in the same commit,
  // and a capture that raises still commits the files it finished (without the stamp).
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  claude_codex_corpus(&home);
  let path = dir.path().join("ledger.sqlite3");
  PROBE.with(|probe| *probe.borrow_mut() = Some(path.clone()));
  let mut ledger = UsageLedger::open(&path).unwrap();
  capture_round(&mut ledger, &config, &registry);
  PROBE.with(|probe| *probe.borrow_mut() = None);
  let seen = PROBED.with(|probed| probed.borrow().clone());
  assert!(seen.len() >= 2 && seen.iter().all(|count| *count == 0), "{seen:?}"); // nothing committed while files were read
  let mut reader = UsageLedger::open(&path).unwrap();
  assert_eq!(reader.captured_sigs(HOST).unwrap().len(), 5); // 4 Claude and Codex files + 1 rollout
  assert!(reader.last_capture_at(HOST).unwrap().is_some());

  let stamped = reader.last_capture_at(HOST).unwrap();
  cli_file(
    &home,
    "claude",
    "sess1",
    &[
      call("m1-id", NAME, "2024-01-01T00:00:00Z", [100, 20, 40, 30], &["sess1"]),
      call("m3", NAME, "2024-01-05T00:00:00Z", [1, 0, 0, 1], &["sess1"]),
    ],
  );
  cli_file(&home, "claude", "zz-broken", &[json!({"fail": true})]);
  assert!(capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &registry).is_err());
  assert_eq!(reader.last_capture_at(HOST).unwrap(), stamped); // the failed capture left the stamp alone
  let rows = page_rows(&mut reader);
  assert_eq!(row(&rows, "Claude Code", NAME).calls, 3); // ... but the Claude file it finished is durable
}

// The malformed record shapes: each builds the charlie-bot logs that carry them and asserts the rows Python's
// token_tally writes for the same files. Python skips a shape it cannot key, and reads the rest through its truth
// tests (`x or {}`, `if backend`, `line.get("type") == ...`).

const SHAPE_RESULT: &str = r#"{"type": "result", "timestamp": "2026-09-11T20:00:00+00:00", "usage": {"input_tokens": 10, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3, "output_tokens": 4}}"#;
const SHAPE_RUN_RESULT: &str = r#"{"type": "result", "usage": {"input_tokens": 10, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3, "output_tokens": 4}}"#;

fn charlie_rows(ledger: &mut UsageLedger) -> Vec<LedgerRow> {
  page_rows(ledger).into_iter().filter(|row| row.source == "charlie-bot").collect()
}

#[test]
fn malformed_type_shapes_capture_the_recorded_rows() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  write_thread(
    &home,
    "s-type",
    "t1",
    Some(("claude-main", Some("vendor/claude-model"))),
    &events(&[
      SHAPE_RESULT,
      r#"{"type": 7, "timestamp": "2026-09-11T20:00:00+00:00", "usage": {"input_tokens": 10, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3, "output_tokens": 4}}"#,
      r#"{"type": ["result"], "timestamp": "2026-09-11T20:00:00+00:00", "usage": {"input_tokens": 10, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3, "output_tokens": 4}}"#,
      r#"{"type": null, "session_id": "null-type-sid"}"#,
      r#"{"session_id": "claude-kept"}"#,
    ]),
  );
  write_master(
    &home,
    "s-type",
    "2026-09-01T00:00:00Z",
    &events(&[
      r#"{"type": "context", "model": "vendor/native-model"}"#,
      r#"{"type": 3, "model": "vendor/other"}"#,
      r#"{"type": "result", "usage": {"input_tokens": 20, "cached_tokens": 4, "output_tokens": 7}}"#,
      r#"{"type": ["result"], "usage": {"input_tokens": 999, "cached_tokens": 0, "output_tokens": 999}}"#,
    ]),
  );
  write_run_json(
    &home,
    "run-session",
    "run-type",
    r#"{"backend": "codex-main", "model": "vendor/gpt-run", "started_at": "2026-09-11T20:00:00+00:00"}"#,
    &events(&[
      r#"{"type": ["turn.completed"], "usage": {"input_tokens": 999, "cached_input_tokens": 0, "output_tokens": 9}}"#,
      r#"{"type": "turn.completed", "usage": {"input_tokens": 60, "cached_input_tokens": 20, "output_tokens": 8}}"#,
      r#"{"type": "thread.started", "thread_id": "run-type-thread"}"#,
      r#"{"type": 5, "thread_id": "not-a-session"}"#,
    ]),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 3);
  let rows = charlie_rows(&mut ledger);
  assert_eq!(rows.len(), 3);
  let claude = row(&rows, "charlie-bot", "claude-model");
  assert_eq!(
    (
      claude.calls,
      claude.in_fresh.int(),
      claude.cache_write.int(),
      claude.cache_read.int(),
      claude.output.int(),
      claude.total.int()
    ),
    (1, 10, 2, 3, 4, 19)
  );
  assert_eq!((claude.first.as_str(), claude.last.as_str(), claude.fallback_calls), ("2026-09-11", "2026-09-11", 1));
  assert_eq!(claude.accounts.iter().map(|account| account.name.as_str()).collect::<Vec<_>>(), ["claude-main"]);
  assert_eq!(fallback_sessions(&ledger, "thread:s-type/t1/0"), ["claude-kept", "null-type-sid"]);
  let native = row(&rows, "charlie-bot", "native-model");
  assert_eq!(
    (native.calls, native.in_fresh.int(), native.cache_read.int(), native.output.int(), native.total.int()),
    (1, 16, 4, 7, 27)
  );
  assert_eq!((native.first.as_str(), native.last.as_str()), ("2026-09-01", "2026-09-01"));
  let gpt = row(&rows, "charlie-bot", "gpt-run");
  assert_eq!(
    (gpt.calls, gpt.in_fresh.int(), gpt.cache_read.int(), gpt.output.int(), gpt.total.int(), gpt.fallback_calls),
    (1, 40, 20, 8, 68, 1)
  );
  assert_eq!(gpt.accounts.iter().map(|account| account.name.as_str()).collect::<Vec<_>>(), ["codex-main"]);
  assert_eq!(fallback_sessions(&ledger, "run:run-type"), ["run-type-thread"]);
  assert_eq!(ledger.captured_sigs(HOST).unwrap().len(), 3);
}

#[test]
fn malformed_session_shapes_capture_the_recorded_rows() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  write_thread(
    &home,
    "s-sid",
    "t1",
    Some(("claude-main", Some("vendor/claude-model"))),
    &events(&[
      SHAPE_RESULT,
      r#"{"session_id": 42}"#,
      r#"{"session_id": ""}"#,
      r#"{"session_id": null}"#,
      r#"{"session_id": ["x"]}"#,
      r#"{"session_id": "claude-kept"}"#,
    ]),
  );
  write_thread(
    &home,
    "s-only-bad",
    "t1",
    Some(("claude-main", Some("vendor/claude-model"))),
    &events(&[SHAPE_RESULT, r#"{"session_id": 42}"#, r#"{"session_id": ""}"#]),
  );
  write_thread(
    &home,
    "s-codex-bad",
    "t1",
    Some(("codex-main", Some("vendor/gpt-thread"))),
    &events(&[SHAPE_RESULT, r#"{"session_id": 42}"#]),
  );
  write_run_json(
    &home,
    "run-session",
    "run-sid",
    r#"{"backend": "codex-main", "model": "vendor/gpt-run", "started_at": "2026-09-11T20:00:00+00:00", "native_session_id": 7}"#,
    &events(&[
      r#"{"type": "turn.completed", "usage": {"input_tokens": 60, "cached_input_tokens": 20, "output_tokens": 8}}"#,
      r#"{"type": "thread.started", "thread_id": 5}"#,
      r#"{"type": "thread.started", "thread_id": ""}"#,
      r#"{"type": "thread.started", "thread_id": "run-sid-thread"}"#,
      r#"{"session_id": 3}"#,
    ]),
  );
  write_run_json(
    &home,
    "run-session",
    "run-sid-none",
    r#"{"backend": "claude-main", "model": null, "started_at": "2026-09-11T20:00:00+00:00", "native_session_id": ""}"#,
    &events(&[SHAPE_RUN_RESULT, r#"{"session_id": null}"#]),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 2);
  let rows = charlie_rows(&mut ledger);
  assert_eq!(rows.len(), 2);
  let claude = row(&rows, "charlie-bot", "claude-model");
  assert_eq!((claude.calls, claude.total.int()), (1, 19));
  assert_eq!(fallback_sessions(&ledger, "thread:s-sid/t1/0"), ["claude-kept"]);
  let gpt = row(&rows, "charlie-bot", "gpt-run");
  assert_eq!((gpt.calls, gpt.total.int()), (1, 68));
  assert_eq!(fallback_sessions(&ledger, "run:run-sid"), ["run-sid-thread"]);
  assert_eq!(ledger.captured_sigs(HOST).unwrap().len(), 5);
}

#[test]
fn malformed_backend_shapes_capture_nothing_and_mark_every_file() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  let metadatas = [
    r#"{"backend": false, "model": "vendor/claude-model"}"#,
    r#"{"backend": "", "model": "vendor/claude-model"}"#,
    r#"{"backend": 0, "model": "vendor/claude-model"}"#,
    r#"{"backend": [], "model": "vendor/claude-model"}"#,
    r#"{"backend": {}, "model": "vendor/claude-model"}"#,
    r#"{"model": "vendor/claude-model"}"#,
  ];
  let usage_events = events(&[SHAPE_RESULT, r#"{"session_id": "claude-kept"}"#]);
  for (index, metadata) in metadatas.into_iter().enumerate() {
    write_thread_json(&home, "s-falsy", &format!("t{index}"), Some(metadata), &usage_events);
  }
  write_thread_json(&home, "s-falsy", "t-absent", Some(metadatas[5]), &usage_events);
  let run_usage = events(&[SHAPE_RUN_RESULT]);
  write_run_json(
    &home,
    "run-session",
    "run-false",
    r#"{"backend": false, "started_at": "2026-09-11T20:00:00+00:00"}"#,
    &run_usage,
  );
  write_run_json(
    &home,
    "run-session",
    "run-zero",
    r#"{"backend": 0, "started_at": "2026-09-11T20:00:00+00:00"}"#,
    &run_usage,
  );
  write_run_json(
    &home,
    "run-session",
    "run-null",
    r#"{"backend": null, "started_at": "2026-09-11T20:00:00+00:00"}"#,
    &run_usage,
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 0);
  assert!(page_rows(&mut ledger).is_empty());
  assert_eq!(ledger.captured_sigs(HOST).unwrap().len(), 10);
}

#[test]
fn malformed_falsy_values_capture_the_recorded_rows() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  write_thread_json(
    &home,
    "s-values",
    "t1",
    Some(r#"{"backend": "claude-main", "model": false}"#),
    &events(&[r#"{"type": "result", "timestamp": false, "usage": false}"#, r#"{"session_id": "claude-kept"}"#]),
  );
  write_thread_json(
    &home,
    "s-values",
    "t2",
    Some(r#"{"backend": "claude-main", "model": ""}"#),
    &events(&[
      r#"{"type": "result", "timestamp": "", "usage": {"input_tokens": false, "output_tokens": ""}}"#,
      r#"{"session_id": "claude-kept"}"#,
    ]),
  );
  write_run_json(
    &home,
    "run-session",
    "run-values",
    r#"{"backend": "claude-main", "model": 0, "started_at": false}"#,
    &events(&[
      r#"{"type": "result", "usage": {"input_tokens": 13, "cache_read_input_tokens": false, "output_tokens": 6}}"#,
      r#"{"session_id": "claude-run"}"#,
    ]),
  );
  write_master(
    &home,
    "s-values",
    "2026-09-02T00:00:00Z",
    &events(&[
      r#"{"type": "context", "model": "vendor/native-model"}"#,
      r#"{"type": "context", "model": false}"#,
      r#"{"type": "result", "usage": {"input_tokens": 5, "cached_tokens": false, "output_tokens": 2}}"#,
    ]),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 4);
  let rows = charlie_rows(&mut ledger);
  assert_eq!(rows.len(), 2);
  let claude = row(&rows, "charlie-bot", "claude-model");
  assert_eq!(
    (claude.calls, claude.in_fresh.int(), claude.output.int(), claude.total.int(), claude.fallback_calls),
    (3, 13, 6, 19, 3)
  );
  assert_eq!((claude.first.as_str(), claude.last.as_str()), ("", ""));
  assert_eq!(claude.accounts.iter().map(|account| account.name.as_str()).collect::<Vec<_>>(), ["claude-main"]);
  let native = row(&rows, "charlie-bot", "native-model");
  assert_eq!((native.calls, native.in_fresh.int(), native.output.int(), native.total.int()), (1, 5, 2, 7));
  assert_eq!((native.first.as_str(), native.last.as_str()), ("2026-09-02", "2026-09-02"));
}

/// The five count columns of one stored row, in the order `in_fresh, cache_write, cache_read, in_unsplit, output`, as
/// SQLite holds them.
fn stored_counts(ledger: &UsageLedger, record_id: &str) -> Vec<SqlValue> {
  ledger
    .connection()
    .query_row(
      "SELECT in_fresh, cache_write, cache_read, in_unsplit, output FROM usage WHERE record_id = ?",
      [record_id],
      |row| (0..5).map(|index| row.get(index)).collect(),
    )
    .unwrap()
}

#[test]
fn malformed_count_shapes_capture_the_recorded_rows() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  write_thread(
    &home,
    "s-count",
    "claude",
    Some(("claude-main", Some("vendor/claude-model"))),
    &events(&[
      r#"{"type": "result", "timestamp": "2026-09-11T20:00:00+00:00", "usage": {"input_tokens": 1.5, "cache_creation_input_tokens": true, "cache_read_input_tokens": 3, "output_tokens": 4}}"#,
      r#"{"type": "result", "timestamp": "2026-09-11T20:00:01+00:00", "usage": {"input_tokens": true, "cache_creation_input_tokens": 2.0, "cache_read_input_tokens": 3, "output_tokens": 1.5}}"#,
      r#"{"session_id": "claude-count"}"#,
    ]),
  );
  write_thread(
    &home,
    "s-count",
    "codex",
    Some(("codex-main", Some("vendor/gpt-count"))),
    &events(&[
      r#"{"type": "result", "timestamp": "2026-09-11T20:00:00+00:00", "usage": {"input_tokens": 1.5, "cache_creation_input_tokens": [1], "cache_read_input_tokens": 3, "output_tokens": true}}"#,
      r#"{"session_id": "codex-count"}"#,
    ]),
  );
  write_thread(
    &home,
    "s-count",
    "native",
    Some(("charlie-code-native", Some("vendor/native-count"))),
    &events(&[
      r#"{"type": "result", "timestamp": "2026-09-11T20:00:00+00:00", "usage": {"input_tokens": 1.5, "cache_creation_input_tokens": [1], "cache_read_input_tokens": {"a": 1}, "output_tokens": 4}}"#,
    ]),
  );
  write_run_json(
    &home,
    "run-session",
    "run-claude",
    r#"{"backend": "claude-main", "model": "vendor/claude-run-count", "started_at": "2026-09-11T20:00:00+00:00", "native_session_id": "claude-run-count"}"#,
    &events(&[
      r#"{"type": "result", "usage": {"input_tokens": 1.5, "cache_creation_input_tokens": false, "cache_read_input_tokens": 0.5, "output_tokens": true}}"#,
    ]),
  );
  write_run_json(
    &home,
    "run-session",
    "run-clc",
    r#"{"backend": "charlie-code-native", "model": "vendor/native-run-count", "started_at": "2026-09-11T20:00:00+00:00"}"#,
    &events(&[r#"{"type": "result", "usage": {"input_tokens": true, "cached_tokens": 1.5, "output_tokens": 4}}"#]),
  );
  write_run_json(
    &home,
    "run-session",
    "run-codex",
    r#"{"backend": "codex-main", "model": "vendor/gpt-run-count", "started_at": "2026-09-11T20:00:00+00:00"}"#,
    &events(&[
      r#"{"type": "turn.completed", "usage": {"input_tokens": 1.5, "cached_input_tokens": 0.5, "output_tokens": true}}"#,
      r#"{"type": "thread.started", "thread_id": "codex-count-run"}"#,
    ]),
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 7);
  assert_eq!(
    stored_counts(&ledger, "thread:s-count/claude/0"),
    [SqlValue::Real(1.5), SqlValue::Integer(1), SqlValue::Integer(3), SqlValue::Integer(0), SqlValue::Integer(4)]
  );
  assert_eq!(
    stored_counts(&ledger, "thread:s-count/claude/1"),
    [SqlValue::Integer(1), SqlValue::Integer(2), SqlValue::Integer(3), SqlValue::Integer(0), SqlValue::Real(1.5)]
  );
  assert_eq!(
    stored_counts(&ledger, "thread:s-count/codex/0"),
    [SqlValue::Real(-1.5), SqlValue::Integer(0), SqlValue::Integer(3), SqlValue::Integer(0), SqlValue::Integer(1)]
  );
  assert_eq!(
    stored_counts(&ledger, "thread:s-count/native/0"),
    [SqlValue::Integer(0), SqlValue::Integer(0), SqlValue::Integer(0), SqlValue::Real(1.5), SqlValue::Integer(4)]
  );
  assert_eq!(
    stored_counts(&ledger, "run:run-claude"),
    [SqlValue::Real(1.5), SqlValue::Integer(0), SqlValue::Real(0.5), SqlValue::Integer(0), SqlValue::Integer(1)]
  );
  assert_eq!(
    stored_counts(&ledger, "run:run-clc"),
    [SqlValue::Real(-0.5), SqlValue::Integer(0), SqlValue::Real(1.5), SqlValue::Integer(0), SqlValue::Integer(4)]
  );
  assert_eq!(
    stored_counts(&ledger, "run:run-codex"),
    [SqlValue::Integer(1), SqlValue::Integer(0), SqlValue::Real(0.5), SqlValue::Integer(0), SqlValue::Integer(1)]
  );
  let rows = charlie_rows(&mut ledger);
  let claude = row(&rows, "charlie-bot", "claude-model");
  assert_eq!(
    (claude.calls, claude.in_fresh, claude.cache_write, claude.cache_read, claude.output, claude.total),
    (2, Count::Real(2.5), Count::Int(3), Count::Int(6), Count::Real(5.5), Count::Real(17.0))
  );
  // A list or map under a key the verdict stores or subtracts fails the bind or the arithmetic, so the capture raises.
  let raising = [
    r#"{"type": "result", "usage": {"input_tokens": 1, "output_tokens": [1]}}"#,
    r#"{"type": "result", "usage": {"input_tokens": {"a": 1}}}"#,
  ];
  for line in raising {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let (config, registry) = fixture(&home);
    let session = r#"{"session_id": "raise-kept"}"#;
    write_thread(&home, "s-raise", "t", Some(("claude-main", Some("vendor/claude-model"))), &events(&[line, session]));
    let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
    assert!(capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &registry).is_err());
  }
}

/// The stored timestamp of one row, as the TEXT column holds it.
fn stored_ts(ledger: &UsageLedger, record_id: &str) -> SqlValue {
  ledger.connection().query_row("SELECT ts FROM usage WHERE record_id = ?", [record_id], |row| row.get(0)).unwrap()
}

#[test]
fn malformed_timestamp_shapes_capture_the_recorded_rows() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  write_thread(
    &home,
    "s-ts",
    "stamps",
    Some(("claude-main", Some("vendor/claude-model"))),
    &events(&[
      r#"{"type": "result", "timestamp": 7, "usage": {"input_tokens": 10, "output_tokens": 4}}"#,
      r#"{"type": "result", "timestamp": 1.5, "usage": {"input_tokens": 10, "output_tokens": 4}}"#,
      r#"{"type": "result", "timestamp": true, "usage": {"input_tokens": 10, "output_tokens": 4}}"#,
      r#"{"type": "result", "timestamp": 2.0, "usage": {"input_tokens": 10, "output_tokens": 4}}"#,
      r#"{"type": "result", "timestamp": 0, "usage": {"input_tokens": 10, "output_tokens": 4}}"#,
      r#"{"type": "result", "timestamp": false, "usage": {"input_tokens": 10, "output_tokens": 4}}"#,
      r#"{"session_id": "ts-kept"}"#,
    ]),
  );
  let run_usage = events(&[
    r#"{"session_id": "claude-run-ts"}"#,
    r#"{"type": "result", "usage": {"input_tokens": 13, "output_tokens": 6}}"#,
  ]);
  let clc_usage =
    events(&[r#"{"type": "result", "usage": {"input_tokens": 18, "cached_tokens": 4, "output_tokens": 7}}"#]);
  write_run_json(
    &home,
    "run-session",
    "run-int",
    r#"{"backend": "claude-main", "model": "vendor/claude-run-ts", "started_at": 7}"#,
    &run_usage,
  );
  write_run_json(
    &home,
    "run-session",
    "run-float",
    r#"{"backend": "claude-main", "model": "vendor/claude-run-ts", "started_at": 1.5}"#,
    &run_usage,
  );
  write_run_json(
    &home,
    "run-session",
    "run-bool",
    r#"{"backend": "charlie-code-native", "model": "vendor/native-run-ts", "started_at": true}"#,
    &clc_usage,
  );
  write_run_json(
    &home,
    "run-session",
    "run-zero",
    r#"{"backend": "charlie-code-native", "model": "vendor/native-run-ts", "started_at": 0}"#,
    &clc_usage,
  );
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 10);
  assert_eq!(stored_ts(&ledger, "thread:s-ts/stamps/0"), SqlValue::Text("7".to_string()));
  assert_eq!(stored_ts(&ledger, "thread:s-ts/stamps/1"), SqlValue::Text("1.5".to_string()));
  assert_eq!(stored_ts(&ledger, "thread:s-ts/stamps/2"), SqlValue::Text("1".to_string()));
  assert_eq!(stored_ts(&ledger, "thread:s-ts/stamps/3"), SqlValue::Text("2.0".to_string()));
  assert_eq!(stored_ts(&ledger, "thread:s-ts/stamps/4"), SqlValue::Text(String::new()));
  assert_eq!(stored_ts(&ledger, "thread:s-ts/stamps/5"), SqlValue::Text(String::new()));
  assert_eq!(stored_ts(&ledger, "run:run-int"), SqlValue::Text("7".to_string()));
  assert_eq!(stored_ts(&ledger, "run:run-float"), SqlValue::Text("1.5".to_string()));
  assert_eq!(stored_ts(&ledger, "run:run-bool"), SqlValue::Text("1".to_string()));
  assert_eq!(stored_ts(&ledger, "run:run-zero"), SqlValue::Text(String::new()));
  // A list or map timestamp fails Python's bind, so the capture raises.
  let raising = [
    r#"{"type": "result", "timestamp": [1], "usage": {"input_tokens": 1}}"#,
    r#"{"type": "result", "timestamp": {"a": 1}, "usage": {"input_tokens": 1}}"#,
  ];
  for line in raising {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let (config, registry) = fixture(&home);
    let session = r#"{"session_id": "raise-kept"}"#;
    write_thread(&home, "s-raise", "t", Some(("claude-main", Some("vendor/claude-model"))), &events(&[line, session]));
    let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
    assert!(capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &registry).is_err());
  }

  // Run metadata uses the same `... or ""` bind as a thread timestamp.
  let raising_started_at = [
    r#"{"backend": "claude-main", "model": "vendor/claude-run-ts", "started_at": [1]}"#,
    r#"{"backend": "claude-main", "model": "vendor/claude-run-ts", "started_at": {"a": 1}}"#,
  ];
  for (index, metadata) in raising_started_at.into_iter().enumerate() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let (config, registry) = fixture(&home);
    write_run_json(&home, "run-session", &format!("run-bad-ts-{index}"), metadata, &run_usage);
    let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
    assert!(capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &registry).is_err());
  }
}

const MASTER_RESULT: &str =
  r#"{"type": "result", "usage": {"input_tokens": 20, "cached_tokens": 4, "output_tokens": 7}}"#;

#[test]
fn malformed_master_model_shapes_capture_the_recorded_rows() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
  write_master(
    &home,
    "s-model",
    "2026-09-01T00:00:00Z",
    &events(&[
      r#"{"type": "context", "model": 7}"#,
      r#"{"type": "context", "model": 1.5}"#,
      r#"{"type": "context", "model": true}"#,
      r#"{"type": "context", "model": [1]}"#,
      r#"{"type": "context", "model": {"a": 1}}"#,
      r#"{"type": "context", "model": "vendor/native-model"}"#,
      MASTER_RESULT,
    ]),
  );
  write_master(&home, "s-model", "2026-09-02T00:00:00Z", &events(&[r#"{"type": "context", "model": 7}"#]));
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert_eq!(count_of(&capture_round(&mut ledger, &config, &registry), "charlie-bot"), 1);
  let rows = charlie_rows(&mut ledger);
  let native = row(&rows, "charlie-bot", "native-model");
  assert_eq!(
    (native.calls, native.in_fresh, native.cache_read, native.output, native.total),
    (1, Count::Int(16), Count::Int(4), Count::Int(7), Count::Int(27))
  );
  assert_eq!((native.first.as_str(), native.last.as_str()), ("2026-09-01", "2026-09-01"));
  assert_eq!(native.accounts.iter().map(|account| account.name.as_str()).collect::<Vec<_>>(), ["charlie-code-native"]);
  // A non-string model that is the last truthy one when the row is built fails Python's rsplit, so the capture raises.
  let raising: [[&str; 3]; 2] = [
    [r#"{"type": "context", "model": "vendor/native-model"}"#, r#"{"type": "context", "model": 7}"#, MASTER_RESULT],
    [r#"{"type": "context", "model": 7}"#, r#"{"type": "context", "model": ""}"#, MASTER_RESULT],
  ];
  for lines in raising {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let (config, registry) = fixture(&home);
    write_master(&home, "s-model", "2026-09-03T00:00:00Z", &events(&lines));
    let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
    assert!(capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &registry).is_err());
  }
}

const TS_A: &str = "2026-01-10T08:00:00+00:00";
