use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use charliebot_core::config::{self, CharlieBotConfig, ConfigSchema};
use charliebot_registry::usage::{RecordKind, UsageOps, UsageRecord, UsageSource, UsageSourceImpl};
use charliebot_registry::{Registry, RegistryError};

const HOST: &str = "fixture-host";
static EXTERNAL_READS: AtomicUsize = AtomicUsize::new(0);

struct FixtureSource {
  external: bool,
  codex: bool,
}

impl UsageSourceImpl for FixtureSource {
  fn ops(&self) -> UsageOps {
    UsageOps { logs: true, read: true, live_cli_sessions: self.codex, ..UsageOps::default() }
  }

  fn logs(&self, config: &CharlieBotConfig) -> anyhow::Result<Vec<(PathBuf, String)>> {
    if !self.external {
      return Ok(Vec::new());
    }
    let path = config.charliebot_home.join("cli/external.log");
    Ok(path.exists().then_some((path, "fixture-account".into())).into_iter().collect())
  }

  fn read(&self, path: &Path, _account: &str, _previous: Option<&str>) -> anyhow::Result<(String, Vec<UsageRecord>)> {
    if !self.external {
      bail!("fixture source has no log files: {}", path.display());
    }
    EXTERNAL_READS.fetch_add(1, Ordering::SeqCst);
    let metadata = fs::metadata(path)?;
    let signature = format!(
      "{}:{}",
      i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()),
      metadata.len()
    );
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
    if !self.codex {
      bail!("fixture source does not declare live_cli_sessions");
    }
    Ok(HashSet::from(["codex-live".to_string(), "live-run".to_string()]))
  }
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
    implementation: Some(Arc::new(FixtureSource { external: true, codex: false })),
  })?;
  registry.usage_source(UsageSource {
    name: "Codex",
    id_prefixes: &["codex-"],
    run_logs_only: false,
    implementation: Some(Arc::new(FixtureSource { external: false, codex: true })),
  })?;
  registry.attribute_backend_type("codex", "Codex")?;
  registry.usage_source(UsageSource {
    name: "Claude Code",
    id_prefixes: &["claude-"],
    run_logs_only: false,
    implementation: Some(Arc::new(FixtureSource { external: false, codex: false })),
  })?;
  registry.attribute_backend_type("cc-claude", "Claude Code")?;
  Ok(())
}

fn fixture(home: &Path) -> (CharlieBotConfig, Registry) {
  fs::create_dir_all(home).unwrap();
  fs::write(
    home.join("config.yaml"),
    r#"backends:
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
"#,
  )
  .unwrap();
  let schema = ConfigSchema { backend_types: vec!["charlie-code", "codex", "cc-claude"], ..ConfigSchema::default() };
  let config = config::load_config(home, &schema).unwrap();
  let mut registry = Registry::new();
  registry.register_package("usage-fixture", register_sources).unwrap();
  (config, registry)
}

fn write_thread(
  home: &Path,
  session: &str,
  thread: &str,
  metadata: Option<(&str, Option<&str>)>,
  events: &[u8],
) -> PathBuf {
  let directory = home.join("sessions").join(session).join("threads").join(thread);
  fs::create_dir_all(directory.join("data")).unwrap();
  if let Some((backend, model)) = metadata {
    let model = model.map_or(String::new(), |model| format!(", \"model\": \"{model}\""));
    fs::write(directory.join("metadata.json"), format!("{{\"backend\": \"{backend}\"{model}}}")).unwrap();
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
  let directory = home.join("sessions").join(session).join("data/runs").join(id);
  fs::create_dir_all(&directory).unwrap();
  let model = model.map_or(String::new(), |model| format!(", \"model\": \"{model}\""));
  let native = native_session_id.map_or(String::new(), |id| format!(", \"native_session_id\": \"{id}\""));
  fs::write(
    directory.join("metadata.json"),
    format!("{{\"backend\": \"{backend}\", \"started_at\": \"{TS_A}\"{model}{native}}}"),
  )
  .unwrap();
  let path = directory.join("agent.raw.ndjson");
  fs::write(&path, events).unwrap();
  path
}

fn result_line(timestamp: &str, input: i64, cache_write: i64, cache_read: i64, output: i64) -> String {
  format!(
    "{{\"type\": \"result\", \"timestamp\": \"{timestamp}\", \"usage\": {{\"input_tokens\": {input}, \"cache_creation_input_tokens\": {cache_write}, \"cache_read_input_tokens\": {cache_read}, \"output_tokens\": {output}}}}}\n"
  )
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
    [("External".into(), 1), ("Codex".into(), 0), ("Claude Code".into(), 0), ("charlie-bot".into(), 0)]
  );
  assert_eq!(EXTERNAL_READS.load(Ordering::SeqCst), 1);
  let second = capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &registry).unwrap();
  assert_eq!(
    second,
    [("External".into(), 0), ("Codex".into(), 0), ("Claude Code".into(), 0), ("charlie-bot".into(), 0)]
  );
  assert_eq!(EXTERNAL_READS.load(Ordering::SeqCst), 1);
  assert!(ledger.last_capture_at(HOST).unwrap().is_some());
}

#[test]
fn charliebot_threads_masters_and_runs_match_their_verdicts_and_marker_rules() {
  let dir = tempfile::tempdir().unwrap();
  let home = dir.path().join("home");
  let (config, registry) = fixture(&home);
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
  write_run(&home, "run-session", "run-codex-no-id", "codex-main", None, None,
    b"{\"type\": \"turn.completed\", \"usage\": {\"input_tokens\": 8, \"cached_input_tokens\": 2, \"output_tokens\": 1}}\n");
  write_run(&home, "run-session", "run-claude", "claude-main", None, Some("claude-run"),
    b"{\"session_id\": \"claude-run\"}\n{\"type\": \"result\", \"usage\": {\"input_tokens\": 13, \"cache_creation_input_tokens\": 3, \"cache_read_input_tokens\": 2, \"output_tokens\": 6}}\n");
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
  let counts = capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &registry).unwrap();
  assert_eq!(
    counts.iter().map(|(source, _)| source.as_str()).collect::<Vec<_>>(),
    ["External", "Codex", "Claude Code", "charlie-bot"]
  );
  assert_eq!(counts[3].1, 10);
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
  let second = capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &registry).unwrap();
  assert!(second.iter().all(|(_, count)| *count == 0));
  assert!(ledger.last_capture_at(HOST).unwrap().is_some());

  fs::write(&native_run_path, format!("{native_run}\n{}", result_line(TS_A, 19, 0, 0, 8))).unwrap();
  let appended = capture_usage(&mut ledger, HOST, &config.sessions_dir(), &config, &registry).unwrap();
  assert_eq!(appended[3], ("charlie-bot".into(), 1));
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
fn missing_usage_registration_fails_before_stamping() {
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

const TS_A: &str = "2026-01-10T08:00:00+00:00";
