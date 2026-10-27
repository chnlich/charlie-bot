//! Trace decoding, event rewriting, gzip output and the per-profile cache.

use std::collections::HashMap;
use std::fs::{self, File, FileTimes};
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use charliebot_compat::json::fast_json_bytes;
use charliebot_core::venv::VenvBin;
use charliebot_registry::FeatureContext;
use memchr::{memchr, memchr_iter, memchr2, memrchr};
use serde::Serialize;
use serde_json::ser::Formatter;
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use tempfile::Builder;
use tokio::sync::{Mutex, Semaphore, watch};

const MERGE_CACHE_LIMIT: usize = 24;
const MERGE_BATCH_EVENTS: usize = 512;
const MEMBER_ID_STRIDE: u64 = 1 << 24;
const DIRECT_MODE: &str = "gzip";
const MERGE_MODE: &str = "merge";

type PidLabelOrder = Vec<(String, String)>;
type PidLabelLookup = HashMap<String, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BuildMode {
  DirectPass,
  Merge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BuildFailure {
  NotATrace(String),
  Parse(String),
  Failed(String),
}

impl BuildFailure {
  pub(super) fn message(&self) -> &str {
    match self {
      BuildFailure::NotATrace(message) | BuildFailure::Parse(message) | BuildFailure::Failed(message) => message,
    }
  }

  pub(super) fn is_not_a_trace(&self) -> bool {
    matches!(self, BuildFailure::NotATrace(_))
  }
}

type FlightResult = Result<PathBuf, BuildFailure>;

struct MergeRuntime {
  gate: Arc<Semaphore>,
  flights: Mutex<HashMap<String, watch::Receiver<Option<FlightResult>>>>,
}

#[derive(Clone)]
struct BuildPlan {
  cache_dir: PathBuf,
  cache_path: PathBuf,
  paths: Vec<PathBuf>,
  slim: bool,
  mode: BuildMode,
  venv_bin: PathBuf,
}

static RUNTIME: OnceLock<Arc<MergeRuntime>> = OnceLock::new();

fn runtime() -> Arc<MergeRuntime> {
  Arc::clone(RUNTIME.get_or_init(|| {
    let cpu_count = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    let cpus = if cpu_count < 1 { 2 } else { cpu_count as usize };
    Arc::new(MergeRuntime { gate: Arc::new(Semaphore::new(cpus.min(4))), flights: Mutex::new(HashMap::new()) })
  }))
}

pub(super) async fn cached_build(
  features: Arc<FeatureContext>,
  paths: Vec<PathBuf>,
  slim: bool,
  mode: BuildMode,
) -> Result<PathBuf, BuildFailure> {
  let python_mode = match mode {
    BuildMode::DirectPass => DIRECT_MODE,
    BuildMode::Merge => MERGE_MODE,
  };
  let key = merge_cache_key(&paths, slim, python_mode)?;
  let cfg = features.config.get().map_err(|error| BuildFailure::Failed(error.to_string()))?;
  let cache_dir = cfg.charliebot_home.join("cache").join("perfetto_merge");
  fs::create_dir_all(&cache_dir).map_err(io_failure)?;
  let cache_path = cache_dir.join(format!("{key}.json.gz"));
  if cache_path.is_file() {
    touch(&cache_path)?;
    return Ok(cache_path);
  }

  let venv_bin = features.startup.require::<VenvBin>().map_err(|error| BuildFailure::Failed(error.to_string()))?;
  let plan =
    BuildPlan { cache_dir, cache_path: cache_path.clone(), paths, slim, mode, venv_bin: venv_bin.path().to_path_buf() };
  let runtime = runtime();
  let receiver = {
    let mut flights = runtime.flights.lock().await;
    if let Some(receiver) = flights.get(&key) {
      receiver.clone()
    } else {
      if plan.cache_path.is_file() {
        touch(&plan.cache_path)?;
        return Ok(plan.cache_path.clone());
      }
      let (sender, receiver) = watch::channel(None);
      flights.insert(key.clone(), receiver.clone());
      let runtime = Arc::clone(&runtime);
      tokio::spawn(async move {
        let result = match runtime.gate.clone().acquire_owned().await {
          Ok(permit) => tokio::task::spawn_blocking(move || {
            let _permit = permit;
            build_cached(&plan)
          })
          .await
          .unwrap_or_else(|error| Err(BuildFailure::Failed(format!("trace build task failed: {error}")))),
          Err(error) => Err(BuildFailure::Failed(format!("trace build gate failed: {error}"))),
        };
        sender.send_replace(Some(result));
        runtime.flights.lock().await.remove(&key);
      });
      receiver
    }
  };
  await_flight(receiver).await
}

async fn await_flight(mut receiver: watch::Receiver<Option<FlightResult>>) -> FlightResult {
  loop {
    if let Some(result) = receiver.borrow().clone() {
      return result;
    }
    if receiver.changed().await.is_err() {
      if let Some(result) = receiver.borrow().clone() {
        return result;
      }
      return Err(BuildFailure::Failed("trace build ended without a result".to_string()));
    }
  }
}

fn build_cached(plan: &BuildPlan) -> FlightResult {
  let temp_file = Builder::new().suffix(".tmp").tempfile_in(&plan.cache_dir).map_err(io_failure)?;
  let (file, temp_path) = temp_file.into_parts();
  drop(file);
  let temp_path_buf = temp_path.to_path_buf();
  let built = match plan.mode {
    BuildMode::DirectPass => direct_pass(&plan.paths[0], &temp_path_buf, &plan.venv_bin),
    BuildMode::Merge => merge_artifact(&plan.paths, &temp_path_buf, plan.slim, &plan.venv_bin),
  };
  if let Err(error) = built {
    drop(temp_path);
    return Err(error);
  }
  temp_path.persist(&plan.cache_path).map_err(|error| io_failure(error.error))?;
  prune_cache(&plan.cache_dir, &plan.cache_path)?;
  Ok(plan.cache_path.clone())
}

fn merge_cache_key(paths: &[PathBuf], slim: bool, mode: &str) -> Result<String, BuildFailure> {
  #[derive(Serialize)]
  struct CacheKeyPayload<'a> {
    paths: Vec<(String, u64, i64)>,
    slim: bool,
    mode: &'a str,
  }

  let mut path_rows = Vec::with_capacity(paths.len());
  for path in paths {
    let metadata = fs::metadata(path).map_err(io_failure)?;
    path_rows.push((
      path.to_string_lossy().into_owned(),
      metadata.size(),
      metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
    ));
  }
  let payload = CacheKeyPayload { paths: path_rows, slim, mode };
  let mut encoded = Vec::new();
  let mut serializer = serde_json::Serializer::with_formatter(&mut encoded, PythonAsciiFormatter);
  payload.serialize(&mut serializer).map_err(|error| BuildFailure::Failed(error.to_string()))?;
  Ok(format!("{:x}", Sha256::digest(encoded)))
}

