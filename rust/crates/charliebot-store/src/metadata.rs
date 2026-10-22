//! Session and thread metadata models with Python-compatible key ordering.

use std::fmt;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot};
use charliebot_core::pyfmt::{self, JsonStyle};
use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::{StoreError, StoreResult};

const MODEL_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ",", key_separator: ": ", indent: Some(2) };

/// Datetime stored as UTC with Python's Pydantic `Z` and microsecond format.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UtcDateTime(OffsetDateTime);

impl UtcDateTime {
  /// Return the current UTC time, truncated to Python's microsecond precision.
  pub fn now() -> Self {
    let now = OffsetDateTime::now_utc();
    let nanos = (now.nanosecond() / 1_000) * 1_000;
    Self(now.replace_nanosecond(nanos).expect("a microsecond fits in the current second"))
  }

  /// Parse an ISO 8601 datetime; treat an offset-less value as UTC.
  pub fn parse(value: &str) -> Result<Self, time::error::Parse> {
    let normalized = value.replace(' ', "T");
    let time_part = normalized.split_once('T').map_or("", |(_, time)| time);
    let has_offset = time_part.contains('+') || time_part.contains('-');
    let with_utc = if normalized.ends_with('Z') || has_offset { normalized } else { format!("{normalized}Z") };
    OffsetDateTime::parse(&with_utc, &Rfc3339).map(|datetime| Self(datetime.to_offset(time::UtcOffset::UTC)))
  }

  /// Render the UTC datetime with Python's Pydantic microsecond rule.
  pub fn to_python_string(self) -> String {
    let value = self.0.to_offset(time::UtcOffset::UTC);
    let base = format!(
      "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
      value.year(),
      u8::from(value.month()),
      value.day(),
      value.hour(),
      value.minute(),
      value.second()
    );
    let microseconds = value.microsecond();
    if microseconds == 0 { format!("{base}Z") } else { format!("{base}.{microseconds:06}Z") }
  }

  /// Render the same UTC instant using Python `datetime.isoformat()`'s `+00:00` suffix.
  pub fn to_python_isoformat(self) -> String {
    format!("{}+00:00", self.to_python_string().trim_end_matches('Z'))
  }

  /// Return the underlying UTC instant.
  pub fn into_inner(self) -> OffsetDateTime {
    self.0
  }
}

impl Serialize for UtcDateTime {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    serializer.serialize_str(&self.to_python_string())
  }
}

impl<'de> Deserialize<'de> for UtcDateTime {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let value = String::deserialize(deserializer)?;
    Self::parse(&value).map_err(serde::de::Error::custom)
  }
}

impl fmt::Display for UtcDateTime {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.to_python_string())
  }
}

/// Session lifecycle state (`src/infra/models.py::SessionStatus`).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
  #[default]
  Active,
  Archived,
}

/// Task execution profile.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskProfile {
  Manager,
  Worker,
}

/// Scheduled task result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LastRunStatus {
  Running,
  Success,
  Failed,
  Skipped,
}

/// Task type persisted in `TaskSpec`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TaskType {
  #[serde(rename = "implement")]
  Implement,
  #[serde(rename = "quick-edit")]
  QuickEdit,
  #[serde(rename = "script-run")]
  ScriptRun,
  #[serde(rename = "verify")]
  Verify,
  #[serde(rename = "cold-read")]
  ColdRead,
}

/// Round feedback value persisted in `round_ratings`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundRating {
  ThumbsUp,
  ThumbsDown,
}

/// Round ratings in source order, matching Python dictionary serialization.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoundRatings(Vec<(String, RoundRating)>);

impl RoundRatings {
  /// Set or clear one rating while preserving dictionary insertion order.
  pub fn set(&mut self, round_id: String, rating: Option<RoundRating>) {
    if let Some(index) = self.0.iter().position(|(key, _)| key == &round_id) {
      match rating {
        Some(rating) => self.0[index].1 = rating,
        None => {
          self.0.remove(index);
        }
      }
    } else if let Some(rating) = rating {
      self.0.push((round_id, rating));
    }
  }
}

impl Serialize for RoundRatings {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    let mut map = serializer.serialize_map(Some(self.0.len()))?;
    for (key, value) in &self.0 {
      map.serialize_entry(key, value)?;
    }
    map.end()
  }
}

