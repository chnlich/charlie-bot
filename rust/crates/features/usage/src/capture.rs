use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types::{
  USAGE_CACHE_CREATION_INPUT_TOKENS, USAGE_CACHE_READ_INPUT_TOKENS, USAGE_CACHED_INPUT_TOKENS, USAGE_INPUT_TOKENS,
  USAGE_OUTPUT_TOKENS,
};
use charliebot_core::metadata_slot::METADATA_FILE_NAME;
use charliebot_core::text_repr::is_truthy;
use charliebot_core::timefmt;
use charliebot_registry::Registry;
use charliebot_registry::usage::{RecordKind, UsageSource, UsageSourceImpl};
use charliebot_store::layout;
use charliebot_store::metadata::{MetadataReadError, read_metadata_object};
use charliebot_store::run_logs as run_log_files;
use charliebot_store::run_records::{RAW_LOG_NAME, RUN_EVENTS_NAME, RUN_METADATA_NAME};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};

use crate::count::Count;
use crate::ledger::{StoredRecord, UsageLedger, default_ledger_path};
use crate::ndjson;

pub(crate) const CHARLIE_BOT_SOURCE: &str = "charlie-bot";
const CLC_MASTER_ACCOUNT: &str = "clc-master";
const THREAD_MARKERS: [&[u8]; 2] = [b"\"type\": \"result\"", b"\"session_id\""];
const MASTER_MARKERS: [&[u8]; 2] = [b"\"type\": \"context\"", b"\"type\": \"result\""];
const RUN_MARKERS: [&[u8]; 5] =
  [b"\"type\": \"result\"", b"\"type\":\"result\"", b"\"turn.completed\"", b"\"session_id\"", b"\"thread.started\""];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
  Native,
  CodexRule,
  Fallback,
}

#[derive(Default)]
struct LiveSessions {
  by_source: HashMap<&'static str, HashSet<String>>,
}

impl LiveSessions {
  fn excludes(&mut self, source: &UsageSource, ids: &HashSet<String>) -> anyhow::Result<bool> {
    if !self.by_source.contains_key(source.name) {
      let implementation = source
        .implementation
        .as_ref()
        .ok_or_else(|| anyhow!("usage source {} declares live_cli_sessions without an implementation", source.name))?;
      if !implementation.ops().live_cli_sessions {
        bail!("usage source {} does not declare live_cli_sessions", source.name);
      }
      self.by_source.insert(source.name, implementation.live_cli_sessions()?);
    }
    let live = self.by_source.get(source.name).expect("the source was inserted above");
    Ok(ids.iter().any(|session| live.contains(session)))
  }
}

pub fn capture_usage(
  ledger: &mut UsageLedger,
  host: &str,
  sessions_dir: &Path,
  config: &CharlieBotConfig,
  registry: &Registry,
) -> anyhow::Result<Vec<(String, usize)>> {
  let sources = registry.usage_sources().collect::<Vec<_>>();
  if sources.is_empty() {
    bail!("no usage source is registered: the entry point must call registrations.register_all()");
  }
  let captured = ledger.captured_sigs(host)?;
  ledger.batch(|ledger| {
    let mut written = Vec::new();
    for source in sources {
      let Some(implementation) = source.implementation.as_ref() else {
        continue;
      };
      let ops = implementation.ops();
      if ops.logs && ops.read {
        let count = capture_source(ledger, host, config, source, implementation.as_ref(), &captured)?;
        written.push((source.name.to_string(), count));
      }
    }
    let charliebot_count = capture_charliebot(ledger, host, sessions_dir, config, registry, &captured)?;
    let run_count = capture_runs(ledger, host, sessions_dir, config, registry, &captured)?;
    let count = charliebot_count + run_count;
    written.push((CHARLIE_BOT_SOURCE.to_string(), count));
    ledger.mark_capture_finished(host)?;
    Ok(written)
  })
}

pub fn capture_local(
  ledger: &mut UsageLedger,
  config: &CharlieBotConfig,
  registry: &Registry,
) -> anyhow::Result<Vec<(String, usize)>> {
  capture_usage(ledger, &host_name()?, &config.sessions_dir(), config, registry)
}

