//! The startup-only fact checkpoint of one session's history.
//!
//! A boot's fact scan folds a session's archived events and then its live log into the node and run facts. The
//! checkpoint holds that fold. The fold holds the task facts, the run facts and the newest prompt fact of each scope.
//! Beside them sit the archive files, the byte offset of the live log they cover, and two digests of the covered
//! prefix. A later boot reads those facts and folds only the bytes appended past the offset. Its read cost then
//! follows the folded state it proves and the appended tail, not the history the state covers.
//!
//! The folded facts do not carry the event index, which is one entry per event and most of the covered history. The
//! checkpoint keeps the events themselves as the scan kept them, one compact line per kept event in log order. Beside
//! them sits a sorted `(id hash, line offset)` table over the lines that carry an id. The event index is built from
//! those two sections only when a reader asks for an event by reference. A membership test costs one binary search and
//! one line's parse. A fold of the appended tail compares each of its ids against the table without parsing the rest.
//!
//! The file is startup-only and is never synced. Every read validates it against its sources before use. The sources
//! are the archived extent, each archive file's name, size and modification time, and the live log's length (which
//! must cover the offset). They also include the line boundary at the offset and the SHA-256 of the first and last
//! [`WINDOW_BYTES`] of the covered prefix. A checkpoint that is absent, truncated, torn, stale or written against
//! another log fails that check, and the boot scans the log whole. That full scan is also the recovery a missing
//! checkpoint gives, so a torn or stale copy costs a slower boot and nothing else.

use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use charliebot_core::fsutil;
use charliebot_core::fsutil::FileBytes;
use charliebot_core::text_repr::{self, JsonStyle};
use serde_json::{Map, Value, json};

use crate::compact::CompactValue;
use crate::control_events::sha256_bytes_hex;
use crate::event_types::{ChatEvent, FileIdentity, SharedChatEvent};
use crate::run_facts::RunFacts;
use crate::task_tree_state::{
  EventIdIndex, LineDecoder, LivePromptChanges, NodeFacts, PrefixIndex, TaskFacts, event_id_hash,
};
use crate::{StoreError, StoreResult};

/// The format version of the checkpoint file. A file of another version is not read.
const CHECKPOINT_VERSION: u64 = 2;
/// The bytes of each end of the covered prefix whose digest the checkpoint keeps. A rewrite that leaves both windows
/// and the length intact would have to reproduce the prefix's ends, which no writer here does.
pub(crate) const WINDOW_BYTES: usize = 4096;
/// One event-id table entry: a little-endian `u64` id hash and a `u32` offset of the event's line in the index section.
pub(crate) const TABLE_ENTRY_BYTES: usize = 12;
/// The checkpoint's JSON style: one object per line, no spaces between members.
const CHECKPOINT_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ",", key_separator: ":", indent: None };

/// One archive file the checkpoint's facts folded, identified by the values a rename or append changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArchiveMark {
  pub(crate) name: String,
  pub(crate) size: u64,
  pub(crate) mtime_ns: i128,
}

impl ArchiveMark {
  pub(crate) fn of(path: &Path) -> StoreResult<Option<Self>> {
    let Some(identity) = FileIdentity::read(path)? else { return Ok(None) };
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default().to_string();
    Ok(Some(Self { name, size: identity.size(), mtime_ns: identity.mtime_ns }))
  }
}

/// The facts a scan folded from the prefix of one history, and the prefix they cover.
pub(crate) struct FactCheckpoint {
  /// The archived events among the kept lines. The scan keeps exactly the session's `archive_offset` events from its
  /// archive files, so this is also where the live events' fold begins.
  pub(crate) archived: usize,
  /// The live events among the kept lines.
  pub(crate) covered: usize,
  /// The live-log bytes the prefix covers. Its last byte is a line end.
  pub(crate) offset: u64,
  head_sha: String,
  tail_sha: String,
  pub(crate) archive_marks: Vec<ArchiveMark>,
  /// The bytes of the index section, which the file holds last.
  index_bytes: u64,
  /// The unique event ids the table indexes, and so the table's entry count.
  index_events: usize,
  /// The table's byte count, `index_events * TABLE_ENTRY_BYTES`.
  table_bytes: u64,
}

