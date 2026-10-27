//! The cases of `tests/test_storage_cool.py` that the real run reaches. Each test runs the sweep over a temporary tree whose
//! Claude, Codex and opencode stores are registered through their own `register_with_host`, so the sources' sweeps are
//! the ported ones.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use charliebot_claude_code as claude_code;
use charliebot_claude_code::ClaudeHost;
use charliebot_claude_code::usage_quota::QuotaHttp;
use charliebot_codex as codex;
use charliebot_codex::CodexHost;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::paths::realpath;
use charliebot_opencode::usage_logs::OpencodeLogs;
use charliebot_registry::usage::{CategoryResult, UsageSource};
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::UtcDateTime;
use charliebot_store::run_records::{CURSOR_NAME, DATA_DIR_NAME, RAW_LOG_NAME, STDERR_LOG_NAME};
use serde_json::{Map, Value, json};

use super::*;
use crate::{UsageLedger, default_ledger_path};

/// 2026-09-04T12:00:00Z, the instant every age in these tests is measured from.
const NOW: &str = "2026-09-04T12:00:00+00:00";
const NOW_UNIX: u64 = 1_788_523_200;
/// Thirty days before NOW: cold once archived.
const OLD: &str = "2026-08-05T12:00:00+00:00";
/// One day before NOW: idle, so never cold.
const RECENT: &str = "2026-09-03T12:00:00+00:00";

const SID_COLD: &str = "11111111-2222-4333-8444-555555555555";
const SID_WARM: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const SID_DEAD: &str = "99999999-8888-4777-8666-555555555555";
const TID: &str = "f0e1d2c3-a4b5-4c6d-8e7f-0a1b2c3d4e5f";
const CODEX_COLD: &str = "0f0e1d2c-3b4a-4c5d-8e9f-0a1b2c3d4e5f";
const CC_OPENCOLD: &str = "ses_coldbackend0000000000000000000000";

/// A master run's raw log: one model line and one usage line (the charlie-code shape the capture reads). Its output
/// count, 7, is what the ledger must hold after the sweep deletes the file.
const MASTER_USAGE: &str = concat!(
  r#"{"type": "context", "model": "vendor/native-model"}"#,
  "\n",
  r#"{"type": "result", "usage": {"input_tokens": 20, "cached_tokens": 4, "output_tokens": 7}}"#,
  "\n",
);

const OPENCODE_SCHEMA: &str = "
  CREATE TABLE event_sequence (aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, owner_id TEXT);
  CREATE TABLE event (id TEXT PRIMARY KEY, aggregate_id TEXT NOT NULL, seq INTEGER NOT NULL, type TEXT NOT NULL,
    data TEXT NOT NULL);
  CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
";

thread_local! {
  /// The hosts the next `register_test_sources` call registers. A test sets them on its own thread before it builds its
  /// registry, so each test's sources read its own tree.
  static TREE_HOSTS: RefCell<Option<TreeHosts>> = const { RefCell::new(None) };
}

struct TreeHosts {
  claude: ClaudeHost,
  codex: CodexHost,
  opencode_db: PathBuf,
}

fn register_test_sources(registry: &mut Registry) -> Result<(), RegistryError> {
  let hosts = TREE_HOSTS.with(|slot| slot.borrow_mut().take()).expect("the test sets its hosts before it registers");
  let http = QuotaHttp::from_process().expect("the test HTTP client builds");
  claude_code::register_with_host(registry, hosts.claude, http)?;
  codex::register_with_host(registry, &hosts.codex)?;
  registry.usage_source(UsageSource {
    name: "opencode",
    id_prefixes: &["opencode-", "oc-"],
    run_logs_only: false,
    implementation: Some(Arc::new(OpencodeLogs::new(hosts.opencode_db, PathBuf::from("/proc")))),
  })
}