struct PythonAsciiFormatter;

impl Formatter for PythonAsciiFormatter {
  fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> std::io::Result<()>
  where
    W: ?Sized + Write,
  {
    let mut start = 0;
    for (index, character) in fragment.char_indices().filter(|(_, character)| !character.is_ascii()) {
      writer.write_all(&fragment.as_bytes()[start..index])?;
      let code = character as u32;
      if code <= 0xffff {
        write!(writer, "\\u{code:04x}")?;
      } else {
        let code = code - 0x10000;
        write!(writer, "\\u{:04x}\\u{:04x}", 0xd800 + (code >> 10), 0xdc00 + (code & 0x3ff))?;
      }
      start = index + character.len_utf8();
    }
    writer.write_all(&fragment.as_bytes()[start..])
  }
}

fn touch(path: &Path) -> Result<(), BuildFailure> {
  let file = File::open(path).map_err(io_failure)?;
  let now = SystemTime::now();
  file.set_times(FileTimes::new().set_accessed(now).set_modified(now)).map_err(io_failure)
}

fn prune_cache(cache_dir: &Path, fresh_path: &Path) -> Result<(), BuildFailure> {
  let mut entries = Vec::new();
  for entry in fs::read_dir(cache_dir).map_err(io_failure)? {
    let path = entry.map_err(io_failure)?.path();
    if path == fresh_path || !path.file_name().is_some_and(|name| name.to_string_lossy().ends_with(".json.gz")) {
      continue;
    }
    let metadata = fs::metadata(&path).map_err(io_failure)?;
    entries.push((metadata.mtime(), metadata.mtime_nsec(), path));
  }
  entries.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
  for (_, _, stale_path) in entries.into_iter().skip(MERGE_CACHE_LIMIT - 1) {
    fs::remove_file(stale_path).map_err(io_failure)?;
  }
  Ok(())
}

fn io_failure(error: std::io::Error) -> BuildFailure {
  BuildFailure::Failed(error.to_string())
}

pub(super) fn is_json_trace(path: &Path) -> Result<bool, BuildFailure> {
  let mut trace = File::open(path).map_err(io_failure)?;
  let mut prefix = [0; 64];
  let length = trace.read(&mut prefix).map_err(io_failure)?;
  Ok(prefix[..length].iter().find(|byte| !b" \t\n\r".contains(byte)).is_some_and(|byte| b"{[".contains(byte)))
}

fn direct_pass(path: &Path, out_path: &Path, venv_bin: &Path) -> Result<(), BuildFailure> {
  let source = fs::read(path).map_err(io_failure)?;
  let document = match parse_document(&source) {
    Ok(document) => document,
    Err(BuildFailure::Parse(message)) => return Err(BuildFailure::Parse(format!("trace failed to parse: {message}"))),
    Err(error) => return Err(error),
  };
  trace_events_or_raise(&document, path)?;

  let out_file = File::create(out_path).map_err(io_failure)?;
  let output = Command::new(venv_bin.join("python"))
    .arg("-m")
    .arg("isal.igzip")
    .arg("-1")
    .arg("-n")
    .arg("-c")
    .arg(path)
    .stdout(Stdio::from(out_file))
    .stderr(Stdio::piped())
    .output()
    .map_err(io_failure)?;
  if output.status.success() {
    return Ok(());
  }
  Err(BuildFailure::Failed(String::from_utf8_lossy(&output.stderr).trim().to_string()))
}