/// The capture both scheduled handlers run: this host's sources into the default ledger. The cold-storage sweep runs it
/// before any deletion, so a failure here stops the sweep.
pub(crate) fn capture_into_default_ledger(
  config: &CharlieBotConfig,
  registry: &Registry,
) -> anyhow::Result<Vec<(String, usize)>> {
  let mut ledger = UsageLedger::open(default_ledger_path(&config.charliebot_home))?;
  let written = capture_local(&mut ledger, config, registry)?;
  ledger.close()?;
  Ok(written)
}

/// This host's name, as the ledger keys its capture times.
pub(crate) fn host_name() -> anyhow::Result<String> {
  charliebot_core::hostname::os_name()?
    .into_string()
    .map_err(|hostname| anyhow!("hostname is not valid UTF-8: {}", hostname.to_string_lossy()))
}

pub fn capture_charliebot(
  ledger: &mut UsageLedger,
  host: &str,
  sessions_dir: &Path,
  config: &CharlieBotConfig,
  registry: &Registry,
  captured: &BTreeMap<String, String>,
) -> anyhow::Result<usize> {
  let mut live = LiveSessions::default();
  let mut written = 0;
  for (kind, path, metadata) in charliebot_logs(sessions_dir)? {
    let signature = format!("p2:{}:{}", timefmt::file_time_ns(metadata.mtime(), metadata.mtime_nsec()), metadata.len());
    let path_text = path_text(&path)?;
    if captured.get(path_text) == Some(&signature) {
      continue;
    }
    let records = match kind {
      CharlieLogKind::Thread => thread_records(&path, config, registry, &mut live)?,
      CharlieLogKind::Master => master_records(&path, config, registry)?,
    };
    written += ledger.record_stored_file(host, path_text, &signature, &records)?;
  }
  Ok(written)
}

pub fn capture_runs(
  ledger: &mut UsageLedger,
  host: &str,
  sessions_dir: &Path,
  config: &CharlieBotConfig,
  registry: &Registry,
  captured: &BTreeMap<String, String>,
) -> anyhow::Result<usize> {
  let mut live = LiveSessions::default();
  let mut written = 0;
  for (path, metadata) in run_logs(sessions_dir)? {
    let signature = format!("{}:{}", timefmt::file_time_ns(metadata.mtime(), metadata.mtime_nsec()), metadata.len());
    let path_text = path_text(&path)?;
    if captured.get(path_text) == Some(&signature) {
      continue;
    }
    let objects = ndjson::parse_marker_lines(&path, &RUN_MARKERS)?;
    let metadata = run_metadata(&path)?;
    let backend = truthy_string(&metadata, "backend")?;
    let record = backend
      .as_deref()
      .map(|backend| run_record(&objects, &metadata, config, registry, backend, run_id(&path)?, &mut live))
      .transpose()?
      .flatten();
    let records = record.into_iter().collect::<Vec<_>>();
    written += ledger.record_stored_file(host, path_text, &signature, &records)?;
  }
  Ok(written)
}

fn capture_source(
  ledger: &mut UsageLedger,
  host: &str,
  config: &CharlieBotConfig,
  source: &UsageSource,
  implementation: &dyn UsageSourceImpl,
  captured: &BTreeMap<String, String>,
) -> anyhow::Result<usize> {
  let mut written = 0;
  for (path, account) in implementation.logs(config).with_context(|| format!("listing {} usage logs", source.name))? {
    let path_text = path_text(&path)?;
    let metadata = fs::metadata(&path).with_context(|| format!("statting {}", path.display()))?;
    let previous = captured.get(path_text);
    let stat_signature =
      format!("{}:{}", timefmt::file_time_ns(metadata.mtime(), metadata.mtime_nsec()), metadata.len());
    if previous == Some(&stat_signature) {
      continue;
    }
    let (signature, records) = implementation
      .read(&path, &account, previous.map(String::as_str))
      .with_context(|| format!("reading {} usage log {}", source.name, path.display()))?;
    if previous == Some(&signature) && records.is_empty() {
      continue;
    }
    written += ledger.record_file(host, path_text, &signature, &records)?;
  }
  Ok(written)
}

