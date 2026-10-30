//! The Codex usage source's log side (`src/backends/codex/usage_logs.py`): the rollouts, their records, and the session
//! ids they register.
//!
//! Record id: `codex-total:<root session id>:<total input>:<total cached>:<total output>`, one per `token_count` event.
//! The root is the rollout's first `session_meta` session id, which a whole fork tree shares. A forked copy of an
//! event therefore lands on the original event's id and counts once.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use charliebot_core::text_repr::python_str;
use charliebot_registry::usage::{
  QuotaAccount, RecordKind, SourceSweep, SweepScope, UsageOps, UsageRecord, UsageSourceImpl,
};
use memchr::{memmem, memrchr};
use serde_json::{Map, Value};

use super::rollout::{self, SESSION_META, TURN_CONTEXT, mtime_ns, token_count_payload, walk_files};
use super::text::{count_or_zero, object_or_empty, truthy};
use super::usage_quota::CodexQuotaAccount;
use super::usage_sweep;

/// The usage source's name: the ledger's source value and the usage page's card title.
pub const USAGE_SOURCE: &str = "Codex";

/// Codex has one account: its default home.
const ACCOUNT: &str = "work (default)";

/// The byte markers of the record types this reader parses. A line without one is never parsed.
const MARKERS: [&[u8]; 3] = [b"\"session_meta\"", b"\"turn_context\"", b"\"token_count\""];

/// The `Codex` usage source: the rollouts under one codex home, with its quota account and its sweep.
pub struct CodexLogs {
  codex_home: PathBuf,
  quota: Arc<dyn QuotaAccount>,
}

impl CodexLogs {
  pub fn new(codex_home: PathBuf) -> Self {
    let quota: Arc<dyn QuotaAccount> = Arc::new(CodexQuotaAccount::new(&codex_home));
    Self { codex_home, quota }
  }

  fn log_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
    let sessions = self.codex_home.join(super::rollout::CODEX_SESSIONS_DIR_NAME);
    if !sessions.is_dir() {
      return Ok(Vec::new());
    }
    walk_files(&sessions, |name| name.ends_with(".jsonl"))
      .with_context(|| format!("listing codex logs under {}", sessions.display()))
  }
}

impl UsageSourceImpl for CodexLogs {
  fn ops(&self) -> UsageOps {
    UsageOps { logs: true, read: true, live_cli_sessions: true, quota_accounts: true, sweep: true }
  }

  fn logs(&self, _cfg: &charliebot_core::config::CharlieBotConfig) -> anyhow::Result<Vec<(PathBuf, String)>> {
    Ok(self.log_paths()?.into_iter().map(|path| (path, ACCOUNT.to_string())).collect())
  }

  fn read(&self, path: &Path, account: &str, _previous: Option<&str>) -> anyhow::Result<(String, Vec<UsageRecord>)> {
    read_rollout(&self.codex_home, path, account)
  }

  fn live_cli_sessions(&self) -> anyhow::Result<HashSet<String>> {
    self.log_paths()?.iter().map(|path| session_id(path, &self.codex_home)).collect()
  }

  fn quota_accounts(
    &self,
    _cfg: &charliebot_core::config::CharlieBotConfig,
  ) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> {
    Ok(vec![Arc::clone(&self.quota)])
  }

  fn sweep(&self, scope: &SweepScope) -> anyhow::Result<SourceSweep> {
    usage_sweep::sweep(&self.codex_home, scope)
  }
}

/// The session a rollout registers: the thread id its name embeds, or the path relative to the codex home for any other
/// file name.
pub fn session_id(path: &Path, codex_home: &Path) -> anyhow::Result<String> {
  let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
  if let Some(thread_id) = rollout::rollout_thread_id(&name) {
    return Ok(thread_id);
  }
  let relative = path
    .strip_prefix(codex_home)
    .with_context(|| format!("{} is not under the codex home {}", path.display(), codex_home.display()))?;
  Ok(relative.to_string_lossy().into_owned())
}