impl<'de> Deserialize<'de> for RoundRatings {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    struct RoundRatingsVisitor;

    impl<'de> Visitor<'de> for RoundRatingsVisitor {
      type Value = RoundRatings;

      fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a map of round identifiers to ratings")
      }

      fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
      where
        A: MapAccess<'de>,
      {
        let mut ratings = Vec::new();
        while let Some(entry) = access.next_entry()? {
          ratings.push(entry);
        }
        Ok(RoundRatings(ratings))
      }
    }

    deserializer.deserialize_map(RoundRatingsVisitor)
  }
}

/// Cold-read task payload.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ColdReadSpec {
  pub page_path: String,
  pub genre: String,
  pub trigger: String,
}

/// Task body carried by a v2 session.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
  #[serde(default)]
  pub goal: String,
  #[serde(default)]
  pub acceptance: Vec<String>,
  #[serde(default)]
  pub context_refs: Vec<String>,
  #[serde(default)]
  pub repo_path: Option<String>,
  #[serde(default)]
  pub base_branch: Option<String>,
  #[serde(default)]
  pub task_type: Option<TaskType>,
  #[serde(default)]
  pub keep_worktree: bool,
  #[serde(default)]
  pub cold_read: Option<ColdReadSpec>,
}

impl<'de> Deserialize<'de> for TaskSpec {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fields {
      #[serde(default)]
      goal: String,
      #[serde(default)]
      acceptance: Vec<String>,
      #[serde(default)]
      context_refs: Vec<String>,
      #[serde(default)]
      repo_path: Option<String>,
      #[serde(default)]
      base_branch: Option<String>,
      #[serde(default)]
      task_type: Option<TaskType>,
      #[serde(default)]
      keep_worktree: bool,
      #[serde(default)]
      cold_read: Option<ColdReadSpec>,
    }
    let fields = Fields::deserialize(deserializer)?;
    if fields.task_type == Some(TaskType::ColdRead) && fields.cold_read.is_none() {
      return Err(serde::de::Error::custom("a cold-read task requires cold_read"));
    }
    if fields.task_type != Some(TaskType::ColdRead) && fields.cold_read.is_some() {
      let task_type = fields.task_type.map_or_else(|| "None".to_string(), |value| format!("{value:?}"));
      return Err(serde::de::Error::custom(format!("a {task_type} task rejects cold_read")));
    }
    Ok(Self {
      goal: fields.goal,
      acceptance: fields.acceptance,
      context_refs: fields.context_refs,
      repo_path: fields.repo_path,
      base_branch: fields.base_branch,
      task_type: fields.task_type,
      keep_worktree: fields.keep_worktree,
      cold_read: fields.cold_read,
    })
  }
}

/// Provenance pointer to a chat event.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventRef {
  pub session_id: String,
  #[serde(default)]
  pub event_id: Option<String>,
}

/// Thread lifecycle state.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
  #[default]
  Idle,
  Running,
  Completed,
  Failed,
  Cancelled,
}

/// Legacy thread metadata (`src/infra/models.py::ThreadMetadata`).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ThreadMetadata {
  #[serde(default = "fresh_uuid")]
  pub id: String,
  pub session_id: String,
  pub description: String,
  #[serde(default)]
  pub status: ThreadStatus,
  #[serde(default = "UtcDateTime::now")]
  pub created_at: UtcDateTime,
  #[serde(default)]
  pub started_at: Option<UtcDateTime>,
  #[serde(default)]
  pub completed_at: Option<UtcDateTime>,
  #[serde(default)]
  pub pid: Option<u32>,
  #[serde(default)]
  pub pid_start: Option<String>,
  #[serde(default)]
  pub exit_code: Option<i32>,
  #[serde(default)]
  pub branch_name: Option<String>,
  #[serde(default)]
  pub base_branch: Option<String>,
  #[serde(default)]
  pub repo_path: Option<String>,
  #[serde(default)]
  pub worktree_path: Option<String>,
  #[serde(default)]
  pub context: Option<String>,
  #[serde(default)]
  pub backend: Option<String>,
  #[serde(default)]
  pub model: Option<String>,
  #[serde(default)]
  pub keep_worktree: bool,
  #[serde(default)]
  pub tried_backends: Vec<String>,
  #[serde(default)]
  pub task_type: Option<TaskType>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