fn charliebot_logs(sessions_dir: &Path) -> anyhow::Result<Vec<(CharlieLogKind, PathBuf, fs::Metadata)>> {
  let Some(sessions) = subdirs(sessions_dir)? else {
    return Ok(Vec::new());
  };
  let mut logs = Vec::new();
  for session in sessions {
    let containers: [(CharlieLogKind, PathBuf, LogPathFn); 2] = [
      (CharlieLogKind::Thread, layout::threads_dir(&session), thread_log),
      (CharlieLogKind::Master, layout::master_runs_dir(&session), master_log),
    ];
    for (kind, container, log_of) in containers {
      let Some(entries) = subdirs(&container)? else {
        continue;
      };
      for entry in entries {
        let path = log_of(&entry);
        // The plain path keys the ledger, so compressing a log re-records the same rows under the same key; its
        // signature comes from the file that holds the bytes now.
        match run_log_files::stored_metadata(&path) {
          Ok(Some(metadata)) => logs.push((kind, path, metadata)),
          Ok(None) => {}
          Err(error) => return Err(error).with_context(|| format!("statting {}", path.display())),
        }
      }
    }
  }
  Ok(logs)
}

/// Maps a container entry's directory to the log file that holds its usage.
type LogPathFn = fn(&Path) -> PathBuf;

/// The event log of one thread directory.
fn thread_log(thread: &Path) -> PathBuf {
  layout::data_dir(thread).join(RUN_EVENTS_NAME)
}

/// The raw log of one master-run directory.
fn master_log(master_run: &Path) -> PathBuf {
  master_run.join(RAW_LOG_NAME)
}

fn run_logs(sessions_dir: &Path) -> anyhow::Result<Vec<(PathBuf, fs::Metadata)>> {
  let Some(sessions) = subdirs(sessions_dir)? else {
    return Ok(Vec::new());
  };
  let mut logs = Vec::new();
  for session in sessions {
    let Some(runs) = subdirs(&layout::runs_dir(&session))? else {
      continue;
    };
    for run in runs {
      let path = run.join(RAW_LOG_NAME);
      // The plain path keys the ledger, so compressing a log re-records the same rows under the same key; its
      // signature comes from the file that holds the bytes now.
      match run_log_files::stored_metadata(&path) {
        Ok(Some(metadata)) => logs.push((path, metadata)),
        Ok(None) => {}
        Err(error) => return Err(error).with_context(|| format!("statting {}", path.display())),
      }
    }
  }
  Ok(logs)
}

fn subdirs(path: &Path) -> anyhow::Result<Option<Vec<PathBuf>>> {
  let entries = match fs::read_dir(path) {
    Ok(entries) => entries,
    Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(error).with_context(|| format!("listing {}", path.display())),
  };
  let mut dirs = Vec::new();
  for entry in entries {
    let entry = entry.with_context(|| format!("listing {}", path.display()))?;
    let file_type = entry.file_type().with_context(|| format!("reading entry under {}", path.display()))?;
    if file_type.is_dir() && !file_type.is_symlink() {
      dirs.push(entry.path());
    }
  }
  Ok(Some(dirs))
}

fn path_text(path: &Path) -> anyhow::Result<&str> {
  path.to_str().ok_or_else(|| anyhow!("path is not valid UTF-8: {}", path.display()))
}

fn path_parts(path: &Path) -> anyhow::Result<Vec<String>> {
  let parts = path
    .components()
    .map(|component| {
      component
        .as_os_str()
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("path component is not valid UTF-8 in {}", path.display()))
    })
    .collect::<anyhow::Result<Vec<_>>>()?;
  if parts.len() < 5 {
    bail!("{} does not have the expected usage-log path shape", path.display());
  }
  Ok(parts)
}