/// A synthetic home under a temporary tree: `charlie/` is the CharlieBot home, `home/` is HOME (Codex's `.codex` and
/// Claude's `.claude` sit under it), and `claude-config/` is the Claude config directory.
struct Fixture {
  tree: tempfile::TempDir,
  cfg: Arc<CharlieBotConfig>,
  registry: Arc<Registry>,
}

impl Fixture {
  fn new() -> Self {
    let tree = tempfile::tempdir().expect("the temporary tree");
    let charlie = tree.path().join("charlie");
    let home = tree.path().join("home");
    let claude_config = tree.path().join("claude-config");
    let worktrees = tree.path().join("worktrees");
    let opencode_db = charlie.join("opencode/data/opencode/opencode.db");
    fs::create_dir_all(&charlie).expect("the CharlieBot home");
    fs::write(charlie.join("config.yaml"), format!("paths:\n  worktree_dir: '{}'\n", worktrees.display()))
      .expect("the synthetic config");
    // The sandbox guard: every path a sweep reads resolves inside the temporary tree, checked before any sweep runs.
    for path in [&charlie, &home, &claude_config, &worktrees, &home.join(".codex"), &opencode_db] {
      assert_inside(tree.path(), path);
    }
    TREE_HOSTS.with(|slot| {
      *slot.borrow_mut() = Some(TreeHosts {
        claude: ClaudeHost { home: home.clone(), claude_config_dir: claude_config.clone() },
        codex: CodexHost { path: "/usr/bin:/bin".to_string(), home: home.clone() },
        opencode_db,
      })
    });
    let mut registry = Registry::new();
    registry.register_package("usage-cool-storage-tests", register_test_sources).expect("the test sources register");
    let cfg = registry.load_config(&charlie).expect("the synthetic config loads");
    Self { tree, cfg: Arc::new(cfg), registry: Arc::new(registry) }
  }

  fn sessions(&self) -> PathBuf {
    self.cfg.sessions_dir()
  }

  fn claude_projects(&self) -> PathBuf {
    self.tree.path().join("home/.claude/projects")
  }

  fn codex_sessions(&self) -> PathBuf {
    self.tree.path().join("home/.codex/sessions")
  }

  fn opencode_db(&self) -> PathBuf {
    self.cfg.charliebot_home.join("opencode/data/opencode/opencode.db")
  }

  fn ledger(&self) -> PathBuf {
    default_ledger_path(&self.cfg.charliebot_home)
  }

  /// Writes `sessions/<sid>/metadata.json` and returns the session directory.
  fn write_session(&self, sid: &str, meta: Value) -> PathBuf {
    let dir = self.sessions().join(sid);
    put(&dir.join("metadata.json"), meta.to_string().as_bytes(), None);
    dir
  }

  /// A rollout file in the Codex tree, named as Codex names it: the thread id ends the stem.
  fn rollout(&self, thread_id: &str, mtime: Option<SystemTime>) -> PathBuf {
    let name = format!("rollout-2026-08-01T00-00-00-{thread_id}.jsonl");
    put(&self.codex_sessions().join("2026/09/04").join(name), b"rollout", mtime)
  }

  fn sweep(&self) -> anyhow::Result<Vec<CategoryResult>> {
    run_cool_sweep(Arc::clone(&self.cfg), &self.registry, now())
  }
}

fn now() -> UtcDateTime {
  UtcDateTime::parse(NOW).expect("NOW parses")
}

/// The instant `seconds` before NOW, for a file's mtime.
fn ago(seconds: u64) -> SystemTime {
  SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_UNIX - seconds)
}

const fn days(count: u64) -> u64 {
  count * 86_400
}

/// Asserts that `path` resolves inside `tree` (the sandbox guard).
fn assert_inside(tree: &Path, path: &Path) {
  assert!(realpath(path).starts_with(realpath(tree)), "{} resolves outside the synthetic tree", path.display());
}