/// One rollout's records and its `<mtime_ns>:<size>` signature, taken before the read.
///
/// A `token_count` event yields no record while its info is null or its `last_token_usage` is all zero: such an event
/// restates the previous one. A `token_count` event with no `session_meta` session id above it, or one that carries a
/// count without `total_token_usage`, is an error. The model in force comes from the `session_meta` and
/// `turn_context` lines, and the file's first model also covers a `token_count` event that precedes it.
fn read_rollout(codex_home: &Path, path: &Path, account: &str) -> anyhow::Result<(String, Vec<UsageRecord>)> {
  let metadata = fs::metadata(path).with_context(|| format!("statting {}", path.display()))?;
  let signature = format!("{}:{}", mtime_ns(&metadata), metadata.len());
  let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
  let lines = marker_lines(&bytes)?;
  let session = session_id(path, codex_home)?;

  let mut model = None;
  for line in lines.iter().filter(|line| is_context(line)) {
    if let Some(named) = payload_model(line)? {
      model = Some(named);
      break;
    }
  }
  let mut root = None;
  if let Some(line) = lines.iter().find(|line| rollout::text_of(line, "type") == Some(SESSION_META)) {
    root = payload_root(line)?;
  }
  let mut seen = HashSet::new();
  let mut records = Vec::new();
  for line in &lines {
    if is_context(line) {
      if let Some(named) = payload_model(line)? {
        model = Some(named);
      }
      if root.is_none() && rollout::text_of(line, "type") == Some(SESSION_META) {
        root = payload_root(line)?;
      }
      continue;
    }
    let Some(payload) = token_count_payload(line)? else {
      continue;
    };
    let root = root
      .as_deref()
      .with_context(|| format!("{}: token_count event with no session_meta session_id above it", path.display()))?;
    let info = object_or_empty(payload, "info")?;
    let last = object_or_empty(info, "last_token_usage")?;
    let cached = count_or_zero(last, "cached_input_tokens")?;
    let fresh = count_or_zero(last, "input_tokens")? - cached;
    let output = count_or_zero(last, "output_tokens")?;
    if fresh == 0 && cached == 0 && output == 0 {
      continue;
    }
    let Some(total) = info.get("total_token_usage").and_then(Value::as_object) else {
      bail!("{}: token_count event without total_token_usage", path.display());
    };
    let total_in = count_or_zero(total, "input_tokens")?;
    let total_cached = count_or_zero(total, "cached_input_tokens")?;
    let total_out = count_or_zero(total, "output_tokens")?;
    // Within one file only the first record per id is kept.
    let record_id = format!("codex-total:{root}:{total_in}:{total_cached}:{total_out}");
    if !seen.insert(record_id.clone()) {
      continue;
    }
    records.push(UsageRecord {
      record_id,
      kind: RecordKind::Native,
      source: USAGE_SOURCE.to_string(),
      model: model.clone().unwrap_or_else(|| "unknown".to_string()),
      account: account.to_string(),
      ts: timestamp_of(line),
      in_fresh: fresh,
      cache_write: 0,
      cache_read: cached,
      output,
      in_unsplit: 0,
      sessions: vec![session.clone()],
    });
  }
  Ok((signature, records))
}

/// The complete lines of `bytes` that carry a marker, parsed. A trailing fragment without its newline waits for a
/// later read. A line that does not parse is dropped; a valid JSON value that is not an object fails when read.
fn marker_lines(bytes: &[u8]) -> anyhow::Result<Vec<Map<String, Value>>> {
  let Some(last_newline) = memrchr(b'\n', bytes) else {
    return Ok(Vec::new());
  };
  let mut records = Vec::new();
  for line in bytes[..=last_newline]
    .split(|byte| *byte == b'\n')
    .filter(|line| MARKERS.iter().any(|marker| memmem::find(line, marker).is_some()))
  {
    match serde_json::from_slice::<Value>(line) {
      Ok(Value::Object(record)) => records.push(record),
      Ok(other) => anyhow::bail!("codex rollout marker line is not a JSON object: {other}"),
      Err(_) => {}
    }
  }
  Ok(records)
}

fn is_context(line: &Map<String, Value>) -> bool {
  matches!(rollout::text_of(line, "type"), Some(SESSION_META | TURN_CONTEXT))
}

/// The truthy `model` a record's payload names, as text.
fn payload_model(line: &Map<String, Value>) -> anyhow::Result<Option<String>> {
  let payload = object_or_empty(line, "payload")?;
  Ok(payload.get("model").filter(|model| truthy(model)).map(python_str))
}

/// The `session_id` of a record's payload, as text, when it has one.
fn payload_root(line: &Map<String, Value>) -> anyhow::Result<Option<String>> {
  let payload = object_or_empty(line, "payload")?;
  Ok(payload.get("session_id").filter(|session_id| !session_id.is_null()).map(python_str))
}

/// The record's timestamp, or the empty string when it has none.
fn timestamp_of(line: &Map<String, Value>) -> String {
  match line.get("timestamp") {
    Some(timestamp) if truthy(timestamp) => python_str(timestamp),
    _ => String::new(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn session_ids_come_from_the_rollout_name_or_the_relative_path() {
    let home = Path::new("/synthetic/home/.codex");
    let rollout =
      home.join("sessions/2026/10/18/rollout-2026-10-18T10-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl");
    assert_eq!(session_id(&rollout, home).unwrap(), "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    assert_eq!(session_id(&home.join("sessions/other.jsonl"), home).unwrap(), "sessions/other.jsonl");
  }

  #[test]
  fn marker_lines_skip_a_trailing_fragment_and_unmarked_lines() {
    let bytes = b"{\"type\":\"token_count\"}\n{\"type\":\"token_count\"\n{\"a\":1}\n{\"type\":\"turn_context\"}";
    let lines = marker_lines(bytes).unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["type"], "token_count");
  }

  #[test]
  fn a_marked_non_object_record_fails() {
    assert!(marker_lines(b"[\"token_count\"]\n").is_err());
  }
}