fn thread_records(
  path: &Path,
  config: &CharlieBotConfig,
  registry: &Registry,
  live: &mut LiveSessions,
) -> anyhow::Result<Vec<StoredRecord>> {
  let Some(metadata) = thread_metadata(path)? else {
    return Ok(Vec::new());
  };
  let Some(backend) = truthy_string(&metadata, "backend")? else {
    return Ok(Vec::new());
  };
  let Some((verdict, source)) = backend_verdict(&backend, config, registry)? else {
    return Ok(Vec::new());
  };
  let lines = ndjson::parse_marker_lines(path, &THREAD_MARKERS)?;
  let mut results = Vec::new();
  let mut ids = HashSet::new();
  for line in &lines {
    let object = as_object(line, "thread event")?;
    if is_type(object, "result") {
      results.push(object);
    } else if let Some(session) = session_text(object, "session_id") {
      ids.insert(session);
    }
  }
  if !admitted(verdict, source, &ids, live)? {
    return Ok(Vec::new());
  }
  let model = thread_row_model(&metadata, config, registry, &backend)?;
  let parts = path_parts(path)?;
  let session = &parts[parts.len() - 5];
  let thread = &parts[parts.len() - 3];
  let mut records = Vec::with_capacity(results.len());
  for (index, result) in results.into_iter().enumerate() {
    let usage = usage_object(result.get("usage"))?;
    let (in_fresh, cache_write, cache_read, output, in_unsplit) = verdict_counts(usage, verdict)?;
    let sessions = if verdict == Verdict::Native { Vec::new() } else { sorted_ids(&ids) };
    records.push(StoredRecord {
      record_id: format!("thread:{session}/{thread}/{index}"),
      kind: if verdict == Verdict::Native { RecordKind::Native } else { RecordKind::Fallback },
      source: CHARLIE_BOT_SOURCE.to_string(),
      model: model.clone(),
      account: backend.clone(),
      ts: text_value(result, "timestamp")?,
      in_fresh,
      cache_write,
      cache_read,
      output,
      in_unsplit,
      sessions,
    });
  }
  Ok(records)
}

fn thread_metadata(path: &Path) -> anyhow::Result<Option<Map<String, Value>>> {
  let metadata_path = path
    .parent()
    .and_then(Path::parent)
    .ok_or_else(|| anyhow!("{} has no thread metadata path", path.display()))?
    .join(METADATA_FILE_NAME);
  match read_metadata_object(&metadata_path) {
    Ok(Some(object)) => Ok(Some(object)),
    Ok(None) => Err(anyhow!("{}: metadata is not an object", metadata_path.display())),
    Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
    Err(error) => Err(metadata_read_error(error, &metadata_path)),
  }
}

fn master_records(path: &Path, config: &CharlieBotConfig, registry: &Registry) -> anyhow::Result<Vec<StoredRecord>> {
  let lines = ndjson::parse_marker_lines(path, &MASTER_MARKERS)?;
  let mut model = None;
  let mut last_result = None;
  for line in &lines {
    let object = as_object(line, "master event")?;
    if is_type(object, "context") {
      // The last truthy model wins, so a non-string model before a string one is never read as a name.
      if let Some(named) = object.get("model").filter(|named| is_truthy(named)) {
        model = Some(named);
      }
    } else if is_type(object, "result") {
      last_result = Some(object);
    }
  }
  let (Some(model), Some(result)) = (model, last_result) else {
    return Ok(Vec::new());
  };
  let model = model.as_str().ok_or_else(|| anyhow!("master context model is not a string"))?;
  let usage = usage_object(result.get("usage"))?;
  let input = count(usage, USAGE_INPUT_TOKENS)?;
  let cached = count(usage, "cached_tokens")?;
  let output = count(usage, USAGE_OUTPUT_TOKENS)?;
  let mut account = CLC_MASTER_ACCOUNT.to_string();
  for option in &config.backends.options {
    if option.model == model && run_logs_only(option, registry)? {
      account = option.id.clone();
      break;
    }
  }
  let parts = path_parts(path)?;
  let session = &parts[parts.len() - 5];
  let started = &parts[parts.len() - 2];
  Ok(vec![StoredRecord {
    record_id: format!("master:{session}/{started}"),
    kind: RecordKind::Native,
    source: CHARLIE_BOT_SOURCE.to_string(),
    model: bare_model(model),
    account,
    ts: SqlValue::Text(started.clone()),
    in_fresh: input.sub(cached, "master cached tokens")?,
    cache_write: Count::Int(0),
    cache_read: cached,
    output,
    in_unsplit: Count::Int(0),
    sessions: Vec::new(),
  }])
}

fn run_metadata(path: &Path) -> anyhow::Result<Map<String, Value>> {
  let metadata_path =
    path.parent().ok_or_else(|| anyhow!("{} has no run metadata path", path.display()))?.join(RUN_METADATA_NAME);
  match read_metadata_object(&metadata_path) {
    Ok(Some(object)) => Ok(object),
    Ok(None) => Err(anyhow!("{}: metadata is not an object", metadata_path.display())),
    Err(error) => Err(metadata_read_error(error, &metadata_path)),
  }
}