/// Writes a file, creating its parents, and sets its mtime when one is given.
fn put(path: &Path, bytes: &[u8], mtime: Option<SystemTime>) -> PathBuf {
  fs::create_dir_all(path.parent().expect("a file has a parent")).expect("the parent directory");
  fs::write(path, bytes).expect("the file");
  if let Some(mtime) = mtime {
    File::options().write(true).open(path).expect("the file reopens").set_modified(mtime).expect("the mtime is set");
  }
  path.to_path_buf()
}

fn meta(value: Value) -> Map<String, Value> {
  value.as_object().cloned().expect("a JSON object")
}

fn cold_meta() -> Value {
  json!({"status": "archived", "updated_at": OLD})
}

fn live_meta() -> Value {
  json!({"status": "active", "updated_at": RECENT})
}

fn thread_data(session: &Path) -> PathBuf {
  session.join("threads").join(TID).join(DATA_DIR_NAME)
}

fn master_run(session: &Path, started_at: &str) -> PathBuf {
  session.join(DATA_DIR_NAME).join("master_runs").join(started_at)
}

/// The transcript directory name Claude Code derives from a process cwd: separators, dots and underscores are hyphens.
fn encoded(path: &Path) -> String {
  path.to_string_lossy().replace(['/', '.', '_'], "-")
}

fn category<'a>(categories: &'a [CategoryResult], name: &str) -> &'a CategoryResult {
  categories.iter().find(|category| category.name == name).unwrap_or_else(|| panic!("no {name} category"))
}

/// Every file under `root`, keyed by its path relative to `root`, with its bytes.
fn tree_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
  let mut files = BTreeMap::new();
  collect(root, root, &mut files);
  files
}

fn collect(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
  if !dir.exists() {
    return;
  }
  for entry in fs::read_dir(dir).expect("the directory lists") {
    let path = entry.expect("the entry reads").path();
    if path.is_dir() {
      collect(root, &path, files);
    } else {
      files.insert(path.strip_prefix(root).expect("under the root").to_path_buf(), fs::read(&path).expect("the file"));
    }
  }
}

/// A transcript directory holding one transcript, the shape Claude Code leaves per cwd.
fn claude_dir(projects: &Path, name: &str, mtime: Option<SystemTime>) -> PathBuf {
  let dir = projects.join(name);
  put(&dir.join("transcript.jsonl"), b"claude-transcript", mtime);
  dir
}

/// An opencode store with one assistant message for each session id. Its sweep never deletes a row.
fn make_opencode_db(path: &Path, session_ids: &[&str]) {
  fs::create_dir_all(path.parent().expect("the store has a directory")).expect("the store directory");
  let connection = rusqlite::Connection::open(path).expect("the store opens");
  connection.execute_batch(OPENCODE_SCHEMA).expect("the schema is created");
  for (index, session_id) in session_ids.iter().enumerate() {
    let data = json!({
      "model": {"id": "vendor/opencode-model", "providerID": "opencode-account"},
      "tokens": {"input": 30, "output": 5, "cache": {"read": 2, "write": 1}},
      "time": {"created": 1_785_000_000_000_i64},
    });
    connection
      .execute(
        "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
         VALUES (?1, ?2, 'assistant', 0, ?3, ?3, ?4)",
        rusqlite::params![format!("msg-{index}"), session_id, 1_785_000_000_000_i64, data.to_string()],
      )
      .expect("the message is inserted");
  }
}

#[test]
fn cold_rule_requires_archived_status_and_idle_age() {
  let now = now();
  assert!(is_cold_session(&meta(json!({"status": "archived", "updated_at": OLD})), now));
  assert!(!is_cold_session(&meta(json!({"status": "archived", "updated_at": RECENT})), now));
  assert!(!is_cold_session(&meta(json!({"status": "active", "updated_at": OLD})), now));
  // Missing or unreadable metadata fields never qualify.
  assert!(!is_cold_session(&meta(json!({"status": "archived"})), now));
  assert!(!is_cold_session(&meta(json!({"status": "archived", "updated_at": "not-a-date"})), now));
  assert!(!is_cold_session(&meta(json!({})), now));
}