fn merge_artifact(paths: &[PathBuf], out_path: &Path, slim: bool, venv_bin: &Path) -> Result<(), BuildFailure> {
  let document = if paths.len() == 1 { merge_traces(paths, slim)? } else { build_multi_trace_merge(paths, slim)? };
  gzip_bytes(&document, out_path, venv_bin)
}

fn gzip_bytes(document: &[u8], out_path: &Path, venv_bin: &Path) -> Result<(), BuildFailure> {
  let out_file = File::create(out_path).map_err(io_failure)?;
  let mut child = Command::new(venv_bin.join("python"))
    .arg("-m")
    .arg("isal.igzip")
    .arg("-1")
    .arg("-n")
    .stdin(Stdio::piped())
    .stdout(Stdio::from(out_file))
    .stderr(Stdio::piped())
    .spawn()
    .map_err(io_failure)?;
  let write_result = child.stdin.take().expect("the compressor has piped stdin").write_all(document);
  if let Err(error) = write_result {
    let output = child.wait_with_output().map_err(io_failure)?;
    if !output.status.success() {
      return Err(compressor_failure(&output.stderr, "trace merge"));
    }
    return Err(io_failure(error));
  }
  let output = child.wait_with_output().map_err(io_failure)?;
  if !output.status.success() {
    return Err(compressor_failure(&output.stderr, "trace merge"));
  }
  Ok(())
}

fn compressor_failure(stderr: &[u8], context: &str) -> BuildFailure {
  BuildFailure::Failed(format!("isal.igzip -1 failed ({context}): {}", String::from_utf8_lossy(stderr).trim()))
}

fn parse_document(source: &[u8]) -> Result<Value, BuildFailure> {
  serde_json::from_slice(source).map_err(|error| BuildFailure::Parse(orjson_error_text(source, &error)))
}

fn trace_events_or_raise<'a>(document: &'a Value, path: &Path) -> Result<&'a Vec<Value>, BuildFailure> {
  match document {
    Value::Array(events) => Ok(events),
    Value::Object(fields) => match fields.get("traceEvents") {
      Some(Value::Array(events)) => Ok(events),
      _ => Err(BuildFailure::NotATrace(format!("Not a Chrome-JSON trace (no traceEvents array): {}", path.display()))),
    },
    _ => Err(BuildFailure::NotATrace(format!("Not a Chrome-JSON trace (no traceEvents array): {}", path.display()))),
  }
}

fn merge_traces(paths: &[PathBuf], slim: bool) -> Result<Vec<u8>, BuildFailure> {
  let mut output = Vec::new();
  output.extend_from_slice(b"{\"traceEvents\":[");
  let mut batcher = EventBatcher::new(&mut output);
  let mut tid_seq = IdSequencer::new(1);
  let mut flow_seq = IdSequencer::new(1);
  for (file_index, path) in paths.iter().enumerate() {
    tid_seq.start_trace();
    flow_seq.start_trace();
    let source = fs::read(path).map_err(io_failure)?;
    let document = parse_document(&source)?;
    merge_one_trace(&document, path, file_index, &mut batcher, &mut tid_seq, &mut flow_seq, slim)?;
  }
  output.extend_from_slice(b"]}");
  Ok(output)
}

fn build_multi_trace_merge(paths: &[PathBuf], slim: bool) -> Result<Vec<u8>, BuildFailure> {
  let mut output = Vec::new();
  output.extend_from_slice(b"{\"traceEvents\":[");
  let mut emitted_any = false;
  let mut skipped = 0;
  for (file_index, path) in paths.iter().enumerate() {
    let mut fragment = Vec::new();
    let mut batcher = EventBatcher::new(&mut fragment);
    let start = 1 + file_index as u64 * MEMBER_ID_STRIDE;
    let mut tid_seq = IdSequencer::new(start);
    let mut flow_seq = IdSequencer::new(start);
    let source = fs::read(path).map_err(io_failure)?;
    let document = parse_document(&source)?;
    match merge_one_trace(&document, path, file_index, &mut batcher, &mut tid_seq, &mut flow_seq, slim) {
      Ok(()) => {
        if !fragment.is_empty() {
          if emitted_any {
            output.push(b',');
          }
          output.extend_from_slice(&fragment);
          emitted_any = true;
        }
      }
      Err(error @ BuildFailure::NotATrace(_)) => {
        tracing::warn!(path = %path.display(), error = %error.message(), "perfetto_merge_member_skipped");
        skipped += 1;
      }
      Err(error) => return Err(error),
    }
  }
  output.extend_from_slice(b"]}");
  if !paths.is_empty() && skipped == paths.len() {
    return Err(BuildFailure::Failed(format!(
      "multi-trace merge rejected every member as a non-Chrome-JSON trace: {}",
      paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", ")
    )));
  }
  Ok(output)
}

struct EventBatcher<'a> {
  output: &'a mut Vec<u8>,
  emitted_any: bool,
}