/// The read failure under the verb the step reports: an unreadable file reads `reading` and a file that is not
/// JSON reads `parsing`.
fn metadata_read_error(error: MetadataReadError, metadata_path: &Path) -> anyhow::Error {
  match error {
    MetadataReadError::Io(error) => anyhow::Error::new(error).context(format!("reading {}", metadata_path.display())),
    MetadataReadError::Json(message) => {
      anyhow::Error::msg(message).context(format!("parsing {}", metadata_path.display()))
    }
  }
}

fn run_record(
  objects: &[Value],
  metadata: &Map<String, Value>,
  config: &CharlieBotConfig,
  registry: &Registry,
  backend: &str,
  id: &str,
  live: &mut LiveSessions,
) -> anyhow::Result<Option<StoredRecord>> {
  let Some((verdict, source)) = backend_verdict(backend, config, registry)? else {
    return Ok(None);
  };
  let (in_fresh, cache_write, cache_read, output, kind, sessions) = match verdict {
    Verdict::Native => {
      let Some(result) = last_result(objects)? else {
        return Ok(None);
      };
      let usage = usage_object(result.get("usage"))?;
      let input = count(usage, USAGE_INPUT_TOKENS)?;
      let cached = count(usage, "cached_tokens")?;
      (
        input.sub(cached, "native run cached tokens")?,
        Count::Int(0),
        cached,
        count(usage, USAGE_OUTPUT_TOKENS)?,
        RecordKind::Native,
        Vec::new(),
      )
    }
    Verdict::CodexRule => {
      let Some((in_fresh, cache_read, output)) = turn_sums(objects)? else {
        return Ok(None);
      };
      let ids = run_session_ids(objects, metadata)?;
      if !admitted(verdict, source, &ids, live)? {
        return Ok(None);
      }
      (in_fresh, Count::Int(0), cache_read, output, RecordKind::Fallback, sorted_ids(&ids))
    }
    Verdict::Fallback => {
      let Some(result) = last_result(objects)? else {
        return Ok(None);
      };
      let usage = usage_object(result.get("usage"))?;
      let (in_fresh, cache_write, cache_read, output) = usage_counts(usage)?;
      let ids = run_session_ids(objects, metadata)?;
      if !admitted(verdict, source, &ids, live)? {
        return Ok(None);
      }
      (in_fresh, cache_write, cache_read, output, RecordKind::Fallback, sorted_ids(&ids))
    }
  };
  let model = thread_row_model(metadata, config, registry, backend)?;
  Ok(Some(StoredRecord {
    record_id: format!("run:{id}"),
    kind,
    source: CHARLIE_BOT_SOURCE.to_string(),
    model,
    account: backend.to_string(),
    ts: text_value(metadata, "started_at")?,
    in_fresh,
    cache_write,
    cache_read,
    output,
    in_unsplit: Count::Int(0),
    sessions,
  }))
}

fn last_result(objects: &[Value]) -> anyhow::Result<Option<&Map<String, Value>>> {
  for value in objects.iter().rev() {
    let object = as_object(value, "run event")?;
    if is_type(object, "result") {
      return Ok(Some(object));
    }
  }
  Ok(None)
}

fn turn_sums(objects: &[Value]) -> anyhow::Result<Option<(Count, Count, Count)>> {
  let mut totals = (Count::Int(0), Count::Int(0), Count::Int(0));
  let mut seen = false;
  for value in objects {
    let object = as_object(value, "run event")?;
    if !is_type(object, "turn.completed") {
      continue;
    }
    seen = true;
    let usage = usage_object(object.get("usage"))?;
    let cached = count(usage, USAGE_CACHED_INPUT_TOKENS)?;
    let fresh = count(usage, USAGE_INPUT_TOKENS)?.sub(cached, "Codex cached tokens")?;
    totals.0 = totals.0.add(fresh, "Codex input tokens")?;
    totals.1 = totals.1.add(cached, "Codex cached tokens")?;
    totals.2 = totals.2.add(count(usage, USAGE_OUTPUT_TOKENS)?, "Codex output tokens")?;
  }
  Ok(seen.then_some(totals))
}

fn run_session_ids(objects: &[Value], metadata: &Map<String, Value>) -> anyhow::Result<HashSet<String>> {
  let mut ids = HashSet::new();
  ids.extend(session_text(metadata, "native_session_id"));
  for value in objects {
    let object = as_object(value, "run event")?;
    ids.extend(session_text(object, "session_id"));
    if is_type(object, "thread.started") {
      ids.extend(session_text(object, "thread_id"));
    }
  }
  Ok(ids)
}