impl ThreadMetadata {
  /// Render this thread using registered field order and preserve unregistered keys.
  pub fn to_json_string(&self, slots: &[MetadataSlot]) -> String {
    let mut declared = Vec::new();
    push(&mut declared, "id", &self.id);
    push(&mut declared, "session_id", &self.session_id);
    push(&mut declared, "description", &self.description);
    push(&mut declared, "status", &self.status);
    push(&mut declared, "created_at", &self.created_at);
    push(&mut declared, "started_at", &self.started_at);
    push(&mut declared, "completed_at", &self.completed_at);
    push(&mut declared, "pid", &self.pid);
    push(&mut declared, "pid_start", &self.pid_start);
    push(&mut declared, "exit_code", &self.exit_code);
    push(&mut declared, "branch_name", &self.branch_name);
    push(&mut declared, "base_branch", &self.base_branch);
    push(&mut declared, "repo_path", &self.repo_path);
    push(&mut declared, "worktree_path", &self.worktree_path);
    push(&mut declared, "context", &self.context);
    push(&mut declared, "backend", &self.backend);
    push(&mut declared, "model", &self.model);
    push(&mut declared, "keep_worktree", &self.keep_worktree);
    push(&mut declared, "tried_backends", &self.tried_backends);
    push(&mut declared, "task_type", &self.task_type);
    finish_metadata(declared, &self.extra, slots, MetadataFile::Thread)
  }
}

/// Session metadata (`src/infra/models.py::SessionMetadata`).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionMetadata {
  #[serde(default = "fresh_uuid")]
  pub id: String,
  pub name: String,
  #[serde(default)]
  pub status: SessionStatus,
  #[serde(default)]
  pub has_unread: bool,
  #[serde(default)]
  pub has_running_tasks: bool,
  #[serde(default)]
  pub work_state: Option<String>,
  #[serde(default)]
  pub has_pending_trigger: bool,
  #[serde(default)]
  pub pending_trigger_count: i64,
  #[serde(default)]
  pub next_trigger_at: Option<UtcDateTime>,
  #[serde(default)]
  pub has_pending_plan_approval: bool,
  #[serde(default)]
  pub starred: bool,
  #[serde(default)]
  pub thinking_since: Option<UtcDateTime>,
  #[serde(default)]
  pub run_backend: Option<String>,
  #[serde(default = "UtcDateTime::now")]
  pub created_at: UtcDateTime,
  #[serde(default = "UtcDateTime::now")]
  pub updated_at: UtcDateTime,
  #[serde(default)]
  pub cc_session_id: Option<String>,
  #[serde(default)]
  pub cc_session_started_at: Option<UtcDateTime>,
  #[serde(default)]
  pub backend: String,
  #[serde(default)]
  pub schedule_cron: Option<String>,
  #[serde(default)]
  pub schedule_enabled: Option<bool>,
  #[serde(default)]
  pub schedule_next_run: Option<String>,
  #[serde(default)]
  pub schedule_timezone: Option<String>,
  #[serde(default)]
  pub schedule_project: Option<String>,
  #[serde(default)]
  pub schedule_allow_failure: Option<bool>,
  #[serde(default)]
  pub parent_session_id: Option<String>,
  #[serde(default)]
  pub successor_session_id: Option<String>,
  #[serde(default = "schema_version")]
  pub schema_version: u8,
  #[serde(default)]
  pub task_parent_id: Option<String>,
  pub profile: TaskProfile,
  #[serde(default)]
  pub task: Option<TaskSpec>,
  #[serde(default)]
  pub created_by_event: Option<EventRef>,
  #[serde(default)]
  pub origin_ref: Option<EventRef>,
  #[serde(default)]
  pub subtree_prompt_ref: Option<String>,
  #[serde(default)]
  pub node_prompt_ref: Option<String>,
  #[serde(default)]
  pub native_prompt_hash: Option<String>,
  #[serde(default)]
  pub native_backend: Option<String>,
  #[serde(default)]
  pub native_model: Option<String>,
  #[serde(default)]
  pub round_ratings: RoundRatings,
  #[serde(default)]
  pub group: Option<String>,
  #[serde(default)]
  pub archive_offset: i64,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

fn fresh_uuid() -> String {
  uuid::Uuid::new_v4().to_string()
}