impl<'a> EventBatcher<'a> {
  fn new(output: &'a mut Vec<u8>) -> EventBatcher<'a> {
    EventBatcher { output, emitted_any: false }
  }

  fn flush(&mut self, pending: &mut Vec<Value>) {
    if pending.is_empty() {
      return;
    }
    if self.emitted_any {
      self.output.push(b',');
    }
    self.emitted_any = true;
    self.output.extend_from_slice(&fast_json_bytes(&Value::Array(std::mem::take(pending)))[1..]);
    let last = self.output.len() - 1;
    self.output.remove(last);
  }
}

struct IdSequencer {
  next_id: u64,
  seen: HashMap<String, u64>,
}

impl IdSequencer {
  fn new(start: u64) -> IdSequencer {
    IdSequencer { next_id: start, seen: HashMap::new() }
  }

  fn start_trace(&mut self) {
    self.seen.clear();
  }

  fn admit(&mut self, original: &Value) -> u64 {
    let key = python_value_text(original);
    if let Some(id) = self.seen.get(&key) {
      return *id;
    }
    let id = self.next_id;
    self.seen.insert(key, id);
    self.next_id += 1;
    id
  }
}

fn merge_one_trace(
  document: &Value,
  path: &Path,
  file_index: usize,
  batcher: &mut EventBatcher<'_>,
  tid_seq: &mut IdSequencer,
  flow_seq: &mut IdSequencer,
  slim: bool,
) -> Result<(), BuildFailure> {
  let events = trace_events_or_raise(document, path)?;
  let rank_label = rank_label(path);
  let (pid_labels, pid_label_lookup) = collect_pid_labels(events)?;
  let mut synthetic_meta = Vec::new();
  let mut synthetic_meta_index = HashMap::new();
  for (_, label) in &pid_labels {
    let synthetic_pid = if label == "CPU" { rank_label.clone() } else { format!("{rank_label}/{label}") };
    let base = file_index as u64 * 10000;
    let meta = if label == "CPU" {
      (base, format!("{rank_label} CPU"))
    } else {
      let gpu_index = gpu_index(label).unwrap_or(0);
      (base + 1000 + gpu_index, format!("{rank_label} {label}"))
    };
    if let Some(index) = synthetic_meta_index.get(&synthetic_pid).copied() {
      synthetic_meta[index] = (synthetic_pid.clone(), meta);
    } else {
      synthetic_meta_index.insert(synthetic_pid.clone(), synthetic_meta.len());
      synthetic_meta.push((synthetic_pid, meta));
    }
  }

  let mut pending = Vec::with_capacity(MERGE_BATCH_EVENTS);
  for original in events {
    let Some(mut event) = original.as_object().cloned() else {
      return Err(attribute_failure(original, "get"));
    };
    let phase = event.get("ph").cloned().unwrap_or(Value::Null);
    if walk_drops(&event, &phase, slim)? {
      continue;
    }
    if slim && let Some(Value::Object(args)) = event.get("args") {
      let stream = args.get("stream").cloned();
      match stream {
        Some(value) => {
          let mut reduced = Map::new();
          reduced.insert("stream".to_string(), value);
          event.insert("args".to_string(), Value::Object(reduced));
        }
        None => {
          event.shift_remove("args");
        }
      }
    }
    let original_pid = event.get("pid").cloned().unwrap_or(Value::Null);
    let pid_key = python_value_text(&original_pid);
    let synthetic_pid = pid_label_lookup
      .get(&pid_key)
      .map(|label| if label == "CPU" { rank_label.clone() } else { format!("{rank_label}/{label}") })
      .unwrap_or_else(|| rank_label.clone());
    event.insert("pid".to_string(), Value::String(synthetic_pid.clone()));

    if let Some(original_tid) = event.get("tid").cloned() {
      let tid_key = python_value_text(&original_tid);
      let first_sight = !tid_seq.seen.contains_key(&tid_key);
      let synthetic_tid = tid_seq.admit(&original_tid);
      if first_sight {
        pending.push(thread_name_event(&synthetic_pid, synthetic_tid, &original_tid, &rank_label));
        if pending.len() >= MERGE_BATCH_EVENTS {
          batcher.flush(&mut pending);
        }
      }
      event.insert("tid".to_string(), Value::Number(Number::from(synthetic_tid)));
    }
    if matches!(phase.as_str(), Some("s" | "t" | "f"))
      && let Some(original_id) = event.get("id").cloned()
    {
      let synthetic_id = flow_seq.admit(&original_id);
      event.insert("id".to_string(), Value::Number(Number::from(synthetic_id)));
    }
    pending.push(Value::Object(event));
    if pending.len() >= MERGE_BATCH_EVENTS {
      batcher.flush(&mut pending);
    }
  }

  let meta_tid = tid_seq.admit(&Value::String("meta".to_string()));
  for (synthetic_pid, (sort_index, label)) in synthetic_meta {
    for (name, args) in [
      ("process_name", serde_json::json!({"name": label})),
      ("process_labels", serde_json::json!({"labels": label})),
      ("process_sort_index", serde_json::json!({"sort_index": sort_index})),
    ] {
      pending.push(metadata_event(&synthetic_pid, meta_tid, name, args));
      if pending.len() >= MERGE_BATCH_EVENTS {
        batcher.flush(&mut pending);
      }
    }
  }
  batcher.flush(&mut pending);
  Ok(())
}

fn collect_pid_labels(events: &[Value]) -> Result<(PidLabelOrder, PidLabelLookup), BuildFailure> {
  let mut ordered: PidLabelOrder = Vec::new();
  let mut positions: HashMap<String, usize> = HashMap::new();
  let mut labels = HashMap::new();
  for event in events {
    let event = event.as_object().ok_or_else(|| attribute_failure(event, "get"))?;
    if event.get("ph").and_then(Value::as_str) != Some("M")
      || event.get("name").and_then(Value::as_str) != Some("process_labels")
    {
      continue;
    }
    let Some(args_value) = event.get("args") else {
      continue;
    };
    if !python_truthy(args_value) {
      continue;
    }
    let args = args_value.as_object().ok_or_else(|| attribute_failure(args_value, "get"))?;
    let key = python_value_text(event.get("pid").unwrap_or(&Value::Null));
    let raw_label = args.get("labels").cloned().unwrap_or(Value::Null);
    let label = if python_truthy(&raw_label) { python_value_text(&raw_label) } else { String::new() };
    if let Some(index) = positions.get(&key).copied() {
      ordered[index].1 = label.clone();
    } else {
      positions.insert(key.clone(), ordered.len());
      ordered.push((key.clone(), label.clone()));
    }
    labels.insert(key, label);
  }
  Ok((ordered, labels))
}

fn walk_drops(event: &Map<String, Value>, phase: &Value, slim: bool) -> Result<bool, BuildFailure> {
  if phase.as_str() == Some("M") {
    let name = event.get("name").unwrap_or(&Value::Null);
    if python_truthy(name) {
      let name = name.as_str().ok_or_else(|| attribute_failure(name, "startswith"))?;
      if name.starts_with("process_") {
        return Ok(true);
      }
    }
  }
  Ok(slim && event.get("cat").and_then(Value::as_str) == Some("cpu_instant_event"))
}

fn rank_label(path: &Path) -> String {
  let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
  let lower = name.to_ascii_lowercase();
  if let Some(index) = lower.find("rank") {
    let digits: String = name[index + 4..].chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
      return format!("rank{digits}");
    }
  }
  if lower.ends_with(".json") { name[..name.len() - 5].to_string() } else { name.to_string() }
}

