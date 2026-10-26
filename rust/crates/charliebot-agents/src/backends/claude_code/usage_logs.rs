//! Claude Code's usage log, read for the "Claude Code" usage source (`src/backends/claude_code/usage_logs.py`).
//!
//! Each Claude login directory keeps its transcripts at `<config_dir>/projects/**/*.jsonl`, and every assistant line
//! carries `message.usage` and `message.model`. The login directories are the default `~/.claude` plus each
//! `accounts.claude` entry of the config.
//!
//! Record id: `claude:<message id, else requestId, else uuid>`, one per response. Claude Code replays history verbatim
//! on resume and fork, so a response dedupes on its id within a file. The record's session is the transcript's file
//! stem.
//!
//! The registry passes the loaded config to `logs`, so this source reads `accounts.claude` through that config.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types::{
  USAGE_CACHE_CREATION_INPUT_TOKENS, USAGE_CACHE_READ_INPUT_TOKENS, USAGE_INPUT_TOKENS, USAGE_OUTPUT_TOKENS,
};
use charliebot_core::pyfmt::python_str;
use charliebot_registry::usage::{
  QuotaAccount, RecordKind, SourceSweep, SweepScope, UsageOps, UsageRecord, UsageSourceImpl,
};
use serde_json::{Map, Value};
use walkdir::WalkDir;

use super::claude_accounts::{ClaudeAccountState, json_truthy};
use super::claude_config::AccountsConfig;
use super::usage_quota::{QuotaAccounts, QuotaHttp};
use super::{ClaudeHost, USAGE_SOURCE, usage_sweep};

/// The byte marker of the lines this reader parses. A line without it is never parsed.
const MARKER: &[u8] = b"\"usage\"";

/// The `Claude Code` usage source: the transcripts of every login directory, with its quota accounts and its sweep.
pub struct ClaudeLogs {
  host: ClaudeHost,
  quota: QuotaAccounts,
}

impl ClaudeLogs {
  pub fn new(host: ClaudeHost, state: Arc<ClaudeAccountState>, http: Arc<QuotaHttp>) -> Self {
    Self { host, quota: QuotaAccounts::new(state, http) }
  }

  /// The current config's login directories: the default one and every `accounts.claude` entry, sorted and deduplicated.
  fn login_dirs(&self, cfg: &CharlieBotConfig) -> Vec<PathBuf> {
    let mut dirs = BTreeSet::from([self.host.default_dir()]);
    for account in &cfg.section::<AccountsConfig>("accounts").claude {
      dirs.insert(self.host.expand(&account.config_dir));
    }
    dirs.into_iter().collect()
  }
}

impl UsageSourceImpl for ClaudeLogs {
  fn ops(&self) -> UsageOps {
    UsageOps { logs: true, read: true, live_cli_sessions: false, quota_accounts: true, sweep: true }
  }

  fn logs(&self, cfg: &CharlieBotConfig) -> anyhow::Result<Vec<(PathBuf, String)>> {
    let mut logs = Vec::new();
    for (label, login_dir) in labeled_login_dirs(&self.login_dirs(cfg)) {
      for path in walk_jsonl(&login_dir.join("projects"))? {
        logs.push((path, label.clone()));
      }
    }
    Ok(logs)
  }

  fn read(&self, path: &Path, account: &str, _previous: Option<&str>) -> anyhow::Result<(String, Vec<UsageRecord>)> {
    read_transcript(path, account)
  }

  fn quota_accounts(&self, cfg: &CharlieBotConfig) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> {
    self.quota.derive(&self.host, cfg)
  }

  fn sweep(&self, scope: &SweepScope) -> anyhow::Result<SourceSweep> {
    usage_sweep::sweep(&self.host, scope)
  }
}

/// The account label of a login directory: `work (default)` for `.claude`, else the name after `.claude-`.
fn account_label(dir: &Path) -> String {
  let name = dir.file_name().unwrap_or_default();
  if name == OsStr::new(".claude") {
    return "work (default)".to_string();
  }
  let name = name.to_string_lossy();
  name.strip_prefix(".claude-").unwrap_or(&name).to_string()
}