fn schema_version() -> u8 {
  2
}

impl SessionMetadata {
  /// Construct metadata with Python model defaults and the required session identity fields.
  pub fn new(id: impl Into<String>, name: impl Into<String>, profile: TaskProfile) -> Self {
    Self {
      id: id.into(),
      name: name.into(),
      status: SessionStatus::Active,
      has_unread: false,
      has_running_tasks: false,
      work_state: None,
      has_pending_trigger: false,
      pending_trigger_count: 0,
      next_trigger_at: None,
      has_pending_plan_approval: false,
      starred: false,
      thinking_since: None,
      run_backend: None,
      created_at: UtcDateTime::now(),
      updated_at: UtcDateTime::now(),
      cc_session_id: None,
      cc_session_started_at: None,
      backend: String::new(),
      schedule_cron: None,
      schedule_enabled: None,
      schedule_next_run: None,
      schedule_timezone: None,
      schedule_project: None,
      schedule_allow_failure: None,
      parent_session_id: None,
      successor_session_id: None,
      schema_version: 2,
      task_parent_id: None,
      profile,
      task: None,
      created_by_event: None,
      origin_ref: None,
      subtree_prompt_ref: None,
      node_prompt_ref: None,
      native_prompt_hash: None,
      native_backend: None,
      native_model: None,
      round_ratings: RoundRatings::default(),
      group: None,
      archive_offset: 0,
      extra: Map::new(),
    }
  }

  /// Serialize the public Python model shape, including defaults for registered session metadata slots.
  pub fn to_response_value(&self, slots: &[MetadataSlot]) -> Value {
    let mut value = serde_json::to_value(self).expect("session metadata serializes to JSON");
    let object = value.as_object_mut().expect("session metadata serializes to an object");
    for slot in slots.iter().filter(|slot| slot.on == MetadataFile::Session) {
      for field in &slot.fields {
        object.entry(field.name.to_string()).or_insert_with(|| field.default.clone());
      }
    }
    value
  }

  /// Parse metadata and enforce the v2 session schema.
  pub fn from_json(raw: &str, path: Option<&str>) -> StoreResult<Self> {
    let value: Value = serde_json::from_str(raw).map_err(|error| {
      StoreError::new(match path {
        Some(path) => format!("{path}: invalid session metadata: {error}"),
        None => format!("invalid session metadata: {error}"),
      })
    })?;
    if let Some(object) = value.as_object()
      && (object.get("profile").is_none_or(Value::is_null)
        || object
          .get("schema_version")
          .is_some_and(|version| version.as_i64() != Some(2) && version.as_f64() != Some(2.0)))
    {
      let location = path.map_or_else(String::new, |path| format!("{path}: "));
      return Err(StoreError::new(format!(
        "{location}session metadata requires a schema_version 2 profile; run scripts/v1_session_conversion.py"
      )));
    }
    serde_json::from_value(value).map_err(|error| {
      StoreError::new(match path {
        Some(path) => format!("{path}: invalid session metadata: {error}"),
        None => format!("invalid session metadata: {error}"),
      })
    })
  }