fn gpu_index(label: &str) -> Option<u64> {
  let lower = label.to_ascii_lowercase();
  let bytes = lower.as_bytes();
  for start in 0..bytes.len().saturating_sub(2) {
    if &bytes[start..start + 3] == b"gpu" {
      let mut index = start + 3;
      while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
      }
      let end = (index..bytes.len()).find(|position| !bytes[*position].is_ascii_digit()).unwrap_or(bytes.len());
      if end > index {
        return lower[index..end].parse().ok();
      }
    }
  }
  None
}

fn python_truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(value) => *value,
    Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
    Value::String(value) => !value.is_empty(),
    Value::Array(value) => !value.is_empty(),
    Value::Object(value) => !value.is_empty(),
  }
}

fn attribute_failure(value: &Value, attribute: &str) -> BuildFailure {
  let type_name = match value {
    Value::Null => "NoneType",
    Value::Bool(_) => "bool",
    Value::Number(number) if number.is_f64() => "float",
    Value::Number(_) => "int",
    Value::String(_) => "str",
    Value::Array(_) => "list",
    Value::Object(_) => "dict",
  };
  BuildFailure::Failed(format!("'{type_name}' object has no attribute '{attribute}'"))
}

fn python_value_text(value: &Value) -> String {
  match value {
    Value::Null => "None".to_string(),
    Value::Bool(true) => "True".to_string(),
    Value::Bool(false) => "False".to_string(),
    Value::String(value) => value.clone(),
    Value::Number(number) if number.is_f64() => {
      charliebot_core::pyfmt::float_repr(number.as_f64().expect("a float number has an f64 value"))
    }
    Value::Number(number) => number.to_string(),
    Value::Array(values) => format!("[{}]", values.iter().map(python_value_repr).collect::<Vec<_>>().join(", ")),
    Value::Object(values) => format!(
      "{{{}}}",
      values
        .iter()
        .map(|(key, value)| format!("{}: {}", charliebot_core::pyfmt::str_repr(key), python_value_repr(value)))
        .collect::<Vec<_>>()
        .join(", ")
    ),
  }
}

fn python_value_repr(value: &Value) -> String {
  match value {
    Value::String(value) => charliebot_core::pyfmt::str_repr(value),
    _ => python_value_text(value),
  }
}

fn thread_name_event(pid: &str, tid: u64, raw: &Value, rank_label: &str) -> Value {
  serde_json::json!({"ph":"M","pid":pid,"tid":tid,"name":"thread_name","args":{"name":format!("{rank_label}/{}", python_value_text(raw))}})
}

fn metadata_event(pid: &str, tid: u64, name: &str, args: Value) -> Value {
  serde_json::json!({"ph":"M","pid":pid,"tid":tid,"name":name,"args":args})
}