/// The facts a checkpoint file holds, with its event index left unparsed behind the facts.
pub(crate) struct ParsedCheckpoint {
  pub(crate) checkpoint: FactCheckpoint,
  pub(crate) facts: NodeFacts,
  pub(crate) runs: RunFacts,
  pub(crate) prompt_changes: LivePromptChanges,
}

impl FactCheckpoint {
  /// Whether this checkpoint's facts cover a prefix of `live`. The archived extent and archive files are the ones it
  /// folded, the file still holds its offset bytes, a line ends there, and both prefix windows keep their digests.
  pub(crate) fn covers(&self, live: &[u8], archives: usize, archive_marks: &[ArchiveMark]) -> bool {
    let offset = self.offset as usize;
    self.archived == archives
      && self.archive_marks == archive_marks
      && live.len() >= offset
      && (offset == 0 || live[offset - 1] == b'\n')
      && self.head_sha == window_sha(live, 0)
      && self.tail_sha == window_sha(live, offset)
  }

  /// The checkpoint a file's bytes hold, with the byte offset where its facts line begins. It is `None` when the bytes
  /// are not a checkpoint of this version or do not hold the sections their header names.
  pub(crate) fn parse(bytes: &Arc<FileBytes>, decode: LineDecoder) -> Option<ParsedCheckpoint> {
    let header_end = bytes.iter().position(|byte| *byte == b'\n')?;
    let value: Value = serde_json::from_slice(&bytes[..header_end]).ok()?;
    let object = value.as_object()?;
    if object.get("version")?.as_u64()? != CHECKPOINT_VERSION {
      return None;
    }
    let archive_marks = object
      .get("archives")?
      .as_array()?
      .iter()
      .map(|mark| {
        let mark = mark.as_object()?;
        Some(ArchiveMark {
          name: mark.get("name")?.as_str()?.to_string(),
          size: mark.get("size")?.as_u64()?,
          mtime_ns: i128::from(mark.get("mtime_ns")?.as_i64()?),
        })
      })
      .collect::<Option<Vec<_>>>()?;
    let checkpoint = Self {
      archived: usize::try_from(object.get("archived")?.as_u64()?).ok()?,
      covered: usize::try_from(object.get("covered")?.as_u64()?).ok()?,
      offset: object.get("offset")?.as_u64()?,
      head_sha: object.get("head_sha")?.as_str()?.to_string(),
      tail_sha: object.get("tail_sha")?.as_str()?.to_string(),
      archive_marks,
      index_bytes: object.get("index_bytes")?.as_u64()?,
      index_events: usize::try_from(object.get("index_events")?.as_u64()?).ok()?,
      table_bytes: object.get("table_bytes")?.as_u64()?,
    };
    if checkpoint.table_bytes != (checkpoint.index_events as u64) * (TABLE_ENTRY_BYTES as u64) {
      return None;
    }
    let facts_start = header_end + 1;
    let facts_end = facts_start + bytes[facts_start..].iter().position(|byte| *byte == b'\n')?;
    let table_start = facts_end + 1;
    let lines_start = table_start.checked_add(usize::try_from(checkpoint.table_bytes).ok()?)?;
    let lines_end = lines_start.checked_add(usize::try_from(checkpoint.index_bytes).ok()?)?;
    // A file shorter than its header names was torn by a writer that stopped; a longer one was extended by hand.
    if lines_end != bytes.len() || (lines_end > lines_start && bytes[lines_end - 1] != b'\n') {
      return None;
    }
    let facts: Value = serde_json::from_slice(&bytes[facts_start..facts_end]).ok()?;
    let TaskCheckpointFacts { mut facts, runs, prompt_changes } = task_facts_from_value(&facts)?;
    facts.events_by_id = EventIdIndex::from_prefix(PrefixIndex {
      bytes: Arc::clone(bytes),
      lines_start,
      lines_end,
      table_start,
      events: checkpoint.index_events,
      decode,
    });
    Some(ParsedCheckpoint { checkpoint, facts: NodeFacts { task: Ok(facts) }, runs, prompt_changes })
  }
}