  /// Render metadata in Python model field order, excluding runtime-only fields used by saves.
  pub fn to_json_string(&self, slots: &[MetadataSlot], exclude_transient: bool) -> String {
    let mut declared = Vec::new();
    push(&mut declared, "id", &self.id);
    push(&mut declared, "name", &self.name);
    push(&mut declared, "status", &self.status);
    push(&mut declared, "has_unread", &self.has_unread);
    if !exclude_transient {
      push(&mut declared, "has_running_tasks", &self.has_running_tasks);
      push(&mut declared, "work_state", &self.work_state);
      push(&mut declared, "has_pending_trigger", &self.has_pending_trigger);
      push(&mut declared, "pending_trigger_count", &self.pending_trigger_count);
      push(&mut declared, "next_trigger_at", &self.next_trigger_at);
      push(&mut declared, "has_pending_plan_approval", &self.has_pending_plan_approval);
    }
    push(&mut declared, "starred", &self.starred);
    if !exclude_transient {
      push(&mut declared, "thinking_since", &self.thinking_since);
      push(&mut declared, "run_backend", &self.run_backend);
    }
    push(&mut declared, "created_at", &self.created_at);
    push(&mut declared, "updated_at", &self.updated_at);
    push(&mut declared, "cc_session_id", &self.cc_session_id);
    push(&mut declared, "cc_session_started_at", &self.cc_session_started_at);
    push(&mut declared, "backend", &self.backend);
    if !exclude_transient {
      push(&mut declared, "schedule_cron", &self.schedule_cron);
      push(&mut declared, "schedule_enabled", &self.schedule_enabled);
      push(&mut declared, "schedule_next_run", &self.schedule_next_run);
      push(&mut declared, "schedule_timezone", &self.schedule_timezone);
      push(&mut declared, "schedule_project", &self.schedule_project);
      push(&mut declared, "schedule_allow_failure", &self.schedule_allow_failure);
    }
    push(&mut declared, "parent_session_id", &self.parent_session_id);
    push(&mut declared, "successor_session_id", &self.successor_session_id);
    push(&mut declared, "schema_version", &self.schema_version);
    push(&mut declared, "task_parent_id", &self.task_parent_id);
    push(&mut declared, "profile", &self.profile);
    push(&mut declared, "task", &self.task);
    push(&mut declared, "created_by_event", &self.created_by_event);
    push(&mut declared, "origin_ref", &self.origin_ref);
    push(&mut declared, "subtree_prompt_ref", &self.subtree_prompt_ref);
    push(&mut declared, "node_prompt_ref", &self.node_prompt_ref);
    push(&mut declared, "native_prompt_hash", &self.native_prompt_hash);
    push(&mut declared, "native_backend", &self.native_backend);
    push(&mut declared, "native_model", &self.native_model);
    push(&mut declared, "round_ratings", &self.round_ratings);
    push(&mut declared, "group", &self.group);
    push(&mut declared, "archive_offset", &self.archive_offset);
    finish_metadata(declared, &self.extra, slots, MetadataFile::Session)
  }

  /// Set an unregistered or registered top-level metadata key.
  pub fn set_extra(&mut self, name: impl Into<String>, value: Value) {
    self.extra.insert(name.into(), value);
  }

  /// Read an extra top-level key.
  pub fn extra(&self, name: &str) -> Option<&Value> {
    self.extra.get(name)
  }
}

