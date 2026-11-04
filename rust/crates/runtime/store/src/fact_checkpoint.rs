//! The startup-only fact checkpoint of one session's history.
//!
//! A boot's fact scan folds a session's archived events and then its live log into the node and run facts. The
//! checkpoint holds that fold: the facts, the archive files they folded, the byte offset of the live log they cover
//! and two digests of the covered prefix. A later boot reads the checkpoint and folds only the bytes appended past
//! its offset, so it reaches the same facts while reading the appended bytes instead of the whole log.
//!
//! The file is startup-only and is never synced. Every read validates it against its sources before use: the archived
//! extent, each archive file's name, size and modification time, the live log's length (which must cover the offset),
//! the line boundary at the offset, and the SHA-256 of the first and last [`WINDOW_BYTES`] of the covered prefix. A
//! checkpoint that is absent, truncated, torn, stale or written against another log fails that check, and the boot
//! scans the log whole. That full scan is also the recovery a missing checkpoint gives, so a torn or stale copy costs
//! a slower boot and nothing else.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use charliebot_core::fsutil;
use serde_json::{Map, Value, json};

use crate::control_events::sha256_bytes_hex;
use crate::event_types::{ChatEvent, FileIdentity, SharedChatEvent};
use crate::run_facts::RunFacts;
use crate::task_tree_state::{EventIdIndex, LivePromptChanges, NodeFacts, TaskFacts};
use crate::{StoreError, StoreResult};

/// The format version of the checkpoint file. A file of another version is not read.
const CHECKPOINT_VERSION: u64 = 1;
/// The bytes of each end of the covered prefix whose digest the checkpoint keeps. A rewrite that leaves both windows
/// and the length intact would have to reproduce the prefix's ends, which no writer here does.
pub(crate) const WINDOW_BYTES: usize = 4096;

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

/// The facts a session's whole history folded to at one live-log offset.
pub(crate) struct FactCheckpoint {
  /// The archived events the fold covered.
  pub(crate) archived: usize,
  /// The archive files those events came from, in fold order.
  pub(crate) archive_marks: Vec<ArchiveMark>,
  /// The live-log bytes the fold covered. The checkpoint's facts cover every event of the file's first `offset`
  /// bytes, whose last byte is a line end.
  pub(crate) offset: u64,
  head_sha: String,
  tail_sha: String,
  /// The live events the fold covered, past the archived extent.
  pub(crate) covered: usize,
  pub(crate) facts: NodeFacts,
  pub(crate) runs: RunFacts,
  pub(crate) prompt_changes: LivePromptChanges,
}

impl FactCheckpoint {
  /// The checkpoint a scan of `bytes` produced: its facts, and the digests of the prefix they cover.
  pub(crate) fn new(
    archives: usize,
    archive_marks: Vec<ArchiveMark>,
    bytes: &[u8],
    covered: usize,
    facts: NodeFacts,
    runs: RunFacts,
    prompt_changes: LivePromptChanges,
  ) -> Self {
    Self {
      archived: archives,
      archive_marks,
      offset: bytes.len() as u64,
      head_sha: window_sha(bytes, 0),
      tail_sha: window_sha(bytes, bytes.len()),
      covered,
      facts,
      runs,
      prompt_changes,
    }
  }

  /// Whether this checkpoint's facts cover a prefix of `live`: the archived extent and archive files are the ones it
  /// folded, the file still holds its offset bytes, a line ends there, and both prefix windows keep their digests.
  pub(crate) fn covers(&self, live: &[u8], archives: usize, archive_marks: &[ArchiveMark]) -> bool {
    let offset = self.offset as usize;
    self.archived == archives
      && self.archive_marks == archive_marks
      && live.len() >= offset
      && (offset == 0 || live[offset - 1] == b'\n')
      && self.head_sha == window_sha(&live[..offset], 0)
      && self.tail_sha == window_sha(&live[..offset], offset)
  }

  /// The line the checkpoint file holds, or `None` when the bytes are not a checkpoint of this version.
  pub(crate) fn parse(raw: &[u8]) -> Option<Self> {
    let text = std::str::from_utf8(raw).ok()?;
    let value: Value = serde_json::from_str(text).ok()?;
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
    let facts = task_facts_from_value(object.get("facts")?).ok()?;
    Some(Self {
      archived: usize::try_from(object.get("archived")?.as_u64()?).ok()?,
      archive_marks,
      offset: object.get("offset")?.as_u64()?,
      head_sha: object.get("head_sha")?.as_str()?.to_string(),
      tail_sha: object.get("tail_sha")?.as_str()?.to_string(),
      covered: usize::try_from(object.get("covered")?.as_u64()?).ok()?,
      facts: NodeFacts { task: Ok(facts) },
      runs: run_facts_from_value(object.get("runs")?)?,
      prompt_changes: LivePromptChanges {
        subtree: event_from_value(object.get("prompt_subtree")?)?,
        node: event_from_value(object.get("prompt_node")?)?,
      },
    })
  }