#[test]
fn cold_rule_counts_fourteen_idle_days_and_no_fewer() {
  let now = now();
  let fourteen_days_idle = json!({"status": "archived", "updated_at": "2026-08-21T12:00:00+00:00"});
  let one_second_short = json!({"status": "archived", "updated_at": "2026-08-21T12:00:01+00:00"});
  assert!(is_cold_session(&meta(fourteen_days_idle), now));
  assert!(!is_cold_session(&meta(one_second_short), now));
}

#[test]
fn metadataless_session_dir_never_qualifies() {
  let fixture = Fixture::new();
  let orphan = fixture.sessions().join(SID_DEAD);
  let transport = put(&thread_data(&orphan).join("stdout.log"), b"x", None);

  let categories = fixture.sweep().expect("the sweep runs");

  assert!(transport.exists());
  assert_eq!(category(&categories, "raw-transport").count, 0);
}

#[test]
fn claude_dirs_delete_for_cold_sessions_and_keep_live_ones() {
  let fixture = Fixture::new();
  let cold_session = fixture.write_session(SID_COLD, cold_meta());
  fixture.write_session(SID_WARM, live_meta());
  let projects = fixture.claude_projects();
  let cold = claude_dir(&projects, &encoded(&cold_session), None);
  let cold_thread = claude_dir(&projects, &format!("{}-threads-{TID}", encoded(&cold_session)), None);
  let live = claude_dir(&projects, &encoded(&fixture.sessions().join(SID_WARM)), None);

  let categories = fixture.sweep().expect("the sweep runs");

  assert!(!cold.exists());
  assert!(!cold_thread.exists());
  assert!(live.exists());
  let claude = category(&categories, "claude-transcripts");
  assert_eq!(claude.count, 2);
  assert_eq!(claude.bytes, 2 * b"claude-transcript".len() as u64);
}

#[test]
fn claude_user_cwd_dirs_never_touched() {
  let fixture = Fixture::new();
  let projects = fixture.claude_projects();
  let user_dirs = [
    claude_dir(&projects, "-home-dev", None),
    claude_dir(&projects, "-home-dev-workspace-charlie-bot", None),
    claude_dir(&projects, "-tmp-cb-e2e-home-sessions", None),
  ];

  fixture.sweep().expect("the sweep runs");

  for dir in user_dirs {
    assert!(dir.exists(), "{} was deleted", dir.display());
  }
}

#[test]
fn every_backend_sweeps_through_its_source_in_registration_order() {
  let fixture = Fixture::new();
  let cold = fixture.write_session(SID_COLD, cold_meta_referencing(CC_OPENCOLD));
  put(&thread_data(&cold).join("stdout.log"), b"transport", None);
  put(&master_run(&cold, "2026-08-01T00:00:00+00:00").join(RAW_LOG_NAME), b"raw", None);
  claude_dir(&fixture.claude_projects(), &encoded(&cold), None);
  fixture.rollout(CODEX_COLD, Some(ago(days(30))));
  make_opencode_db(&fixture.opencode_db(), &[CC_OPENCOLD]);

  let categories = fixture.sweep().expect("the sweep runs");

  assert_eq!(
    categories.iter().map(|category| (category.name, category.count)).collect::<Vec<_>>(),
    [("raw-transport", 2), ("claude-transcripts", 1), ("codex-rollouts", 1)]
  );
}

fn cold_meta_referencing(backend_session: &str) -> Value {
  json!({"status": "archived", "updated_at": OLD, "cc_session_id": backend_session})
}