/// The usage source one backend id attributes to, or `None` when it has none: a
/// registered id reads its config type, and an id that left the config reads the id prefix its source registers.
pub(crate) fn backend_source<'a>(
  backend: &str,
  config: &CharlieBotConfig,
  registry: &'a Registry,
) -> anyhow::Result<Option<&'a UsageSource>> {
  if let Some(option) = config.get_backend_option(backend) {
    return registry.usage_source_for(&option.backend_type);
  }
  Ok(registry.usage_sources().find(|source| source.id_prefixes.iter().any(|prefix| backend.starts_with(prefix))))
}

fn backend_verdict<'a>(
  backend: &str,
  config: &CharlieBotConfig,
  registry: &'a Registry,
) -> anyhow::Result<Option<(Verdict, &'a UsageSource)>> {
  let Some(source) = backend_source(backend, config, registry)? else {
    return Ok(None);
  };
  let verdict = if source.run_logs_only {
    Verdict::Native
  } else if source.implementation.as_ref().is_some_and(|implementation| implementation.ops().live_cli_sessions) {
    Verdict::CodexRule
  } else {
    Verdict::Fallback
  };
  Ok(Some((verdict, source)))
}

fn admitted(
  verdict: Verdict,
  source: &UsageSource,
  ids: &HashSet<String>,
  live: &mut LiveSessions,
) -> anyhow::Result<bool> {
  match verdict {
    Verdict::Native => Ok(true),
    Verdict::Fallback => Ok(!ids.is_empty()),
    Verdict::CodexRule => Ok(!ids.is_empty() && !live.excludes(source, ids)?),
  }
}

fn run_logs_only(option: &BackendOption, registry: &Registry) -> anyhow::Result<bool> {
  Ok(registry.usage_source_for(&option.backend_type)?.is_some_and(|source| source.run_logs_only))
}

fn thread_row_model(
  metadata: &Map<String, Value>,
  config: &CharlieBotConfig,
  registry: &Registry,
  backend: &str,
) -> anyhow::Result<String> {
  if let Some(model) = truthy_string(metadata, "model")? {
    return Ok(bare_model(&model));
  }
  if let Some(option) = config.get_backend_option(backend)
    && !option.model.is_empty()
  {
    return Ok(bare_model(&option.model));
  }
  let mut prefix = "";
  for source in registry.usage_sources() {
    if let Some(known) = source.id_prefixes.iter().find(|prefix| backend.starts_with(**prefix)) {
      prefix = known;
      break;
    }
  }
  Ok(backend.strip_prefix(prefix).unwrap_or(backend).to_string())
}

/// A thread result's (in_fresh, cache_write, cache_read, output, in_unsplit), split by the backend's verdict. The
/// capture reads all four keys but adds or binds only the ones the verdict keeps, so a value under a dropped key cannot
/// fail the capture, and only the kept keys are read here.
fn verdict_counts(usage: &Map<String, Value>, verdict: Verdict) -> anyhow::Result<(Count, Count, Count, Count, Count)> {
  match verdict {
    Verdict::CodexRule => {
      let input = count(usage, USAGE_INPUT_TOKENS)?;
      let cache_read = count(usage, USAGE_CACHE_READ_INPUT_TOKENS)?;
      let output = count(usage, USAGE_OUTPUT_TOKENS)?;
      Ok((input.sub(cache_read, "thread cached tokens")?, Count::Int(0), cache_read, output, Count::Int(0)))
    }
    Verdict::Native => {
      let input = count(usage, USAGE_INPUT_TOKENS)?;
      let output = count(usage, USAGE_OUTPUT_TOKENS)?;
      Ok((Count::Int(0), Count::Int(0), Count::Int(0), output, input))
    }
    Verdict::Fallback => {
      let (input, cache_write, cache_read, output) = usage_counts(usage)?;
      Ok((input, cache_write, cache_read, output, Count::Int(0)))
    }
  }
}

fn usage_counts(usage: &Map<String, Value>) -> anyhow::Result<(Count, Count, Count, Count)> {
  Ok((
    count(usage, USAGE_INPUT_TOKENS)?,
    count(usage, USAGE_CACHE_CREATION_INPUT_TOKENS)?,
    count(usage, USAGE_CACHE_READ_INPUT_TOKENS)?,
    count(usage, USAGE_OUTPUT_TOKENS)?,
  ))
}