/// `_login_dirs()`: the login directories that hold transcripts, by account label. Two directories with one label keep
/// the later one, as a dict does, in the position of the first.
fn labeled_login_dirs(dirs: &[PathBuf]) -> Vec<(String, PathBuf)> {
  let mut labeled: Vec<(String, PathBuf)> = Vec::new();
  for dir in dirs.iter().filter(|dir| dir.join("projects").is_dir()) {
    let label = account_label(dir);
    match labeled.iter_mut().find(|(known, _)| *known == label) {
      Some((_, path)) => *path = dir.clone(),
      None => labeled.push((label, dir.clone())),
    }
  }
  labeled
}

/// Every `.jsonl` file under `root`. A directory is not a transcript, and a symlinked directory is not entered
/// (`ndjson.iter_jsonl_files`).
fn walk_jsonl(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
  let mut found = Vec::new();
  for entry in WalkDir::new(root).follow_links(false) {
    let entry = entry.with_context(|| format!("listing {}", root.display()))?;
    if entry.depth() == 0 || entry.path().is_dir() {
      continue;
    }
    if entry.file_name().to_string_lossy().ends_with(".jsonl") {
      found.push(entry.into_path());
    }
  }
  Ok(found)
}

/// One transcript's records and its `<mtime_ns>:<size>` signature, taken before the read.
///
/// The file streams line by line. Only complete lines parse: a trailing fragment without its newline waits for a
/// later read. A marker line that does not parse is skipped; one that parses to a non-object fails the read.
pub fn read_transcript(path: &Path, account: &str) -> anyhow::Result<(String, Vec<UsageRecord>)> {
  let metadata = fs::metadata(path).with_context(|| format!("statting {}", path.display()))?;
  let signature = format!("{}:{}", mtime_ns(&metadata), metadata.len());
  let session = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
  let mut reader = BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
  let mut seen = HashSet::new();
  let mut records = Vec::new();
  let mut raw = Vec::new();
  loop {
    raw.clear();
    let read = reader.read_until(b'\n', &mut raw).with_context(|| format!("reading {}", path.display()))?;
    if read == 0 || raw.last() != Some(&b'\n') {
      break;
    }
    if !raw.windows(MARKER.len()).any(|window| window == MARKER) {
      continue;
    }
    let Some(line) = parse_marker_line(&raw, path)? else {
      continue;
    };
    let Some(Value::Object(message)) = line.get("message") else {
      continue;
    };
    let Some(Value::Object(usage)) = message.get("usage") else {
      continue;
    };
    let Some(model) = message.get("model").filter(|model| json_truthy(model)) else {
      continue;
    };
    let model = python_str(model);
    if model == "<synthetic>" {
      continue;
    }
    let key = usage_key(message, &line);
    if !seen.insert(key.clone()) {
      continue;
    }
    records.push(UsageRecord {
      record_id: format!("claude:{key}"),
      kind: RecordKind::Native,
      source: USAGE_SOURCE.to_string(),
      model,
      account: account.to_string(),
      ts: timestamp_of(&line),
      in_fresh: usage_count(usage, USAGE_INPUT_TOKENS)?,
      cache_write: usage_count(usage, USAGE_CACHE_CREATION_INPUT_TOKENS)?,
      cache_read: usage_count(usage, USAGE_CACHE_READ_INPUT_TOKENS)?,
      output: usage_count(usage, USAGE_OUTPUT_TOKENS)?,
      in_unsplit: 0,
      sessions: vec![session.clone()],
    });
  }
  Ok((signature, records))
}

/// The JSON object a marker line holds. A line that does not parse is skipped with a debug event.
fn parse_marker_line(raw: &[u8], path: &Path) -> anyhow::Result<Option<Map<String, Value>>> {
  // Invalid UTF-8 decodes with replacement, as the Python reader's fallback parse does.
  let text = String::from_utf8_lossy(raw);
  match serde_json::from_str::<Value>(&text) {
    Ok(Value::Object(line)) => Ok(Some(line)),
    Ok(other) => bail!("{}: a marker line is not a JSON object: {other}", path.display()),
    Err(error) => {
      tracing::debug!(
        target: "charliebot_agents",
        path = %path.display(),
        error = %error,
        "claude_transcript_parse_skip"
      );
      Ok(None)
    }
  }
}