#[test]
fn sweep_leaves_the_opencode_store_bytes_unchanged() {
  let fixture = Fixture::new();
  fixture.write_session(SID_COLD, cold_meta_referencing(CC_OPENCOLD));
  make_opencode_db(&fixture.opencode_db(), &[CC_OPENCOLD, "ses_orphan000000000000000000000000"]);
  let before = fs::read(fixture.opencode_db()).expect("the store reads");

  fixture.sweep().expect("the sweep runs");

  assert_eq!(fs::read(fixture.opencode_db()).expect("the store reads"), before);
}

#[test]
fn capture_failure_aborts_before_any_deletion() {
  let fixture = Fixture::new();
  let cold = fixture.write_session(SID_COLD, cold_meta());
  let transport = put(&thread_data(&cold).join("stdout.log"), b"transport", None);
  claude_dir(&fixture.claude_projects(), &encoded(&cold), None);
  // A file where the ledger's directory belongs makes the capture fail before the sweep reads any source.
  put(&fixture.cfg.charliebot_home.join("usage"), b"not a directory", None);
  let before = tree_bytes(fixture.tree.path());

  assert!(fixture.sweep().is_err(), "the failed capture must fail the sweep");

  assert!(transport.exists());
  assert_eq!(tree_bytes(fixture.tree.path()), before);
}

#[test]
fn capture_reads_a_transport_file_before_the_sweep_deletes_it() {
  let fixture = Fixture::new();
  let cold = fixture.write_session(SID_COLD, cold_meta());
  let raw = put(&master_run(&cold, "2026-08-01T00:00:00+00:00").join(RAW_LOG_NAME), MASTER_USAGE.as_bytes(), None);

  fixture.sweep().expect("the sweep runs");

  assert!(!raw.exists(), "the cold transport file must be deleted");
  let mut ledger = UsageLedger::open(fixture.ledger()).expect("the ledger reopens");
  let output: i64 = ledger.model_rows().expect("the ledger rows read").iter().map(|row| row.output.int()).sum();
  assert_eq!(output, 7, "the usage in the deleted file must be captured");
}

#[test]
fn run_reference_to_outside_path_is_ignored_not_created() {
  let fixture = Fixture::new();
  let cold = fixture.write_session(SID_COLD, json!({"status": "archived", "updated_at": OLD, "schema_version": 2}));
  let transport = put(&master_run(&cold, "2026-08-01T00:00:00+00:00").join(RAW_LOG_NAME), b"x", Some(ago(days(30))));
  let run_meta = json!({
    "id": "run-outside",
    "session_id": SID_COLD,
    "kind": "work",
    "raw_log_ref": "/nonexistent/outside/agent.raw.ndjson",
  });
  put(&cold.join("data/runs/run-outside/metadata.json"), run_meta.to_string().as_bytes(), None);

  fixture.sweep().expect("the sweep runs");

  assert!(!transport.exists());
  assert!(!Path::new("/nonexistent/outside/agent.raw.ndjson").exists());
}

#[test]
fn run_evidence_inside_a_managed_directory_survives_the_sweep() {
  let fixture = Fixture::new();
  let cold = fixture.write_session(SID_COLD, cold_meta());
  let evidence = put(&master_run(&cold, "2026-08-02T00:00:00+00:00").join(RAW_LOG_NAME), b"evidence", None);
  let sibling = put(&master_run(&cold, "2026-08-01T00:00:00+00:00").join(RAW_LOG_NAME), b"sibling", None);
  let run_meta = json!({"id": "run-migrated", "raw_log_ref": evidence.display().to_string()});
  put(&cold.join("data/runs/run-migrated/metadata.json"), run_meta.to_string().as_bytes(), None);

  fixture.sweep().expect("the sweep runs");

  assert!(evidence.exists(), "a file a run record references must stay");
  assert!(!sibling.exists(), "an unreferenced sibling is a plain transport file");
}