fn usage_object(value: Option<&Value>) -> anyhow::Result<&Map<String, Value>> {
  match value {
    None => Ok(empty_object()),
    Some(value) if !is_truthy(value) => Ok(empty_object()),
    Some(Value::Object(object)) => Ok(object),
    Some(_) => bail!("usage field is not an object"),
  }
}

fn empty_object() -> &'static Map<String, Value> {
  static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
  &EMPTY
}

/// One usage count as the capture reads it: an absent or falsy value is 0, `true` is 1, and a
/// number keeps its integer or real type. Any other value fails here; the ledger fails where the value is added or bound.
fn count(object: &Map<String, Value>, key: &str) -> anyhow::Result<Count> {
  match object.get(key) {
    None => Ok(Count::Int(0)),
    Some(value) if !is_truthy(value) => Ok(Count::Int(0)),
    Some(Value::Bool(_)) => Ok(Count::Int(1)),
    Some(Value::Number(number)) => number_count(number).with_context(|| format!("usage count {key}")),
    Some(_) => bail!("usage count {key} is not a number"),
  }
}

/// A JSON number as the ledger binds it: an integer within the signed 64-bit range, else a real.
/// serde_json reads an integer literal past u64 as an f64, the value the ledger format stores for that literal.
fn number_count(number: &serde_json::Number) -> anyhow::Result<Count> {
  if let Some(integer) = number.as_i64() {
    return Ok(Count::Int(integer));
  }
  match number.as_f64() {
    Some(real) if number.is_f64() => Ok(Count::Real(real)),
    _ => bail!("integer {number} is outside the signed 64-bit range"),
  }
}

/// The TEXT column's value under `key`: a falsy value is "", a string is itself,
/// and `true` or a number binds as the INTEGER or REAL that the column stores as text. A list or map fails the bind.
fn text_value(object: &Map<String, Value>, key: &str) -> anyhow::Result<SqlValue> {
  match object.get(key) {
    None => Ok(SqlValue::Text(String::new())),
    Some(value) if !is_truthy(value) => Ok(SqlValue::Text(String::new())),
    Some(Value::String(text)) => Ok(SqlValue::Text(text.clone())),
    Some(Value::Bool(_)) => Ok(SqlValue::Integer(1)),
    Some(Value::Number(number)) => Ok(number_count(number).with_context(|| format!("field {key}"))?.into()),
    Some(_) => bail!("field {key} is neither a string nor a number"),
  }
}

/// The string under `key` read through the truth test: an absent or falsy value is None, and a truthy value
/// that is not a string fails the capture.
fn truthy_string(object: &Map<String, Value>, key: &str) -> anyhow::Result<Option<String>> {
  match object.get(key) {
    None => Ok(None),
    Some(value) if !is_truthy(value) => Ok(None),
    Some(Value::String(value)) => Ok(Some(value.clone())),
    Some(_) => bail!("field {key} is not a string"),
  }
}

/// The non-empty string under `key`. Any other value names no session and is ignored.
fn session_text(object: &Map<String, Value>, key: &str) -> Option<String> {
  object.get(key).and_then(Value::as_str).filter(|session| !session.is_empty()).map(str::to_string)
}

/// Whether the line's `type` is the string `kind`. A `type` that is not a string matches no kind.
fn is_type(object: &Map<String, Value>, kind: &str) -> bool {
  object.get("type").and_then(Value::as_str) == Some(kind)
}

fn as_object<'a>(value: &'a Value, label: &str) -> anyhow::Result<&'a Map<String, Value>> {
  value.as_object().ok_or_else(|| anyhow!("{label} is not an object"))
}

fn bare_model(model: &str) -> String {
  model.rsplit('/').next().unwrap_or(model).to_string()
}

fn sorted_ids(ids: &HashSet<String>) -> Vec<String> {
  let mut ids = ids.iter().cloned().collect::<Vec<_>>();
  ids.sort();
  ids
}

fn run_id(path: &Path) -> anyhow::Result<&str> {
  path
    .parent()
    .and_then(Path::file_name)
    .and_then(|id| id.to_str())
    .ok_or_else(|| anyhow!("{} has no UTF-8 run id", path.display()))
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharlieLogKind {
  Thread,
  Master,
}