/// The facts line of a checkpoint.
struct TaskCheckpointFacts {
  facts: TaskFacts,
  runs: RunFacts,
  prompt_changes: LivePromptChanges,
}

/// The digest of the [`WINDOW_BYTES`] window that ends at `end` (or starts at 0 when that is shorter).
fn window_sha(bytes: &[u8], end: usize) -> String {
  let start = end.saturating_sub(WINDOW_BYTES);
  sha256_bytes_hex(&bytes[start..end])
}

/// The checkpoint file a scan of `file` leaves: the facts it folded and the events it kept, which the file holds as
/// the event index. `None` when the fold failed, because a failed fold writes no checkpoint and rescans every boot.
pub(crate) struct CheckpointFile {
  header: String,
  facts: String,
  table: Vec<(u64, u32)>,
  lines: String,
}

/// One scan's fold and the events it kept: what a checkpoint file writes.
pub(crate) struct CheckpointFold<'a> {
  pub(crate) archived: usize,
  pub(crate) covered: usize,
  pub(crate) archive_marks: &'a [ArchiveMark],
  pub(crate) file: &'a [u8],
  pub(crate) facts: &'a NodeFacts,
  pub(crate) runs: &'a RunFacts,
  pub(crate) prompt_changes: &'a LivePromptChanges,
  pub(crate) kept: &'a [&'a SharedChatEvent],
}

impl CheckpointFile {
  pub(crate) fn new(fold: CheckpointFold<'_>) -> Option<Self> {
    let CheckpointFold { archived, covered, archive_marks, file, facts, runs, prompt_changes, kept } = fold;
    let task = facts.task().ok()?;
    let (lines, table) = encode_index(kept);
    let mut header = Map::new();
    header.insert("version".to_string(), json!(CHECKPOINT_VERSION));
    header.insert("archived".to_string(), json!(archived));
    header.insert("covered".to_string(), json!(covered));
    header.insert("offset".to_string(), json!(file.len() as u64));
    header.insert("head_sha".to_string(), Value::String(window_sha(file, 0)));
    header.insert("tail_sha".to_string(), Value::String(window_sha(file, file.len())));
    header.insert(
      "archives".to_string(),
      Value::Array(
        archive_marks
          .iter()
          .map(|mark| {
            json!({"name": mark.name, "size": mark.size, "mtime_ns": i64::try_from(mark.mtime_ns).unwrap_or_default()})
          })
          .collect(),
      ),
    );
    header.insert("index_bytes".to_string(), json!(lines.len() as u64));
    header.insert("index_events".to_string(), json!(table.len()));
    header.insert("table_bytes".to_string(), json!(table.len() * TABLE_ENTRY_BYTES));
    Some(Self {
      header: text_repr::json_dumps(&Value::Object(header), &CHECKPOINT_JSON),
      facts: facts_to_value(task, runs, prompt_changes),
      table,
      lines,
    })
  }

  /// Write the file to `path`. The file is startup-only: a torn or stale copy fails validation and the next boot
  /// scans the log whole, so the write takes no sync.
  pub(crate) fn write(&self, path: &Path) -> StoreResult<()> {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).map_err(|error| StoreError::new(format!("{}: {error}", parent.display())))?;
    }
    fsutil::atomic_write_stream(path, |stream| {
      stream.write_all(self.header.as_bytes())?;
      stream.write_all(b"\n")?;
      stream.write_all(self.facts.as_bytes())?;
      stream.write_all(b"\n")?;
      for (hash, offset) in &self.table {
        stream.write_all(&hash.to_le_bytes())?;
        stream.write_all(&offset.to_le_bytes())?;
      }
      stream.write_all(self.lines.as_bytes())
    })
    .map_err(|error: io::Error| StoreError::new(format!("{}: {error}", path.display())))
  }
}