#[test]
fn transport_rule_deletes_reserved_names_and_rotations_inside_managed_dirs_only() {
  let fixture = Fixture::new();
  let cold = fixture.write_session(SID_COLD, cold_meta());
  let master = master_run(&cold, "2026-08-01T00:00:00+00:00");
  let thread = thread_data(&cold);
  let deleted = [
    master.join(RAW_LOG_NAME),
    master.join(STDERR_LOG_NAME),
    master.join(CURSOR_NAME),
    master.join("agent.raw.ndjson.1"),
    master.join("stdout.log.2"),
    thread.join("stdout.log"),
    thread.join("stderr.log"),
    thread.join("agent.raw.ndjson.2"),
  ];
  let kept = [
    master.join("events.jsonl"),
    master.join("stdout.log.x"),
    master.join("nested").join(RAW_LOG_NAME),
    thread.join("events.jsonl"),
    thread.join("stdout.log.x"),
    cold.join(DATA_DIR_NAME).join(RAW_LOG_NAME),
    cold.join("threads").join(TID).join(RAW_LOG_NAME),
    cold.join("uploads").join("stdout.log"),
    cold.join("uploads").join("agent.raw.ndjson.1"),
  ];
  for path in deleted.iter().chain(kept.iter()) {
    put(path, b"x", Some(ago(days(30))));
  }

  let categories = fixture.sweep().expect("the sweep runs");

  for path in &deleted {
    assert!(!path.exists(), "{} survived", path.display());
  }
  for path in &kept {
    assert!(path.exists(), "{} was deleted", path.display());
  }
  assert_eq!(category(&categories, "raw-transport").count, deleted.len() as u64);
}

#[test]
fn summary_line_lists_each_category_then_the_total_in_gib() {
  let categories = [
    CategoryResult { name: "raw-transport", unit: "files", count: 2, bytes: 1 << 30 },
    CategoryResult { name: "claude-transcripts", unit: "dirs", count: 1, bytes: 1 << 29 },
  ];

  assert_eq!(
    format_sweep_line(&categories),
    "total 1.50 GiB (raw-transport 2 files 1.00 GiB; claude-transcripts 1 dirs 0.50 GiB)"
  );
}

#[test]
fn cold_rule_reads_updated_at_and_status_as_python_does() {
  let now = now();
  // `models.parse_utc_datetime` reads a date-only or naive value as UTC, and `str()` of an integer gives its digits.
  assert!(is_cold_session(&meta(json!({"status": "archived", "updated_at": "2026-08-05"})), now));
  assert!(is_cold_session(&meta(json!({"status": "archived", "updated_at": "2026-08-05T12:00:00"})), now));
  assert!(is_cold_session(&meta(json!({"status": "archived", "updated_at": 20_260_805})), now));
  // A falsy updated_at, a float whose str() is not a date, and a status other than the string qualify for nothing.
  assert!(!is_cold_session(&meta(json!({"status": "archived", "updated_at": 0})), now));
  assert!(!is_cold_session(&meta(json!({"status": "archived", "updated_at": 20_260_805.0})), now));
  assert!(!is_cold_session(&meta(json!({"status": ["archived"], "updated_at": OLD})), now));
}

#[test]
fn rotation_suffix_counts_unicode_digits_as_isdigit_does() {
  // `str.isdigit` accepts the Arabic-Indic three and the superscript two, and refuses a vulgar fraction.
  assert!(is_transport_name(Path::new("agent.raw.ndjson.\u{663}")));
  assert!(is_transport_name(Path::new("stdout.log.\u{b2}")));
  assert!(is_transport_name(Path::new("stderr.log.2")));
  assert!(!is_transport_name(Path::new("stdout.log.\u{2153}")));
  assert!(!is_transport_name(Path::new("stdout.log.x")));
  assert!(!is_transport_name(Path::new("stdout.log.")));
}