/// The response's key: the message id, else the line's `requestId`, else its `uuid`. When all are falsy, Python's `or`
/// chain yields the `uuid` itself.
fn usage_key(message: &Map<String, Value>, line: &Map<String, Value>) -> String {
  let candidates: [Option<&Value>; 3] = [message.get("id"), line.get("requestId"), line.get("uuid")];
  match candidates.into_iter().flatten().find(|value| json_truthy(value)) {
    Some(value) => python_str(value),
    None => line.get("uuid").map_or_else(|| "None".to_string(), python_str),
  }
}

/// `line.get("timestamp") or ""`.
fn timestamp_of(line: &Map<String, Value>) -> String {
  match line.get("timestamp") {
    Some(timestamp) if json_truthy(timestamp) => python_str(timestamp),
    _ => String::new(),
  }
}

/// `usage.get(key, 0) or 0`: a missing, null or zero count is 0.
fn usage_count(usage: &Map<String, Value>, key: &str) -> anyhow::Result<i64> {
  match usage.get(key) {
    Some(value) if json_truthy(value) => {
      value.as_i64().with_context(|| format!("usage {key} is not an integer: {value}"))
    }
    _ => Ok(0),
  }
}

/// `st_mtime_ns`.
fn mtime_ns(metadata: &fs::Metadata) -> i128 {
  i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())
}

#[cfg(test)]
mod tests {
  use std::io::Write;

  use super::*;