fn push<T: Serialize>(fields: &mut Vec<(&'static str, Value)>, name: &'static str, value: &T) {
  fields.push((name, serde_json::to_value(value).expect("metadata fields serialize to JSON")));
}

fn finish_metadata(
  fields: Vec<(&'static str, Value)>,
  extras: &Map<String, Value>,
  slots: &[MetadataSlot],
  on: MetadataFile,
) -> String {
  let mut output = Map::new();
  let mut placed_slots = std::collections::HashSet::new();
  for (name, value) in fields {
    output.insert(name.to_string(), value);
    for (index, slot) in slots.iter().enumerate().filter(|(_, slot)| slot.on == on && slot.after == Some(name)) {
      place_slot(&mut output, extras, slot);
      placed_slots.insert(index);
    }
  }
  for (index, slot) in slots.iter().enumerate().filter(|(_, slot)| slot.on == on) {
    if slot.after.is_none() || !placed_slots.contains(&index) {
      place_slot(&mut output, extras, slot);
      placed_slots.insert(index);
    }
  }
  let owned: std::collections::HashSet<&str> =
    slots.iter().filter(|slot| slot.on == on).flat_map(|slot| slot.fields.iter().map(|field| field.name)).collect();
  for (name, value) in extras {
    if !owned.contains(name.as_str()) {
      output.insert(name.clone(), value.clone());
    }
  }
  pyfmt::model_json_dumps(&Value::Object(output), &MODEL_JSON)
}

fn place_slot(output: &mut Map<String, Value>, extras: &Map<String, Value>, slot: &MetadataSlot) {
  for field in &slot.fields {
    output.insert(field.name.to_string(), extras.get(field.name).cloned().unwrap_or_else(|| field.default.clone()));
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_core::metadata_slot::SlotField;
  use serde_json::json;

  fn sample_slots() -> Vec<MetadataSlot> {
    vec![MetadataSlot {
      owner: "probe",
      on: MetadataFile::Session,
      after: Some("group"),
      fields: vec![
        SlotField { name: "probe_note", default: Value::Null },
        SlotField { name: "probe_count", default: json!(0) },
      ],
    }]
  }

  #[test]
  fn round_ratings_preserve_order_when_set_and_readded() {
    let mut ratings = RoundRatings(vec![
      ("round-a".to_string(), RoundRating::ThumbsUp),
      ("round-b".to_string(), RoundRating::ThumbsDown),
    ]);

    ratings.set("round-a".to_string(), Some(RoundRating::ThumbsDown));
    ratings.set("round-b".to_string(), None);
    ratings.set("round-b".to_string(), Some(RoundRating::ThumbsUp));

    assert_eq!(
      ratings.0,
      vec![("round-a".to_string(), RoundRating::ThumbsDown), ("round-b".to_string(), RoundRating::ThumbsUp),]
    );
  }

  #[test]
  fn slots_keep_their_position_and_unknown_values_stay_last() {
    let mut session = SessionMetadata::new("synthetic-id", "session", TaskProfile::Manager);
    session.set_extra("probe_note", json!("hello"));
    session.set_extra("left_by_deleted_package", json!({"nested": [1, 2]}));
    let value: Value = serde_json::from_str(&session.to_json_string(&sample_slots(), true)).unwrap();
    let keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
    let group = keys.iter().position(|key| *key == "group").unwrap();
    assert_eq!(&keys[group + 1..group + 3], &["probe_note", "probe_count"]);
    assert_eq!(keys.last(), Some(&"left_by_deleted_package"));
    assert_eq!(value["probe_count"], 0);
  }

  #[test]
  fn response_value_supplies_python_session_model_and_slot_defaults_for_legacy_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("metadata.json");
    std::fs::write(&path, r#"{"name":"Synthetic","profile":"manager"}"#).unwrap();
    let metadata = SessionMetadata::from_json(&std::fs::read_to_string(path).unwrap(), None).unwrap();
    let slots = vec![MetadataSlot {
      owner: "synthetic",
      on: MetadataFile::Session,
      after: None,
      fields: vec![
        SlotField { name: "route", default: Value::Null },
        SlotField { name: "backend_from_pool", default: json!(false) },
        SlotField { name: "web_status", default: Value::Null },
        SlotField { name: "discord_origin", default: Value::Null },
        SlotField { name: "discord_watermark_id", default: Value::Null },
        SlotField { name: "claude_account", default: Value::Null },
        SlotField { name: "slack_watermark_ts", default: Value::Null },
        SlotField { name: "slack_origin", default: Value::Null },
      ],
    }];

    let value = metadata.to_response_value(&slots);
    let object = value.as_object().unwrap();
    let expected_fields = [
      "id",
      "name",
      "status",
      "has_unread",
      "has_running_tasks",
      "work_state",
      "has_pending_trigger",
      "pending_trigger_count",
      "next_trigger_at",
      "has_pending_plan_approval",
      "starred",
      "thinking_since",
      "run_backend",
      "created_at",
      "updated_at",
      "cc_session_id",
      "cc_session_started_at",
      "backend",
      "schedule_cron",
      "schedule_enabled",
      "schedule_next_run",
      "schedule_timezone",
      "schedule_project",
      "schedule_allow_failure",
      "parent_session_id",
      "successor_session_id",
      "schema_version",
      "task_parent_id",
      "profile",
      "task",
      "created_by_event",
      "origin_ref",
      "subtree_prompt_ref",
      "node_prompt_ref",
      "native_prompt_hash",
      "native_backend",
      "native_model",
      "round_ratings",
      "group",
      "archive_offset",
      "route",
      "backend_from_pool",
      "web_status",
      "discord_origin",
      "discord_watermark_id",
      "claude_account",
      "slack_watermark_ts",
      "slack_origin",
    ];
    let actual_fields: std::collections::BTreeSet<&str> = object.keys().map(String::as_str).collect();
    let expected_fields: std::collections::BTreeSet<&str> = expected_fields.into_iter().collect();
    assert_eq!(actual_fields, expected_fields);

    let expected_defaults = [
      ("name", json!("Synthetic")),
      ("status", json!("active")),
      ("has_unread", json!(false)),
      ("has_running_tasks", json!(false)),
      ("work_state", Value::Null),
      ("has_pending_trigger", json!(false)),
      ("pending_trigger_count", json!(0)),
      ("next_trigger_at", Value::Null),
      ("has_pending_plan_approval", json!(false)),
      ("starred", json!(false)),
      ("thinking_since", Value::Null),
      ("run_backend", Value::Null),
      ("cc_session_id", Value::Null),
      ("cc_session_started_at", Value::Null),
      ("backend", json!("")),
      ("schedule_cron", Value::Null),
      ("schedule_enabled", Value::Null),
      ("schedule_next_run", Value::Null),
      ("schedule_timezone", Value::Null),
      ("schedule_project", Value::Null),
      ("schedule_allow_failure", Value::Null),
      ("parent_session_id", Value::Null),
      ("successor_session_id", Value::Null),
      ("schema_version", json!(2)),
      ("task_parent_id", Value::Null),
      ("profile", json!("manager")),
      ("task", Value::Null),
      ("created_by_event", Value::Null),
      ("origin_ref", Value::Null),
      ("subtree_prompt_ref", Value::Null),
      ("node_prompt_ref", Value::Null),
      ("native_prompt_hash", Value::Null),
      ("native_backend", Value::Null),
      ("native_model", Value::Null),
      ("round_ratings", json!({})),
      ("group", Value::Null),
      ("archive_offset", json!(0)),
      ("route", Value::Null),
      ("backend_from_pool", json!(false)),
      ("web_status", Value::Null),
      ("discord_origin", Value::Null),
      ("discord_watermark_id", Value::Null),
      ("claude_account", Value::Null),
      ("slack_watermark_ts", Value::Null),
      ("slack_origin", Value::Null),
    ];
    for (field, expected) in &expected_defaults {
      assert_eq!(&value[*field], expected, "Python SessionMetadata default for {field}");
    }
    assert!(uuid::Uuid::parse_str(value["id"].as_str().unwrap()).is_ok());
    UtcDateTime::parse(value["created_at"].as_str().unwrap()).unwrap();
    UtcDateTime::parse(value["updated_at"].as_str().unwrap()).unwrap();
  }

  #[test]
  fn aware_datetimes_render_with_six_digits_and_utc_suffix() {
    let value = UtcDateTime::parse("2026-01-01T00:00:00.123400+00:00").unwrap();
    assert_eq!(value.to_string(), "2026-01-01T00:00:00.123400Z");
    assert_eq!(UtcDateTime::parse("2026-01-01T00:00:00").unwrap().to_string(), "2026-01-01T00:00:00Z");
  }

  #[test]
  fn floats_render_as_pydantic_model_json() {
    // Pinned from pydantic's model_dump_json, the Python baseline's metadata writer.
    let cases = [
      ("1e16", "1e16"),
      ("1.5e-7", "1.5e-7"),
      ("1e22", "1e22"),
      ("1e308", "1e308"),
      ("1e-5", "0.00001"),
      ("0.0001", "0.0001"),
      ("1234567890123456.0", "1234567890123456.0"),
      ("-0.0", "-0.0"),
    ];
    for (input, expected) in cases {
      let mut session = SessionMetadata::new("synthetic-id", "session", TaskProfile::Manager);
      session.set_extra("probe", serde_json::from_str::<Value>(input).unwrap());
      let text = session.to_json_string(&[], true);
      assert!(text.contains(&format!("\"probe\": {expected}\n")), "{input} rendered as {text}");
    }
  }

  #[test]
  fn task_spec_checks_cold_read_payload_consistency() {
    assert!(serde_json::from_str::<TaskSpec>(r#"{"task_type":"cold-read"}"#).is_err());
    assert!(
      serde_json::from_str::<TaskSpec>(
        r#"{"task_type":"verify","cold_read":{"page_path":"x","genre":"y","trigger":"z"}}"#
      )
      .is_err()
    );
    assert!(
      serde_json::from_str::<TaskSpec>(
        r#"{"task_type":"cold-read","cold_read":{"page_path":"x","genre":"y","trigger":"z"}}"#
      )
      .is_ok()
    );
  }

  #[test]
  fn v1_metadata_refusal_names_the_conversion_script() {
    let error = SessionMetadata::from_json(r#"{"id":"legacy","name":"legacy"}"#, None).unwrap_err();
    assert!(
      error
        .to_string()
        .ends_with("session metadata requires a schema_version 2 profile; run scripts/v1_session_conversion.py")
    );
  }
}