/// The kept events' lines and the table that points into them. A table entry is the event id's stable hash and the
/// line's offset in the index section, taken at the event's first occurrence. The fold inserts an id once.
fn encode_index(kept: &[&SharedChatEvent]) -> (String, Vec<(u64, u32)>) {
  let mut lines = String::new();
  let mut table: Vec<(u64, u32)> = Vec::new();
  let mut ids: std::collections::HashSet<String> = std::collections::HashSet::new();
  for event in kept {
    let offset = u32::try_from(lines.len()).expect("one session's index section fits in four gigabytes");
    if let Some(id) = event.get("id").and_then(CompactValue::as_str)
      && ids.insert(id.to_string())
    {
      table.push((event_id_hash(id), offset));
    }
    encode_event(event, &mut lines);
    lines.push('\n');
  }
  table.sort_unstable();
  (lines, table)
}

/// The first byte of a stand-in line: [`STAND_IN_TAG`] when it carries the event id, [`TYPE_ONLY_TAG`] when not.
pub(crate) const STAND_IN_TAG: u8 = b'S';
/// The first byte of a stand-in line whose event has no string id.
pub(crate) const TYPE_ONLY_TAG: u8 = b'T';

/// Append one kept event's line, without its newline.
fn encode_event(event: &SharedChatEvent, line: &mut String) {
  let mut stand_in = String::new();
  if !encode_stand_in(event, &mut stand_in) {
    stand_in.push_str(&text_repr::json_dumps(&event.to_value(), &CHECKPOINT_JSON));
  }
  line.push_str(&stand_in);
}

/// The event lines the checkpoint file holds, in fold order. A chat-content stand-in is one [`STAND_IN_TAG`] line,
/// which the resume builds without a JSON parse; any other event keeps its compact JSON line.
#[cfg(test)]
pub(crate) fn encode_events<'a>(events: impl IntoIterator<Item = &'a SharedChatEvent>) -> String {
  let mut text = String::new();
  for event in events {
    encode_event(event, &mut text);
    text.push('\n');
  }
  text
}

/// Write the stand-in line for an event that is one. The line is an object holding exactly its string `id` and `type`,
/// in that order, whose text holds no separator or line break. Returns whether the event was written.
fn encode_stand_in(event: &SharedChatEvent, line: &mut String) -> bool {
  let crate::compact::CompactValue::Object(entries) = event.as_ref() else {
    return false;
  };
  let (id, event_type) = match entries.as_ref() {
    [id, event_type] if id.key.as_str() == "id" && event_type.key.as_str() == "type" => {
      let (Some(id), Some(event_type)) = (id.value.as_str(), event_type.value.as_str()) else {
        return false;
      };
      (Some(id), event_type)
    }
    [event_type] if event_type.key.as_str() == "type" => {
      let Some(event_type) = event_type.value.as_str() else {
        return false;
      };
      (None, event_type)
    }
    _ => return false,
  };
  let plain = |text: &str| !text.bytes().any(|byte| matches!(byte, b'\t' | b'\n' | b'\r'));
  if !plain(event_type) || id.is_some_and(|id| !plain(id)) {
    return false;
  }
  match id {
    Some(id) => {
      line.push(STAND_IN_TAG as char);
      line.push_str(id);
      line.push('\t');
    }
    None => line.push(TYPE_ONLY_TAG as char),
  }
  line.push_str(event_type);
  true
}

/// The checkpoint's facts line: the folded task and run facts and the newest prompt fact of each scope.
fn facts_to_value(facts: &TaskFacts, runs: &RunFacts, prompt_changes: &LivePromptChanges) -> String {
  let mut object = Map::new();
  object.insert("facts".to_string(), task_facts_value(facts));
  object.insert("runs".to_string(), run_facts_value(runs));
  object.insert(
    "prompt_subtree".to_string(),
    prompt_changes.subtree.as_ref().map_or(Value::Null, |event| event.to_value()),
  );
  object.insert("prompt_node".to_string(), prompt_changes.node.as_ref().map_or(Value::Null, |event| event.to_value()));
  text_repr::json_dumps(&Value::Object(object), &CHECKPOINT_JSON)
}