  fn message_line(id: &str, model: &str, timestamp: &str, input: i64, output: i64) -> String {
    format!(
      "{{\"message\":{{\"id\":\"{id}\",\"model\":\"{model}\",\"usage\":{{\"input_tokens\":{input},\
       \"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":{output}}}}},\
       \"requestId\":\"req-{id}\",\"uuid\":\"u-{id}\",\"timestamp\":\"{timestamp}\"}}\n"
    )
  }

  fn write_lines(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines.concat()).unwrap();
  }

  #[test]
  fn replays_synthetic_and_unusable_lines_are_skipped_and_a_response_counts_once() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("projects/rel/sess1/sess1.jsonl");
    write_lines(
      &path,
      &[
        message_line("m1", "claude-opus-5-5", "2024-01-01T00:00:00Z", 100, 30),
        message_line("m1", "claude-opus-5-5", "2024-01-01T00:00:00Z", 100, 30),
        message_line("s1", "<synthetic>", "2024-01-01T00:00:01Z", 9, 9),
        "{\"message\":{\"id\":\"n1\",\"model\":\"\",\"usage\":{\"input_tokens\":5}}}\n".to_string(),
        "{\"message\":{\"id\":\"n2\",\"model\":\"claude-opus-5-5\"}}\n".to_string(),
        "{\"usage\": \"not a message\"\n".to_string(),
        message_line("m2", "claude-opus-5-5", "2024-01-02T00:00:00Z", 7, 3),
      ],
    );
    let mut trailing = fs::OpenOptions::new().append(true).open(&path).unwrap();
    trailing.write_all(b"{\"message\":{\"id\":\"m3\",\"usage\":{\"input_tokens\":1}}}").unwrap();

    let (signature, records) = read_transcript(&path, "work (default)").unwrap();

    let metadata = fs::metadata(&path).unwrap();
    assert_eq!(signature, format!("{}:{}", mtime_ns(&metadata), metadata.len()));
    let ids: Vec<&str> = records.iter().map(|record| record.record_id.as_str()).collect();
    assert_eq!(ids, ["claude:m1", "claude:m2"]);
    assert_eq!(records[0].in_fresh, 100);
    assert_eq!(records[0].output, 30);
    assert_eq!(records[0].kind, RecordKind::Native);
    assert_eq!(records[0].source, "Claude Code");
    assert_eq!(records[0].account, "work (default)");
    assert_eq!(records[0].sessions, vec!["sess1".to_string()]);
    assert_eq!(records[1].ts, "2024-01-02T00:00:00Z");
  }

  #[test]
  fn a_resumed_transcript_reads_whole_and_its_signature_moves() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("projects/rel/sess1/sess1.jsonl");
    write_lines(&path, &[message_line("m1", "claude-opus-5-5", "2024-01-01T00:00:00Z", 10, 5)]);
    let (previous, first) = read_transcript(&path, "work (default)").unwrap();
    assert_eq!(first.len(), 1);

    let mut resumed = fs::OpenOptions::new().append(true).open(&path).unwrap();
    resumed.write_all(message_line("m1", "claude-opus-5-5", "2024-01-01T00:00:00Z", 10, 5).as_bytes()).unwrap();
    resumed.write_all(message_line("m2", "claude-opus-5-5", "2024-01-02T00:00:00Z", 3, 3).as_bytes()).unwrap();
    drop(resumed);

    let (signature, records) = read_transcript(&path, "work (default)").unwrap();
    assert_ne!(signature, previous);
    let ids: Vec<&str> = records.iter().map(|record| record.record_id.as_str()).collect();
    assert_eq!(ids, ["claude:m1", "claude:m2"]);
  }

  #[test]
  fn a_marker_line_that_is_not_an_object_fails_the_read() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("bad.jsonl");
    fs::write(&path, "[\"usage\"]\n").unwrap();
    assert!(read_transcript(&path, "work (default)").is_err());
  }

  #[test]
  fn login_directories_are_labelled_and_a_shared_label_keeps_the_later_directory() {
    let root = tempfile::tempdir().unwrap();
    let default = root.path().join(".claude");
    let ext = root.path().join(".claude-ext-1");
    let shared = root.path().join("accounts/ext-1");
    for dir in [&default, &ext, &shared] {
      fs::create_dir_all(dir.join("projects")).unwrap();
    }
    let mut dirs = vec![default.clone(), ext.clone(), shared.clone()];
    dirs.sort();

    assert_eq!(labeled_login_dirs(&dirs), vec![("work (default)".to_string(), default), ("ext-1".to_string(), shared)]);
  }

  #[test]
  fn logs_read_current_login_directories_without_a_quota_or_sweep_call() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let worktrees = root.path().join("worktrees");
    let default_dir = root.path().join(".claude");
    let first_dir = root.path().join(".claude-ext-1");
    let second_dir = root.path().join(".claude-ext-2");
    for dir in [&default_dir, &first_dir, &second_dir] {
      fs::create_dir_all(dir.join("projects")).unwrap();
      fs::write(dir.join("projects/session.jsonl"), "synthetic\n").unwrap();
    }
    let host = ClaudeHost { home: root.path().to_path_buf(), claude_config_dir: default_dir.clone() };
    let initial_cfg = super::super::test_support::load_config(&profile, &[("ext-1", &first_dir)], &worktrees);
    let source = ClaudeLogs::new(
      host,
      Arc::new(ClaudeAccountState::new()),
      Arc::new(QuotaHttp::with_endpoints(
        charliebot_compat::http_client::client(&charliebot_compat::http_client::Settings::default()).unwrap(),
        "http://127.0.0.1:9/usage",
        "http://127.0.0.1:9/token",
      )),
    );

    assert!(!root.path().join(charliebot_core::config::CONFIG_FILENAME).exists());
    let initial = source.logs(&initial_cfg).unwrap();
    assert_eq!(initial.len(), 2);
    assert!(initial.contains(&(default_dir.join("projects/session.jsonl"), "work (default)".to_string())));
    assert!(initial.contains(&(first_dir.join("projects/session.jsonl"), "ext-1".to_string())));

    let refreshed_cfg = super::super::test_support::load_config(&profile, &[("ext-2", &second_dir)], &worktrees);
    let refreshed = source.logs(&refreshed_cfg).unwrap();
    assert_eq!(refreshed.len(), 2);
    assert!(refreshed.contains(&(default_dir.join("projects/session.jsonl"), "work (default)".to_string())));
    assert!(refreshed.contains(&(second_dir.join("projects/session.jsonl"), "ext-2".to_string())));
  }

  #[test]
  fn usage_keys_follow_the_python_or_chain() {
    let message: Map<String, Value> = serde_json::from_str(r#"{"id": ""}"#).unwrap();
    let line: Map<String, Value> = serde_json::from_str(r#"{"requestId": "r1", "uuid": "u1"}"#).unwrap();
    assert_eq!(usage_key(&message, &line), "r1");
    let bare: Map<String, Value> = serde_json::from_str(r#"{"uuid": 0}"#).unwrap();
    assert_eq!(usage_key(&message, &bare), "0");
    let empty: Map<String, Value> = Map::new();
    assert_eq!(usage_key(&message, &empty), "None");
  }
}