fn orjson_error_text(source: &[u8], error: &serde_json::Error) -> String {
  let serde_offset = if error.is_eof() { source.len() } else { byte_offset(source, error.line(), error.column()) };
  let error_text = error.to_string();
  let message = if source.is_empty() {
    "Input is a zero-length, empty document"
  } else if error.is_eof() {
    "unexpected end of data"
  } else if error_text.contains("trailing characters") {
    "unexpected content after document"
  } else if source.get(serde_offset).is_some_and(|byte| byte.is_ascii_digit())
    && serde_offset > 0
    && source[serde_offset - 1] == b'0'
  {
    "number with leading zero is not allowed"
  } else if error_text.contains("key must be a string") {
    "unexpected character, expected a string key"
  } else if error_text.contains("expected `:`") {
    "unexpected character, expected ':' after key"
  } else if error_text.contains("expected `,` or `}`") {
    "unexpected character, expected ',' or '}'"
  } else if error_text.contains("expected `,` or `]`") {
    "unexpected character, expected ',' or ']'"
  } else if error_text.contains("invalid escape") {
    "invalid escaped sequence in string"
  } else if error_text.contains("control character") {
    "unexpected control character in string"
  } else if error_text.contains("number out of range") {
    "number is infinity when parsed as double"
  } else if source.get(serde_offset.saturating_sub(1)) == Some(&b'-') {
    "no digit after sign"
  } else {
    "unexpected character, expected a JSON value"
  };
  let offset = match message {
    "invalid escaped sequence in string" => string_escape_start(source, serde_offset),
    "number with leading zero is not allowed" => serde_offset.saturating_sub(1),
    "unexpected control character in string" => string_control_offset(source, serde_offset),
    "number is infinity when parsed as double" => number_start(source, serde_offset),
    _ => serde_offset,
  };
  let (line, column, char_offset) = error_position(source, offset);
  format!("{message}: line {line} column {column} (char {char_offset})")
}

fn string_escape_start(source: &[u8], end: usize) -> usize {
  let end = end.min(source.len());
  let mut in_string = false;
  let mut escaped = false;
  let mut escape_start = None;
  let mut index = 0;
  while let Some(offset) = memchr2(b'"', b'\\', &source[index..end]) {
    index += offset;
    let byte = source[index];
    if !in_string {
      if byte == b'"' {
        in_string = true;
      }
    } else if escaped {
      escaped = false;
    } else if byte == b'\\' {
      escaped = true;
      escape_start = Some(index);
    } else if byte == b'"' {
      in_string = false;
    }
    index += 1;
  }
  escape_start.unwrap_or_else(|| end.saturating_sub(1))
}

fn string_control_offset(source: &[u8], end: usize) -> usize {
  let end = end.min(source.len());
  let mut in_string = false;
  let mut escaped = false;
  let mut index = 0;
  while index < end {
    if !in_string {
      let Some(offset) = memchr(b'"', &source[index..end]) else {
        break;
      };
      index += offset + 1;
      in_string = true;
      continue;
    }
    if escaped {
      escaped = false;
      index += 1;
      continue;
    }
    let remaining = &source[index..end];
    let event = [memchr2(b'"', b'\\', remaining), first_control_offset(remaining)].into_iter().flatten().min();
    let Some(offset) = event else {
      break;
    };
    index += offset;
    match source[index] {
      b'\\' => {
        escaped = true;
        index += 1;
      }
      b'"' => {
        in_string = false;
        index += 1;
      }
      _ => return index,
    }
  }
  end
}

fn first_control_offset(source: &[u8]) -> Option<usize> {
  (0..0x20).filter_map(|byte| memchr(byte, source)).min()
}

fn number_start(source: &[u8], end: usize) -> usize {
  let prefix = &source[..end.min(source.len())];
  [
    memrchr(b',', prefix),
    memrchr(b'[', prefix),
    memrchr(b'{', prefix),
    memrchr(b':', prefix),
    memrchr(b' ', prefix),
    memrchr(b'\t', prefix),
    memrchr(b'\n', prefix),
    memrchr(b'\r', prefix),
  ]
  .into_iter()
  .flatten()
  .max()
  .map_or(0, |delimiter| delimiter + 1)
}

fn byte_offset(source: &[u8], line: usize, column: usize) -> usize {
  let line_start =
    if line <= 1 { 0 } else { memchr_iter(b'\n', source).nth(line - 2).map_or(source.len(), |offset| offset + 1) };
  (line_start + column.saturating_sub(1)).min(source.len())
}

fn error_position(source: &[u8], offset: usize) -> (usize, usize, usize) {
  let prefix = &source[..offset.min(source.len())];
  let line = prefix.iter().filter(|byte| **byte == b'\n').count() + 1;
  let line_start = prefix.iter().rposition(|byte| *byte == b'\n').map_or(0, |index| index + 1);
  let line_column = std::str::from_utf8(&source[line_start..offset.min(source.len())])
    .map_or(offset.saturating_sub(line_start), |text| text.chars().count());
  let char_offset = std::str::from_utf8(prefix).map_or(offset, |text| text.chars().count());
  (line, line_column + 1, char_offset)
}

#[cfg(test)]
mod tests {
  use std::io::Read;

  use flate2::read::GzDecoder;
  use tempfile::tempdir;

  use super::*;