  /// The checkpoint's bytes, as the object a later boot reads.
  pub(crate) fn to_bytes(&self) -> String {
    let Ok(task) = self.facts.task() else {
      // A task fold error keeps no checkpoint: the scan that met it scans whole at every boot.
      return String::new();
    };
    let mut object = Map::new();
    object.insert("version".to_string(), json!(CHECKPOINT_VERSION));
    object.insert("archived".to_string(), json!(self.archived));
    object.insert(
      "archives".to_string(),
      Value::Array(
        self
          .archive_marks
          .iter()
          .map(|mark| json!({"name": mark.name, "size": mark.size, "mtime_ns": i64::try_from(mark.mtime_ns).unwrap_or_default()}))
          .collect(),
      ),
    );
    object.insert("offset".to_string(), json!(self.offset));
    object.insert("head_sha".to_string(), Value::String(self.head_sha.clone()));
    object.insert("tail_sha".to_string(), Value::String(self.tail_sha.clone()));
    object.insert("covered".to_string(), json!(self.covered));
    object.insert("facts".to_string(), task_facts_to_value(task));
    object.insert("runs".to_string(), run_facts_to_value(&self.runs));
    object.insert(
      "prompt_subtree".to_string(),
      self.prompt_changes.subtree.as_ref().map_or(Value::Null, |event| event.to_value()),
    );
    object.insert(
      "prompt_node".to_string(),
      self.prompt_changes.node.as_ref().map_or(Value::Null, |event| event.to_value()),
    );
    charliebot_core::text_repr::json_dumps(
      &Value::Object(object),
      &charliebot_core::text_repr::JsonStyle {
        ensure_ascii: false,
        sort_keys: false,
        item_separator: ",",
        key_separator: ":",
        indent: None,
      },
    )
  }
}

/// The digest of the [`WINDOW_BYTES`] window that ends at `end` (or starts at 0 when that is shorter).
fn window_sha(bytes: &[u8], end: usize) -> String {
  let start = end.saturating_sub(WINDOW_BYTES);
  sha256_bytes_hex(&bytes[start..end])
}

/// Write the checkpoint's line to `path`. The file is startup-only: a torn or stale copy fails validation and the
/// next boot scans the log whole, so the write takes no sync.
pub(crate) fn write(path: &Path, checkpoint: &FactCheckpoint) -> StoreResult<()> {
  let text = checkpoint.to_bytes();
  if text.is_empty() {
    return Ok(());
  }
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).map_err(|error| StoreError::new(format!("{}: {error}", parent.display())))?;
  }
  fsutil::atomic_write_text(path, &text).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))
}

fn event_to_value(event: &ChatEvent) -> Value {
  event.to_value()
}

fn event_from_value(value: &Value) -> Option<Option<SharedChatEvent>> {
  match value {
    Value::Null => Some(None),
    Value::Object(_) => Some(Some(Arc::new(ChatEvent::from_value(value)))),
    _ => None,
  }
}

fn events_to_value(events: &[SharedChatEvent]) -> Value {
  Value::Array(events.iter().map(|event| event_to_value(event)).collect())
}

fn events_from_value(value: &Value) -> Result<Vec<SharedChatEvent>, String> {
  value
    .as_array()
    .ok_or_else(|| "events must be an array".to_string())?
    .iter()
    .map(|item| {
      item
        .as_object()
        .map(|_| Arc::new(ChatEvent::from_value(item)))
        .ok_or_else(|| "event is not an object".to_string())
    })
    .collect()
}

fn usize_of(value: &Value) -> Result<usize, String> {
  usize::try_from(value.as_u64().ok_or_else(|| "a count must be a non-negative number".to_string())?)
    .map_err(|_| "count out of range".to_string())
}

fn strings_of(value: &Value) -> Result<Vec<String>, String> {
  value
    .as_array()
    .ok_or_else(|| "a string list must be an array".to_string())?
    .iter()
    .map(|item| item.as_str().map(str::to_string).ok_or_else(|| "a string list holds a non-string".to_string()))
    .collect()
}