/// The folded task facts as one object, without the event index, which the checkpoint's lines hold.
fn task_facts_value(facts: &TaskFacts) -> Value {
  let mut run_outcomes = Map::new();
  for (run_id, outcome) in &facts.run_outcomes {
    run_outcomes.insert(run_id.clone(), Value::String(outcome.clone()));
  }
  let mut run_finish_positions = Map::new();
  for (run_id, position) in &facts.run_finish_positions {
    run_finish_positions.insert(run_id.clone(), json!(position));
  }
  let mut first_close_by_request = Map::new();
  for (request_id, event) in &facts.first_close_by_request {
    first_close_by_request.insert(request_id.clone(), event.to_value());
  }
  let sorted = |set: &std::collections::HashSet<String>| {
    let mut values: Vec<&String> = set.iter().collect();
    values.sort();
    Value::Array(values.into_iter().map(|value| Value::String(value.clone())).collect())
  };
  let mut delivered_reports: Vec<Vec<String>> =
    facts.delivered_reports.iter().map(|(child, event)| vec![child.clone(), event.clone()]).collect();
  delivered_reports.sort();
  json!({
    "task_state": facts.task_state,
    "has_user_request": facts.has_user_request,
    "run_outcomes": run_outcomes,
    "run_outcome_string_none_ids": sorted(&facts.run_outcome_string_none_ids),
    "run_finish_positions": run_finish_positions,
    "confirmed_input_ids": sorted(&facts.confirmed_input_ids),
    "input_candidates": events_value(&facts.input_candidates),
    "imported_pending_ids": sorted(&facts.imported_pending_ids),
    "boundary_index": facts.boundary_index.map_or(Value::Null, |index| json!(index)),
    "boundary_open": facts.boundary_open,
    "close_events": events_value(&facts.close_events),
    "close_requests": events_value(&facts.close_requests),
    "close_request_ids": sorted(&facts.close_request_ids),
    "first_close_by_request": first_close_by_request,
    "delivered_reports": delivered_reports,
    "covered": facts.covered,
  })
}

/// The facts line as its facts, run facts and prompt changes, with an empty event index the checkpoint's sections fill.
fn task_facts_from_value(value: &Value) -> Option<TaskCheckpointFacts> {
  let object = value.as_object()?;
  let facts = object.get("facts")?;
  let field = |name: &str| facts.get(name);
  let run_outcomes = field("run_outcomes")?
    .as_object()?
    .iter()
    .map(|(run_id, outcome)| outcome.as_str().map(|outcome| (run_id.clone(), outcome.to_string())))
    .collect::<Option<std::collections::HashMap<_, _>>>()?;
  let run_finish_positions = field("run_finish_positions")?
    .as_object()?
    .iter()
    .map(|(run_id, position)| usize_of(position).map(|position| (run_id.clone(), position)))
    .collect::<Option<std::collections::HashMap<_, _>>>()?;
  let first_close_by_request = field("first_close_by_request")?
    .as_object()?
    .iter()
    .map(|(request_id, event)| event.as_object().map(|_| (request_id.clone(), Arc::new(ChatEvent::from_value(event)))))
    .collect::<Option<std::collections::HashMap<_, _>>>()?;
  let delivered_reports = field("delivered_reports")?
    .as_array()?
    .iter()
    .map(|pair| match pair.as_array()?.as_slice() {
      [child, event] => Some((child.as_str()?.to_string(), event.as_str()?.to_string())),
      _ => None,
    })
    .collect::<Option<std::collections::HashSet<_>>>()?;
  let facts = TaskFacts {
    task_state: field("task_state")?.as_str()?.to_string(),
    has_user_request: field("has_user_request")?.as_bool()?,
    run_outcomes,
    run_outcome_string_none_ids: strings_of(field("run_outcome_string_none_ids")?)?.into_iter().collect(),
    run_finish_positions,
    confirmed_input_ids: strings_of(field("confirmed_input_ids")?)?.into_iter().collect(),
    input_candidates: events_from_value(field("input_candidates")?)?,
    imported_pending_ids: strings_of(field("imported_pending_ids")?)?.into_iter().collect(),
    boundary_index: match field("boundary_index")? {
      Value::Null => None,
      value => Some(usize_of(value)?),
    },
    boundary_open: field("boundary_open")?.as_bool()?,
    close_events: events_from_value(field("close_events")?)?,
    close_requests: events_from_value(field("close_requests")?)?,
    close_request_ids: strings_of(field("close_request_ids")?)?.into_iter().collect(),
    first_close_by_request,
    delivered_reports,
    events_by_id: EventIdIndex::default(),
    covered: usize_of(field("covered")?)?,
  };
  Some(TaskCheckpointFacts {
    facts,
    runs: run_facts_from_value(object.get("runs")?)?,
    prompt_changes: LivePromptChanges {
      subtree: event_from_value(object.get("prompt_subtree")?)?,
      node: event_from_value(object.get("prompt_node")?)?,
    },
  })
}