  fn venv_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap().join(".venv/bin")
  }

  fn write_trace(path: &Path, trace: &str) {
    fs::write(path, trace).unwrap();
  }

  #[test]
  fn json_parse_errors_keep_python_message_and_character_positions() {
    let cases: [(&[u8], &str); 5] = [
      (br#"{"x":"\q"}"#, "invalid escaped sequence in string: line 1 column 7 (char 6)"),
      (b"{\"x\":\"a\nb\"}", "unexpected control character in string: line 1 column 8 (char 7)"),
      (br#"{"x":01}"#, "number with leading zero is not allowed: line 1 column 6 (char 5)"),
      (br#"{"x":1e9999}"#, "number is infinity when parsed as double: line 1 column 6 (char 5)"),
      ("{\"é\": }".as_bytes(), "unexpected character, expected a JSON value: line 1 column 7 (char 6)"),
    ];
    for (source, expected) in cases {
      let error = serde_json::from_slice::<Value>(source).expect_err("the input is invalid JSON");
      assert_eq!(orjson_error_text(source, &error), expected, "source {source:?}");
    }
  }

  #[test]
  fn direct_pass_compresses_original_bytes_and_rejects_non_traces() {
    let directory = tempdir().unwrap();
    let trace = directory.path().join("trace.json");
    let output = directory.path().join("trace.json.gz");
    let original = "{\"traceEvents\":[{\"ph\":\"X\",\"pid\":1,\"tid\":2}],\"note\":\"é\"} \n".as_bytes();
    fs::write(&trace, original).unwrap();
    direct_pass(&trace, &output, &venv_bin()).unwrap();
    let mut decompressed = Vec::new();
    GzDecoder::new(File::open(&output).unwrap()).read_to_end(&mut decompressed).unwrap();
    assert_eq!(decompressed, original);

    write_trace(&trace, "{\"analysis\":[]}");
    assert!(matches!(direct_pass(&trace, &output, &venv_bin()), Err(BuildFailure::NotATrace(_))));
  }

  #[test]
  fn merge_rewrites_ids_labels_flows_and_slim_arguments() {
    let directory = tempdir().unwrap();
    let trace = directory.path().join("trace_rank3.json");
    write_trace(
      &trace,
      r#"{"deviceProperties":[{"gpu":0}],"traceEvents":[{"ph":"M","name":"process_labels","pid":7,"args":{"labels":"GPU 2"}},{"ph":"X","pid":7,"tid":"4","name":"span","args":{"stream":2,"large":true}},{"ph":"s","pid":7,"tid":4,"id":"f","cat":"cpu_instant_event","name":"flow"}]}"#,
    );
    let merged = merge_traces(std::slice::from_ref(&trace), true).unwrap();
    let result: Value = serde_json::from_slice(&merged).unwrap();
    let events = result["traceEvents"].as_array().unwrap();
    assert_eq!(events[0]["name"], "thread_name");
    assert_eq!(events[0]["args"]["name"], "rank3/4");
    assert_eq!(events[1]["pid"], "rank3/GPU 2");
    assert_eq!(events[1]["args"], serde_json::json!({"stream":2}));
    assert_eq!(events.len(), 5);
    assert_eq!(events[2]["name"], "process_name");
  }

  #[test]
  fn multi_merge_skips_non_traces_and_fails_when_every_member_is_skipped() {
    let directory = tempdir().unwrap();
    let trace = directory.path().join("trace_rank0.json");
    let sidecar = directory.path().join("analysis.json");
    write_trace(&trace, "{\"traceEvents\":[{\"ph\":\"X\",\"pid\":1,\"tid\":1}]}");
    write_trace(&sidecar, "{\"analysis\":[]}");
    let merged = build_multi_trace_merge(&[trace.clone(), sidecar.clone()], false).unwrap();
    assert!(String::from_utf8(merged).unwrap().contains("thread_name"));
    let failure = build_multi_trace_merge(std::slice::from_ref(&sidecar), false).unwrap_err();
    assert_eq!(
      failure.message(),
      format!("multi-trace merge rejected every member as a non-Chrome-JSON trace: {}", sidecar.display())
    );
  }
}

#[cfg(test)]
mod differential_tests {
  use std::fs;
  use std::fs::File;
  use std::io::Read;
  use std::path::{Path, PathBuf};
  use std::time::{Duration, SystemTime};

  use flate2::read::GzDecoder;
  use serde::Serialize;
  use serde_json::Value;
  use tempfile::tempdir;

  use super::*;

  const FIXED_MTIME_NS: u64 = 1_700_000_000_123_456_789;

  fn venv_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap().join(".venv/bin")
  }

  fn actual_artifact(builder: &str, paths: &[PathBuf], slim: bool, output: &Path) -> Result<Vec<u8>, String> {
    match builder {
      "merge" => {
        let document = merge_traces(paths, slim).map_err(|error| error.message().to_string())?;
        gzip_bytes(&document, output, &venv_bin()).map_err(|error| error.message().to_string())?;
      }
      "multi" => {
        let document = build_multi_trace_merge(paths, slim).map_err(|error| error.message().to_string())?;
        gzip_bytes(&document, output, &venv_bin()).map_err(|error| error.message().to_string())?;
      }
      "direct" => direct_pass(&paths[0], output, &venv_bin()).map_err(|error| error.message().to_string())?,
      _ => panic!("unknown differential builder: {builder}"),
    }
    fs::read(output).map_err(|error| error.to_string())
  }

  fn decompress(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    GzDecoder::new(bytes).read_to_end(&mut output).unwrap();
    output
  }

  #[derive(Serialize)]
  struct PythonCacheKey<'a> {
    paths: Vec<(String, u64, i64)>,
    slim: bool,
    mode: &'a str,
  }

  #[test]
  fn recorded_python_trace_builders_match_seeded_corpus() {
    let bytes = include_bytes!("../tests/fixtures/python_trace_differential.json.gz");
    let mut decoder = GzDecoder::new(bytes.as_slice());
    let mut contents = Vec::new();
    decoder.read_to_end(&mut contents).unwrap();
    let fixture: Value = serde_json::from_slice(&contents).unwrap();
    assert_eq!(fixture["source"]["helper"], "rust/crates/features/trace/tests/differential.py");
    assert_eq!(fixture["source"]["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
    assert_eq!(fixture["source"]["mtime_ns"], "$FIXED_MTIME_NS");

    let root = tempdir().unwrap();
    for (relative, encoded) in fixture["corpus"].as_object().unwrap() {
      let path = root.path().join(relative);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(&path, decode_hex(encoded.as_str().unwrap())).unwrap();
      let modified =
        SystemTime::UNIX_EPOCH + Duration::new(FIXED_MTIME_NS / 1_000_000_000, (FIXED_MTIME_NS % 1_000_000_000) as u32);
      File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    }

    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 29);
    let mut comparisons = 0;
    let mut differences = Vec::new();
    for case in cases {
      let name = case["name"].as_str().unwrap();
      let paths: Vec<PathBuf> =
        case["paths"].as_array().unwrap().iter().map(|path| root.path().join(path.as_str().unwrap())).collect();
      let slim = case["slim"].as_bool().unwrap();
      for builder in ["merge", "multi", "direct"] {
        let reference = &case["references"][builder];
        let output = tempdir().unwrap();
        let artifact = output.path().join(format!("{builder}.json.gz"));
        let actual = actual_artifact(builder, &paths, slim, &artifact);
        if let Some(expected_error) = reference.get("error") {
          match actual {
            Err(actual_error) if normalize_root(&actual_error, root.path()) == expected_error.as_str().unwrap() => {}
            Err(actual_error) => differences.push(format!(
              "error text differs for {name}/{builder}: {:?} != {:?}",
              normalize_root(&actual_error, root.path()),
              expected_error.as_str().unwrap()
            )),
            Ok(_) => differences.push(format!("Rust succeeded for failing Python case {name}/{builder}")),
          }
        } else {
          let expected = decode_hex(reference["gzip_hex"].as_str().unwrap());
          match actual {
            Ok(actual) => {
              if actual != expected {
                differences.push(format!("gzip bytes differ for {name}/{builder}"));
              }
              if decompress(&actual) != decode_hex(reference["json_hex"].as_str().unwrap()) {
                differences.push(format!("JSON bytes differ for {name}/{builder}"));
              }
            }
            Err(error) => differences.push(format!("unexpected Rust error for {name}/{builder}: {error}")),
          }
        }
        comparisons += 1;
      }
      for mode in ["gzip", "merge"] {
        let key = &case["cache_keys"][mode];
        assert_eq!(key["recorded_python_key"], "$SHA256(paths, slim, mode)");
        let expected = python_cache_key(key, root.path(), slim, mode);
        match merge_cache_key(&paths, slim, mode) {
          Ok(actual) if actual == expected => {}
          Ok(actual) => differences.push(format!("cache key differs for {name}/{mode}: {actual} != {expected}")),
          Err(error) => differences.push(format!("cache key failed for {name}/{mode}: {}", error.message())),
        }
      }
    }
    println!("matched {comparisons} builder results and {} cache keys", cases.len() * 2);
    assert!(differences.is_empty(), "{} differential mismatches:\n{}", differences.len(), differences.join("\n"));
  }

  fn python_cache_key(reference: &Value, root: &Path, slim: bool, mode: &str) -> String {
    let paths = reference["paths"]
      .as_array()
      .unwrap()
      .iter()
      .map(|entry| {
        assert_eq!(entry["mtime_ns"], "$FIXED_MTIME_NS");
        let path = root.join(entry["path"].as_str().unwrap());
        (path.to_string_lossy().into_owned(), entry["size"].as_u64().unwrap(), FIXED_MTIME_NS as i64)
      })
      .collect();
    let payload = PythonCacheKey { paths, slim, mode };
    format!("{:x}", Sha256::digest(serde_json::to_vec(&payload).unwrap()))
  }

  fn normalize_root(value: &str, root: &Path) -> String {
    value.replace(root.to_str().unwrap(), "$TMPROOT")
  }

  fn decode_hex(value: &str) -> Vec<u8> {
    value
      .as_bytes()
      .as_chunks::<2>()
      .0
      .iter()
      .map(|pair| {
        let high = (pair[0] as char).to_digit(16).unwrap();
        let low = (pair[1] as char).to_digit(16).unwrap();
        ((high << 4) | low) as u8
      })
      .collect()
  }
}