pub(crate) fn task_facts_to_value(facts: &TaskFacts) -> Value {
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
    first_close_by_request.insert(request_id.clone(), event_to_value(event));
  }
  let sorted = |set: &HashSet<String>| {
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
    "input_candidates": events_to_value(&facts.input_candidates),
    "imported_pending_ids": sorted(&facts.imported_pending_ids),
    "boundary_index": facts.boundary_index.map_or(Value::Null, |index| json!(index)),
    "boundary_open": facts.boundary_open,
    "close_events": events_to_value(&facts.close_events),
    "close_requests": events_to_value(&facts.close_requests),
    "close_request_ids": sorted(&facts.close_request_ids),
    "first_close_by_request": first_close_by_request,
    "delivered_reports": delivered_reports,
    "events": events_to_value(facts.events_by_id.values().cloned().collect::<Vec<_>>().as_slice()),
    "covered": facts.covered,
  })
}

fn task_facts_from_value(value: &Value) -> Result<TaskFacts, String> {
  let object = value.as_object().ok_or_else(|| "facts must be an object".to_string())?;
  let field = |name: &str| object.get(name).ok_or_else(|| format!("facts lack {name}"));
  let run_outcomes = field("run_outcomes")?
    .as_object()
    .ok_or_else(|| "run_outcomes must be an object".to_string())?
    .iter()
    .map(|(run_id, outcome)| {
      outcome
        .as_str()
        .map(|outcome| (run_id.clone(), outcome.to_string()))
        .ok_or_else(|| "an outcome is not a string".to_string())
    })
    .collect::<Result<HashMap<_, _>, _>>()?;
  let run_finish_positions = field("run_finish_positions")?
    .as_object()
    .ok_or_else(|| "run_finish_positions must be an object".to_string())?
    .iter()
    .map(|(run_id, position)| usize_of(position).map(|position| (run_id.clone(), position)))
    .collect::<Result<HashMap<_, _>, _>>()?;
  let first_close_by_request = field("first_close_by_request")?
    .as_object()
    .ok_or_else(|| "first_close_by_request must be an object".to_string())?
    .iter()
    .map(|(request_id, event)| {
      event
        .as_object()
        .map(|_| (request_id.clone(), Arc::new(ChatEvent::from_value(event))))
        .ok_or_else(|| "a close event is not an object".to_string())
    })
    .collect::<Result<HashMap<_, _>, _>>()?;
  let mut events_by_id = EventIdIndex::default();
  for event in events_from_value(field("events")?)? {
    events_by_id.insert(event);
  }
  let delivered_reports = field("delivered_reports")?
    .as_array()
    .ok_or_else(|| "delivered_reports must be an array".to_string())?
    .iter()
    .map(|pair| {
      let pair = pair.as_array().ok_or_else(|| "a delivered report is not a pair".to_string())?;
      match pair.as_slice() {
        [child, event] => match (child.as_str(), event.as_str()) {
          (Some(child), Some(event)) => Ok((child.to_string(), event.to_string())),
          _ => Err("a delivered report holds a non-string".to_string()),
        },
        _ => Err("a delivered report is not a pair".to_string()),
      }
    })
    .collect::<Result<HashSet<_>, _>>()?;
  Ok(TaskFacts {
    task_state: field("task_state")?.as_str().ok_or_else(|| "task_state must be a string".to_string())?.to_string(),
    has_user_request: field("has_user_request")?
      .as_bool()
      .ok_or_else(|| "has_user_request must be a bool".to_string())?,
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
    boundary_open: field("boundary_open")?.as_bool().ok_or_else(|| "boundary_open must be a bool".to_string())?,
    close_events: events_from_value(field("close_events")?)?,
    close_requests: events_from_value(field("close_requests")?)?,
    close_request_ids: strings_of(field("close_request_ids")?)?.into_iter().collect(),
    first_close_by_request,
    delivered_reports,
    events_by_id,
    covered: usize_of(field("covered")?)?,
  })
}

pub(crate) fn run_facts_to_value(facts: &RunFacts) -> Value {
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
    .collect::<Option<HashMap<_, _>>>()?;
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
        .collect::<Option<HashSet<_>>>()?;
      Some((run_id.clone(), ids))
    })
    .collect::<Option<HashMap<_, _>>>()?;
  Some(RunFacts {
    outcomes,
    stop_requests,
    #[cfg(test)]
    observed_events: 0,
  })
}