#[test]
fn cc_session_id_reads_each_truthy_value_as_python_str() {
  assert_eq!(optional_text(Some(&json!("ses_x"))).as_deref(), Some("ses_x"));
  assert_eq!(optional_text(Some(&json!(123))).as_deref(), Some("123"));
  assert_eq!(optional_text(Some(&json!(1.5))).as_deref(), Some("1.5"));
  assert_eq!(optional_text(Some(&json!(true))).as_deref(), Some("True"));
  assert_eq!(optional_text(Some(&json!(["a"]))).as_deref(), Some("['a']"));
  assert_eq!(optional_text(Some(&json!({"a": 1}))).as_deref(), Some("{'a': 1}"));
  for falsy in [json!(""), json!(0), json!(0.0), json!(false), json!([]), json!({}), Value::Null] {
    assert_eq!(optional_text(Some(&falsy)), None, "{falsy} is falsy");
  }
  assert_eq!(optional_text(None), None);
}

#[test]
fn unreadable_or_malformed_session_metadata_skips_that_session() {
  const SID_MALFORMED: &str = "cccccccc-dddd-4eee-8fff-000000000000";
  let fixture = Fixture::new();
  // A metadata path that is a directory cannot be read, and a file that is not JSON does not parse: each session is
  // skipped, and the run goes on to the cold session beside them.
  fs::create_dir_all(fixture.sessions().join(SID_DEAD).join("metadata.json"))
    .expect("a directory at the metadata path");
  put(&fixture.sessions().join(SID_MALFORMED).join("metadata.json"), b"{not json", None);
  let cold = fixture.write_session(SID_COLD, cold_meta());
  let transport = put(&thread_data(&cold).join("stdout.log"), b"transport", None);

  let categories = fixture.sweep().expect("the sweep runs past the skipped sessions");

  assert!(!transport.exists());
  assert_eq!(category(&categories, "raw-transport").count, 1);
}

#[test]
fn session_metadata_that_is_not_an_object_fails_the_run_before_any_deletion() {
  let fixture = Fixture::new();
  let cold = fixture.write_session(SID_COLD, cold_meta());
  let transport = put(&thread_data(&cold).join("stdout.log"), b"transport", None);
  put(&fixture.sessions().join(SID_DEAD).join("metadata.json"), b"[1, 2]", None);

  assert!(fixture.sweep().is_err(), "the cold rule fails on a document that is not an object");
  assert!(transport.exists());
}

#[test]
fn symlinked_run_dirs_and_links_named_like_transport_files_are_skipped() {
  let fixture = Fixture::new();
  let cold = fixture.write_session(SID_COLD, cold_meta());
  // A master run directory that is a symlink is not a managed directory, so the files behind it stay.
  let outside = fixture.tree.path().join("outside/run");
  let behind_link = put(&outside.join(RAW_LOG_NAME), b"outside the managed directories", None);
  let linked = master_run(&cold, "2026-08-01T00:00:00+00:00");
  fs::create_dir_all(linked.parent().expect("the master runs directory")).expect("the master runs directory");
  std::os::unix::fs::symlink(&outside, &linked).expect("the run directory link");
  // A dangling link and a looping link, each named like a transport file, read as absent. The plain file beside them goes.
  let run = master_run(&cold, "2026-08-02T00:00:00+00:00");
  fs::create_dir_all(&run).expect("the run directory");
  std::os::unix::fs::symlink(run.join("missing"), run.join(STDERR_LOG_NAME)).expect("the dangling link");
  std::os::unix::fs::symlink(run.join(CURSOR_NAME), run.join(CURSOR_NAME)).expect("the looping link");
  let sibling = put(&run.join(RAW_LOG_NAME), b"x", None);

  let categories = fixture.sweep().expect("the sweep runs");

  assert!(behind_link.exists(), "a symlinked run directory is not swept");
  assert!(!sibling.exists());
  assert_eq!(category(&categories, "raw-transport").count, 1);
  assert!(run.join(STDERR_LOG_NAME).symlink_metadata().is_ok(), "a dangling link is left in place");
}