fn events_value(events: &[SharedChatEvent]) -> Value {
  Value::Array(events.iter().map(|event| event.to_value()).collect())
}

fn events_from_value(value: &Value) -> Option<Vec<SharedChatEvent>> {
  value.as_array()?.iter().map(|item| item.as_object().map(|_| Arc::new(ChatEvent::from_value(item)))).collect()
}

fn event_from_value(value: &Value) -> Option<Option<SharedChatEvent>> {
  match value {
    Value::Null => Some(None),
    Value::Object(_) => Some(Some(Arc::new(ChatEvent::from_value(value)))),
    _ => None,
  }
}

fn usize_of(value: &Value) -> Option<usize> {
  usize::try_from(value.as_u64()?).ok()
}

fn strings_of(value: &Value) -> Option<Vec<String>> {
  value.as_array()?.iter().map(|item| item.as_str().map(str::to_string)).collect()
}

fn run_facts_value(facts: &RunFacts) -> Value {
  let mut outcomes = Map::new();
  for (run_id, outcome) in &facts.outcomes {
    outcomes.insert(run_id.clone(), outcome.as_ref().map_or(Value::Null, |outcome| Value::String(outcome.clone())));
  }
  let mut stop_requests = Map::new();
  for (run_id, request_ids) in &facts.stop_requests {
    let mut ids: Vec<Option<String>> = request_ids.iter().cloned().collect();
    ids.sort();
    stop_requests
      .insert(run_id.clone(), Value::Array(ids.into_iter().map(|id| id.map_or(Value::Null, Value::String)).collect()));
  }
  json!({"outcomes": outcomes, "stop_requests": stop_requests})
}

fn run_facts_from_value(value: &Value) -> Option<RunFacts> {
  let object = value.as_object()?;
  let outcomes = object
    .get("outcomes")?
    .as_object()?
    .iter()
    .map(|(run_id, outcome)| {
      let outcome = match outcome {
        Value::Null => None,
        Value::String(outcome) => Some(outcome.clone()),
        _ => return None,
      };
      Some((run_id.clone(), outcome))
    })
    .collect::<Option<std::collections::HashMap<_, _>>>()?;
  let stop_requests = object
    .get("stop_requests")?
    .as_object()?
    .iter()
    .map(|(run_id, ids)| {
      let ids = ids
        .as_array()?
        .iter()
        .map(|id| match id {
          Value::Null => Some(None),
          Value::String(id) => Some(Some(id.clone())),
          _ => None,
        })
        .collect::<Option<std::collections::HashSet<_>>>()?;
      Some((run_id.clone(), ids))
    })
    .collect::<Option<std::collections::HashMap<_, _>>>()?;
  Some(RunFacts {
    outcomes,
    stop_requests,
    #[cfg(test)]
    observed_events: 0,
  })
}
