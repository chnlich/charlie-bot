//! The taste store: an append-only JSONL precedent table at `$CHARLIEBOT_HOME/taste/rows.jsonl`.
//!
//! Two record kinds share the file. A `row` is one choice precedent. A `mark` covers a row with a memory entry or
//! retracts a misread row. The active view is derived on every read: a row is active when no row supersedes it and no
//! mark covers or retracts it. Derived state lives nowhere else, so a correction never rewrites evidence.
//!
//! Every writer appends under an exclusive `flock` on `<taste>/rows.lock`, so a concurrent backfill and daily run
//! assign distinct ids. This module is the only writer of `rows.jsonl`.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use charliebot_core::config::CharlieBotConfig;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The store file's name under the taste directory.
pub const ROWS_FILENAME: &str = "rows.jsonl";
/// The lock file guarding id assignment and appends.
const LOCK_FILENAME: &str = "rows.lock";

/// The index header, verbatim. Each line states one rule the reading agent follows. It names no domain, count or
/// date, so the segment's bytes change only when the set of active domains changes. A changed segment starts a
/// fresh native conversation for every open manager, because the segment enters the hashed prompt blocks.
pub const INDEX_HEADER: &str = "Taste index: the user's choice precedents. Full rows: `charliebot taste query --domain <domain>`.\n\
Before you recommend an option to the user or make a choice yourself in a listed domain, read that domain's rows.\n\
Follow a row whose situation matches, and name its id beside the recommendation or in the digest entry.\n\
When the situation differs, state the difference.\n\
The approval tier of each step stays as the approval policy table sets it.";

/// The reason text of a row whose source states no reason.
pub const UNSTATED_REASON: &str = "未说明";

/// The taste store's directory (`<home>/taste`).
pub fn taste_dir(cfg: &CharlieBotConfig) -> PathBuf {
  cfg.charliebot_home.join("taste")
}

/// One invalid field of a record, naming the field as the record schema spells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
  pub field: String,
  pub detail: String,
}

impl FieldError {
  fn new(field: impl Into<String>, detail: impl Into<String>) -> Self {
    Self { field: field.into(), detail: detail.into() }
  }
}

impl std::fmt::Display for FieldError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(formatter, "{}: {}", self.field, self.detail)
  }
}

impl std::error::Error for FieldError {}

/// What a row records about the user's decision: picking among options, or rejecting a working result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
  Choice,
  Rejection,
}

impl RowKind {
  fn as_str(self) -> &'static str {
    match self {
      Self::Choice => "choice",
      Self::Rejection => "rejection",
    }
  }

  fn parse(value: &str) -> Option<Self> {
    match value {
      "choice" => Some(Self::Choice),
      "rejection" => Some(Self::Rejection),
      _ => None,
    }
  }
}

/// The chat event or plan version a row came from. Exactly one reference kind is present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceRef {
  Event(String),
  PlanVersion(String),
}

/// A row's provenance: the session, its link, and the event or plan version inside it. A row that comes
/// from one comment of a numbered comment batch carries that comment's number in `comment_item`; rows from
/// other sources carry none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
  pub session_id: String,
  pub url: String,
  pub reference: SourceRef,
  pub comment_item: Option<u64>,
}

/// One choice precedent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
  pub id: String,
  pub decided_at: String,
  pub domain: String,
  pub kind: RowKind,
  pub situation: String,
  pub options: Vec<String>,
  pub recommended: Option<String>,
  pub chosen: String,
  pub reason: String,
  pub source: Source,
  pub supersedes: Option<String>,
}

/// How a mark takes a row out of the active view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkStatus {
  /// The row's precedent is now covered by a memory entry.
  Covered(String),
  /// The row read its source wrong and is withdrawn.
  Retracted(String),
}

/// One mark on a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
  pub row: String,
  pub status: MarkStatus,
}

/// One record of the append-only file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
  Row(Box<Row>),
  Mark(Mark),
}

/// The loaded store: every record, in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Store {
  pub records: Vec<Record>,
}

impl Store {
  /// Every row record, in file order.
  pub fn rows(&self) -> impl Iterator<Item = &Row> {
    self.records.iter().filter_map(|record| match record {
      Record::Row(row) => Some(row.as_ref()),
      Record::Mark(_) => None,
    })
  }

  /// The row with `id`, or `None` for an id no row carries.
  pub fn row(&self, id: &str) -> Option<&Row> {
    self.rows().find(|row| row.id == id)
  }

  /// The rows a later row supersedes and the rows a mark covers or retracts.
  fn inactive_ids(&self) -> BTreeSet<&str> {
    let mut inactive = BTreeSet::new();
    for row in self.rows() {
      if let Some(target) = row.supersedes.as_deref() {
        inactive.insert(target);
      }
    }
    for record in &self.records {
      if let Record::Mark(mark) = record {
        inactive.insert(mark.row.as_str());
      }
    }
    inactive
  }

  /// The rows with no superseding row and no mark, in file order.
  pub fn active_rows(&self) -> Vec<&Row> {
    let inactive = self.inactive_ids();
    self.rows().filter(|row| !inactive.contains(row.id.as_str())).collect()
  }

  /// The domains that hold at least one active row, sorted.
  pub fn active_domains(&self) -> BTreeSet<String> {
    self.active_rows().iter().map(|row| row.domain.clone()).collect()
  }

  /// The segment a run's instructions receive. Empty when no domain has an active row.
  pub fn index_text(&self) -> String {
    let domains = self.active_domains();
    if domains.is_empty() {
      return String::new();
    }
    let mut text = String::from(INDEX_HEADER);
    for domain in domains {
      text.push('\n');
      text.push_str(&domain);
    }
    text
  }

  /// The active rows of `domain`, or the known domains when it holds none.
  pub fn query_domain(&self, domain: &str) -> Result<Vec<&Row>, String> {
    let rows: Vec<&Row> = self.active_rows().into_iter().filter(|row| row.domain == domain).collect();
    if !rows.is_empty() {
      return Ok(rows);
    }
    let known: Vec<String> = self.active_domains().into_iter().collect();
    let listing = if known.is_empty() { "none".to_string() } else { known.join(", ") };
    Err(format!("unknown domain: {domain} (known domains: {listing})"))
  }

  /// The rows of `domain` that were active at `moment`: decided earlier, and no superseding row decided by then.
  ///
  /// This is the replay's as-of view. A cover carries no decision time, so a covered row stays in the view. A retracted
  /// row read its source wrong, so no moment makes it a precedent; the view leaves it out. The caller excludes the
  /// held-out row by its own decision time.
  pub fn rows_as_of(&self, domain: &str, moment: OffsetDateTime) -> Vec<&Row> {
    let decided_before =
      |row: &Row| OffsetDateTime::parse(&row.decided_at, &Rfc3339).is_ok_and(|decided| decided < moment);
    let superseded_by_then: BTreeSet<&str> =
      self.rows().filter(|row| decided_before(row)).filter_map(|row| row.supersedes.as_deref()).collect();
    let retracted: BTreeSet<&str> = self
      .records
      .iter()
      .filter_map(|record| match record {
        Record::Mark(Mark { row, status: MarkStatus::Retracted(_) }) => Some(row.as_str()),
        Record::Mark(_) | Record::Row(_) => None,
      })
      .collect();
    self
      .rows()
      .filter(|row| row.domain == domain)
      .filter(|row| decided_before(row))
      .filter(|row| !superseded_by_then.contains(row.id.as_str()))
      .filter(|row| !retracted.contains(row.id.as_str()))
      .collect()
  }
}

/// Load the store at `taste_dir`. A missing store file is an empty store; a malformed line fails with its number.
pub fn load_store(taste_dir: &Path) -> anyhow::Result<Store> {
  let path = taste_dir.join(ROWS_FILENAME);
  let file = match File::open(&path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
    Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
  };
  let mut store = Store::default();
  let mut ids = BTreeSet::new();
  for (index, line) in BufReader::new(file).lines().enumerate() {
    let number = index + 1;
    let line = line.with_context(|| format!("read {}:{number}", path.display()))?;
    if line.trim().is_empty() {
      continue;
    }
    let value: Value =
      serde_json::from_str(&line).with_context(|| format!("{}:{number}: invalid JSON", path.display()))?;
    let record = parse_record(&value).map_err(|error| anyhow::anyhow!("{}:{number}: {error}", path.display()))?;
    if let Record::Row(row) = &record
      && !ids.insert(row.id.clone())
    {
      bail!("{}:{number}: duplicate row id {}", path.display(), row.id);
    }
    store.records.push(record);
  }
  Ok(store)
}

/// Parse one stored record from its JSON value, validating every field.
pub fn parse_record(value: &Value) -> Result<Record, FieldError> {
  let object = value.as_object().ok_or_else(|| FieldError::new("record", "expected a JSON object"))?;
  match object.get("type").and_then(Value::as_str) {
    Some("row") => Ok(Record::Row(Box::new(parse_stored_row(object)?))),
    Some("mark") => Ok(Record::Mark(parse_mark(object)?)),
    Some(other) => Err(FieldError::new("type", format!("expected \"row\" or \"mark\", got {other:?}"))),
    None => Err(FieldError::new("type", "expected \"row\" or \"mark\"")),
  }
}

/// Parse a row as `taste add` receives it. Its id is absent because the CLI assigns one under the lock, and `type`
/// may be absent because the verb takes only rows.
fn parse_input_row(value: &Value) -> Result<Row, FieldError> {
  let object = value.as_object().ok_or_else(|| FieldError::new("record", "expected a JSON object"))?;
  match object.get("type") {
    None => {}
    Some(Value::String(text)) if text == "row" => {}
    Some(Value::String(_)) => return Err(FieldError::new("type", "taste add takes a row, not a mark")),
    Some(other) => return Err(FieldError::new("type", format!("expected \"row\", got {other}"))),
  }
  reject_unknown(object, ROW_FIELDS)?;
  if object.contains_key("id") {
    return Err(FieldError::new("id", "assigned by `charliebot taste add`; omit it"));
  }
  parse_row_body(object)
}

/// Parse a stored row, whose id the CLI assigned.
fn parse_stored_row(object: &Map<String, Value>) -> Result<Row, FieldError> {
  reject_unknown(object, ROW_FIELDS)?;
  let id = required_string(object, "id")?;
  if id.strip_prefix('T').and_then(|number| number.parse::<u64>().ok()).is_none() {
    return Err(FieldError::new("id", format!("expected T<n>, got {id:?}")));
  }
  let mut row = parse_row_body(object)?;
  row.id = id;
  Ok(row)
}

const ROW_FIELDS: &[&str] = &[
  "type",
  "id",
  "decided_at",
  "domain",
  "kind",
  "situation",
  "options",
  "recommended",
  "chosen",
  "reason",
  "source",
  "supersedes",
];

/// Parse a row's content fields. The returned row has no id.
fn parse_row_body(object: &Map<String, Value>) -> Result<Row, FieldError> {
  let decided_at = required_string(object, "decided_at")?;
  OffsetDateTime::parse(&decided_at, &Rfc3339)
    .map_err(|error| FieldError::new("decided_at", format!("not an RFC 3339 timestamp: {error}")))?;
  let domain = required_string(object, "domain")?;
  validate_domain(&domain)?;
  let kind_text = required_string(object, "kind")?;
  let kind = RowKind::parse(&kind_text)
    .ok_or_else(|| FieldError::new("kind", format!("expected \"choice\" or \"rejection\", got {kind_text:?}")))?;
  let situation = required_string(object, "situation")?;
  let options = parse_options(object.get("options"))?;
  let recommended = parse_optional_string(object.get("recommended"), "recommended")?;
  let chosen = required_string(object, "chosen")?;
  let reason = required_string(object, "reason")?;
  let source = parse_source(object.get("source"))?;
  let supersedes = parse_optional_string(object.get("supersedes"), "supersedes")?;
  Ok(Row {
    id: String::new(),
    decided_at,
    domain,
    kind,
    situation,
    options,
    recommended,
    chosen,
    reason,
    source,
    supersedes,
  })
}

fn parse_mark(object: &Map<String, Value>) -> Result<Mark, FieldError> {
  reject_unknown(object, &["type", "row", "status", "entry", "reason"])?;
  let row = required_string(object, "row")?;
  let status = required_string(object, "status")?;
  match status.as_str() {
    "covered" => {
      if object.contains_key("reason") {
        return Err(FieldError::new("reason", "a covered mark carries no reason"));
      }
      Ok(Mark { row, status: MarkStatus::Covered(required_string(object, "entry")?) })
    }
    "retracted" => {
      if object.contains_key("entry") {
        return Err(FieldError::new("entry", "a retracted mark names no entry"));
      }
      Ok(Mark { row, status: MarkStatus::Retracted(required_string(object, "reason")?) })
    }
    other => Err(FieldError::new("status", format!("expected \"covered\" or \"retracted\", got {other:?}"))),
  }
}

fn reject_unknown(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), FieldError> {
  for key in object.keys() {
    if !allowed.contains(&key.as_str()) {
      return Err(FieldError::new(key.clone(), "unknown field"));
    }
  }
  Ok(())
}

fn required_string(object: &Map<String, Value>, field: &str) -> Result<String, FieldError> {
  match object.get(field) {
    Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
    Some(Value::String(_)) => Err(FieldError::new(field, "expected a non-empty string")),
    Some(_) => Err(FieldError::new(field, "expected a string")),
    None => Err(FieldError::new(field, "missing field")),
  }
}

fn parse_optional_string(value: Option<&Value>, field: &str) -> Result<Option<String>, FieldError> {
  match value {
    None | Some(Value::Null) => Ok(None),
    Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
    Some(Value::String(_)) => Err(FieldError::new(field, "expected a non-empty string or null")),
    Some(_) => Err(FieldError::new(field, "expected a string or null")),
  }
}

fn parse_options(value: Option<&Value>) -> Result<Vec<String>, FieldError> {
  let Some(value) = value else { return Ok(Vec::new()) };
  let items = value.as_array().ok_or_else(|| FieldError::new("options", "expected an array of strings"))?;
  items
    .iter()
    .enumerate()
    .map(|(index, item)| match item {
      Value::String(text) if !text.trim().is_empty() => Ok(text.clone()),
      Value::String(_) => Err(FieldError::new(format!("options[{index}]"), "expected a non-empty string")),
      _ => Err(FieldError::new(format!("options[{index}]"), "expected a string")),
    })
    .collect()
}

/// One invalid field inside `source`, named with its parent so a reader sees where it sits.
fn source_error(field: impl std::fmt::Display, detail: impl Into<String>) -> FieldError {
  FieldError::new(format!("source.{field}"), detail)
}

fn parse_source(value: Option<&Value>) -> Result<Source, FieldError> {
  let object = value.and_then(Value::as_object).ok_or_else(|| FieldError::new("source", "expected an object"))?;
  reject_unknown(object, &["session_id", "url", "event_id", "plan_version", "comment_item"])
    .map_err(|error| source_error(&error.field, error.detail))?;
  let session_id = required_string(object, "session_id").map_err(|error| source_error(&error.field, error.detail))?;
  let url = required_string(object, "url").map_err(|error| source_error(&error.field, error.detail))?;
  let event_id = optional_reference(object, "event_id")?;
  let plan_version = optional_reference(object, "plan_version")?;
  let comment_item = optional_comment_item(object)?;
  let reference = match (event_id, plan_version) {
    (Some(event_id), None) => SourceRef::Event(event_id),
    (None, Some(plan_version)) => SourceRef::PlanVersion(plan_version),
    (Some(_), Some(_)) => {
      return Err(source_error("event_id", "exactly one of event_id or plan_version belongs in a source"));
    }
    (None, None) => {
      return Err(source_error("event_id", "a source names event_id or plan_version"));
    }
  };
  Ok(Source { session_id, url, reference, comment_item })
}

fn optional_reference(object: &Map<String, Value>, field: &str) -> Result<Option<String>, FieldError> {
  match object.get(field) {
    None => Ok(None),
    Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
    Some(Value::String(_)) => Err(source_error(field, "expected a non-empty string")),
    Some(_) => Err(source_error(field, "expected a string")),
  }
}

fn optional_comment_item(object: &Map<String, Value>) -> Result<Option<u64>, FieldError> {
  match object.get("comment_item") {
    None | Some(Value::Null) => Ok(None),
    Some(Value::Number(number)) => {
      number.as_u64().map(Some).ok_or_else(|| source_error("comment_item", "expected a non-negative whole number"))
    }
    Some(_) => Err(source_error("comment_item", "expected a number")),
  }
}

fn validate_domain(domain: &str) -> Result<(), FieldError> {
  if domain.chars().any(char::is_whitespace) {
    return Err(FieldError::new("domain", "domain names carry no whitespace"));
  }
  let parts: Vec<&str> = domain.split('/').collect();
  if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
    return Err(FieldError::new("domain", "expected <area>/<topic>, such as work/code-review"));
  }
  Ok(())
}

/// Serialize a row as one JSONL line's value, with fields in the schema's order.
pub fn row_value(row: &Row) -> Value {
  let mut source = Map::new();
  source.insert("session_id".to_string(), json!(row.source.session_id));
  source.insert("url".to_string(), json!(row.source.url));
  match &row.source.reference {
    SourceRef::Event(event_id) => {
      source.insert("event_id".to_string(), json!(event_id));
    }
    SourceRef::PlanVersion(plan_version) => {
      source.insert("plan_version".to_string(), json!(plan_version));
    }
  }
  source.insert("comment_item".to_string(), json!(row.source.comment_item));
  let mut value = Map::new();
  value.insert("type".to_string(), json!("row"));
  value.insert("id".to_string(), json!(row.id));
  value.insert("decided_at".to_string(), json!(row.decided_at));
  value.insert("domain".to_string(), json!(row.domain));
  value.insert("kind".to_string(), json!(row.kind.as_str()));
  value.insert("situation".to_string(), json!(row.situation));
  value.insert("options".to_string(), json!(row.options));
  value.insert("recommended".to_string(), json!(row.recommended));
  value.insert("chosen".to_string(), json!(row.chosen));
  value.insert("reason".to_string(), json!(row.reason));
  value.insert("source".to_string(), Value::Object(source));
  value.insert("supersedes".to_string(), json!(row.supersedes));
  Value::Object(value)
}

/// Serialize a mark as one JSONL line's value.
pub fn mark_value(mark: &Mark) -> Value {
  let mut value = Map::new();
  value.insert("type".to_string(), json!("mark"));
  value.insert("row".to_string(), json!(mark.row));
  match &mark.status {
    MarkStatus::Covered(entry) => {
      value.insert("status".to_string(), json!("covered"));
      value.insert("entry".to_string(), json!(entry));
    }
    MarkStatus::Retracted(reason) => {
      value.insert("status".to_string(), json!("retracted"));
      value.insert("reason".to_string(), json!(reason));
    }
  }
  Value::Object(value)
}

/// Append a row from its JSON value under the store lock, assigning the next `T<n>` id and returning it.
///
/// The call refuses a row whose source reference, comment item and `chosen` repeat an existing row, naming that
/// row, and a row whose `supersedes` names no existing row. Two comment items of one source event are separate
/// decisions, so their rows both land even when they choose the same text. An item number never equals an
/// absent one, so a row that now records its item can land beside an older row of the same event that records
/// none.
pub fn add_row(taste_dir: &Path, value: &Value) -> anyhow::Result<String> {
  let mut row = parse_input_row(value)?;
  ensure_dir(taste_dir)?;
  let _lock = StoreLock::acquire(taste_dir)?;
  let store = load_store(taste_dir)?;
  if let Some(supersedes) = row.supersedes.as_deref()
    && store.row(supersedes).is_none()
  {
    return Err(FieldError::new("supersedes", format!("no row {supersedes} in the store")).into());
  }
  if let Some(existing) = store.rows().find(|existing| duplicates(existing, &row)) {
    let reference = match &row.source.reference {
      SourceRef::Event(event_id) => format!("event {event_id}"),
      SourceRef::PlanVersion(plan_version) => format!("plan version {plan_version}"),
    };
    bail!("duplicate of {}: session {} {reference} already chose {:?}", existing.id, row.source.session_id, row.chosen);
  }
  row.id = next_id(&store);
  append_record(taste_dir, &row_value(&row))?;
  Ok(row.id)
}

/// Append a mark that takes `row_id` out of the active view.
pub fn add_mark(taste_dir: &Path, row_id: &str, status: MarkStatus) -> anyhow::Result<()> {
  ensure_dir(taste_dir)?;
  let _lock = StoreLock::acquire(taste_dir)?;
  let store = load_store(taste_dir)?;
  let row = store.row(row_id).ok_or_else(|| anyhow::anyhow!("no row {row_id} in the store"))?;
  if store.active_rows().iter().all(|active| active.id != row.id) {
    bail!("{row_id} is not active; a mark takes an active row out of the view");
  }
  append_record(taste_dir, &mark_value(&Mark { row: row_id.to_string(), status }))?;
  Ok(())
}

fn duplicates(existing: &Row, row: &Row) -> bool {
  if existing.chosen != row.chosen
    || existing.source.session_id != row.source.session_id
    || existing.source.comment_item != row.source.comment_item
  {
    return false;
  }
  match (&existing.source.reference, &row.source.reference) {
    (SourceRef::Event(left), SourceRef::Event(right)) => left == right,
    (SourceRef::PlanVersion(left), SourceRef::PlanVersion(right)) => left == right,
    _ => false,
  }
}

fn next_id(store: &Store) -> String {
  let highest = store
    .rows()
    .filter_map(|row| row.id.strip_prefix('T').and_then(|number| number.parse::<u64>().ok()))
    .max()
    .unwrap_or(0);
  format!("T{}", highest + 1)
}

fn ensure_dir(taste_dir: &Path) -> anyhow::Result<()> {
  fs::create_dir_all(taste_dir).with_context(|| format!("create {}", taste_dir.display()))
}

fn append_record(taste_dir: &Path, value: &Value) -> anyhow::Result<()> {
  let path = taste_dir.join(ROWS_FILENAME);
  let mut line = serde_json::to_string(value)?;
  line.push('\n');
  let mut file = OpenOptions::new()
    .create(true)
    .append(true)
    .open(&path)
    .with_context(|| format!("open {} for append", path.display()))?;
  file.write_all(line.as_bytes()).with_context(|| format!("append to {}", path.display()))
}

/// The exclusive store lock. Closing the file releases the `flock`.
struct StoreLock {
  _file: File,
}

impl StoreLock {
  fn acquire(taste_dir: &Path) -> anyhow::Result<Self> {
    let path = taste_dir.join(LOCK_FILENAME);
    let file = OpenOptions::new()
      .create(true)
      .read(true)
      .write(true)
      .truncate(false)
      .open(&path)
      .with_context(|| format!("open {}", path.display()))?;
    // SAFETY: `file` owns a valid descriptor for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
      return Err(std::io::Error::last_os_error()).with_context(|| format!("lock {}", path.display()));
    }
    Ok(Self { _file: file })
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;
  use tempfile::TempDir;

  use super::*;

  fn empty_dir() -> TempDir {
    tempfile::tempdir().expect("the test directory is created")
  }

  fn valid_row(domain: &str, event_id: &str, decided_at: &str) -> Value {
    json!({
      "type": "row",
      "decided_at": decided_at,
      "domain": domain,
      "kind": "choice",
      "situation": "a plan fork: change the code or write it as a known limitation",
      "options": ["change the code", "write it as a known limitation"],
      "recommended": "change the code",
      "chosen": "write it as a known limitation",
      "reason": UNSTATED_REASON,
      "source": {"session_id": "sess-1", "url": "/?session=sess-1", "event_id": event_id},
      "supersedes": null
    })
  }

  #[test]
  fn add_validates_every_field_and_names_the_bad_one() {
    let dir = empty_dir();
    let mut missing = valid_row("work/code-review", "evt-1", "2026-10-16T12:00:00+00:00");
    missing.as_object_mut().unwrap().remove("chosen");
    let error = add_row(dir.path(), &missing).expect_err("a row without chosen is refused");
    assert!(error.to_string().contains("chosen"), "{error}");

    let mut two_references = valid_row("work/code-review", "evt-2", "2026-10-16T12:00:00+00:00");
    two_references["source"]["plan_version"] = json!("1/v2");
    let error = add_row(dir.path(), &two_references).expect_err("a source with two references is refused");
    assert!(error.to_string().contains("source.event_id"), "{error}");

    let mut bad_kind = valid_row("work/code-review", "evt-3", "2026-10-16T12:00:00+00:00");
    bad_kind["kind"] = json!("preference");
    let error = add_row(dir.path(), &bad_kind).expect_err("an unknown kind is refused");
    assert!(error.to_string().contains("kind"), "{error}");

    let mut no_type = valid_row("work/code-review", "evt-4", "2026-10-16T12:00:00+00:00");
    no_type.as_object_mut().unwrap().remove("type");
    assert_eq!(add_row(dir.path(), &no_type).expect("a row file may omit type"), "T1");
  }

  #[test]
  fn add_refuses_a_repeated_source_and_chosen_and_names_the_existing_row() {
    let dir = empty_dir();
    let row = valid_row("work/code-review", "evt-1", "2026-10-16T12:00:00+00:00");
    assert_eq!(add_row(dir.path(), &row).unwrap(), "T1");
    let error = add_row(dir.path(), &row).expect_err("the same source and chosen is refused");
    let message = error.to_string();
    assert!(message.contains("duplicate of T1"), "{message}");
  }

  #[test]
  fn two_comment_items_of_one_event_both_land_and_a_rerun_of_one_is_refused() {
    let dir = empty_dir();
    let mut first = valid_row("work/code-review", "evt-1", "2026-10-16T12:00:00+00:00");
    first["source"]["comment_item"] = json!(3);
    let mut second = first.clone();
    second["source"]["comment_item"] = json!(5);
    assert_eq!(add_row(dir.path(), &first).unwrap(), "T1");
    assert_eq!(add_row(dir.path(), &second).unwrap(), "T2", "a different comment item is another decision");
    let error = add_row(dir.path(), &first).expect_err("the same event and item is refused");
    assert!(error.to_string().contains("duplicate of T1"), "{error}");

    let store = load_store(dir.path()).unwrap();
    let items: Vec<Option<u64>> = store.rows().map(|row| row.source.comment_item).collect();
    assert_eq!(items, [Some(3), Some(5)]);
  }

  #[test]
  fn a_row_with_a_comment_item_lands_beside_an_older_row_of_the_same_event_without_one() {
    let dir = empty_dir();
    add_row(dir.path(), &valid_row("work/code-review", "evt-1", "2026-10-16T12:00:00+00:00")).unwrap();
    let mut numbered = valid_row("work/code-review", "evt-1", "2026-10-16T12:00:00+00:00");
    numbered["situation"] = json!("another item of the same batch");
    numbered["source"]["comment_item"] = json!(5);
    assert_eq!(add_row(dir.path(), &numbered).unwrap(), "T2");
  }

  #[test]
  fn a_non_numeric_comment_item_is_refused() {
    let dir = empty_dir();
    let mut row = valid_row("work/code-review", "evt-1", "2026-10-16T12:00:00+00:00");
    row["source"]["comment_item"] = json!("3");
    let error = add_row(dir.path(), &row).expect_err("a comment item is a number");
    assert!(error.to_string().contains("source.comment_item"), "{error}");
  }

  #[test]
  fn a_superseding_row_and_a_mark_take_the_old_row_out_of_the_view() {
    let dir = empty_dir();
    add_row(dir.path(), &valid_row("work/code-review", "evt-1", "2026-10-16T12:00:00+00:00")).unwrap();
    let mut reversal = valid_row("work/code-review", "evt-2", "2026-10-20T12:00:00+00:00");
    reversal["chosen"] = json!("change the code");
    reversal["supersedes"] = json!("T1");
    add_row(dir.path(), &reversal).unwrap();
    add_mark(dir.path(), "T2", MarkStatus::Covered("memory:work/code-review/known-limits".into())).unwrap();

    let store = load_store(dir.path()).unwrap();
    let active: Vec<&str> = store.active_rows().iter().map(|row| row.id.as_str()).collect();
    assert!(active.is_empty(), "{active:?}");
    let error = store.query_domain("work/code-review").expect_err("no active row remains");
    assert!(error.contains("unknown domain"), "{error}");
    assert!(store.index_text().is_empty());
  }

  #[test]
  fn a_mark_requires_an_active_row() {
    let dir = empty_dir();
    add_row(dir.path(), &valid_row("work/code-review", "evt-1", "2026-10-16T12:00:00+00:00")).unwrap();
    add_mark(dir.path(), "T1", MarkStatus::Retracted("misread source".into())).unwrap();
    let error = add_mark(dir.path(), "T1", MarkStatus::Retracted("misread source".into()))
      .expect_err("a second mark finds no active row");
    assert!(error.to_string().contains("not active"), "{error}");
  }

  #[test]
  fn query_of_an_unknown_domain_lists_the_known_domains() {
    let dir = empty_dir();
    add_row(dir.path(), &valid_row("work/code-review", "evt-1", "2026-10-16T12:00:00+00:00")).unwrap();
    let store = load_store(dir.path()).unwrap();
    let error = store.query_domain("life/food").expect_err("an unknown domain fails");
    assert!(error.contains("unknown domain: life/food"), "{error}");
    assert!(error.contains("work/code-review"), "{error}");
  }

  #[test]
  fn index_bytes_depend_only_on_the_set_of_domains() {
    let dir = empty_dir();
    add_row(dir.path(), &valid_row("work/code-review", "evt-1", "2026-10-16T12:00:00+00:00")).unwrap();
    let first = load_store(dir.path()).unwrap().index_text();
    assert!(first.starts_with(INDEX_HEADER));
    assert!(first.ends_with("\nwork/code-review"));
    add_row(dir.path(), &valid_row("work/code-review", "evt-2", "2026-10-17T12:00:00+00:00")).unwrap();
    assert_eq!(load_store(dir.path()).unwrap().index_text(), first);
    add_row(dir.path(), &valid_row("life/food", "evt-3", "2026-10-18T12:00:00+00:00")).unwrap();
    let second = load_store(dir.path()).unwrap().index_text();
    assert_eq!(second, format!("{INDEX_HEADER}\nlife/food\nwork/code-review"));
  }

  #[test]
  fn the_as_of_view_keeps_a_row_that_a_later_row_superseded() {
    let dir = empty_dir();
    add_row(dir.path(), &valid_row("work/code-review", "evt-1", "2026-01-01T12:00:00+00:00")).unwrap();
    let mut reversal = valid_row("work/code-review", "evt-2", "2026-02-01T12:00:00+00:00");
    reversal["chosen"] = json!("change the code");
    reversal["supersedes"] = json!("T1");
    add_row(dir.path(), &reversal).unwrap();
    let store = load_store(dir.path()).unwrap();

    let moment = OffsetDateTime::parse("2026-01-15T12:00:00+00:00", &Rfc3339).unwrap();
    let then: Vec<&str> = store.rows_as_of("work/code-review", moment).iter().map(|row| row.id.as_str()).collect();
    assert_eq!(then, ["T1"], "the superseding row had not happened yet");

    let later = OffsetDateTime::parse("2026-03-01T12:00:00+00:00", &Rfc3339).unwrap();
    let now: Vec<&str> = store.rows_as_of("work/code-review", later).iter().map(|row| row.id.as_str()).collect();
    assert_eq!(now, ["T2"]);

    add_mark(dir.path(), "T2", MarkStatus::Retracted("misread source".into())).unwrap();
    let retracted = load_store(dir.path()).unwrap();
    assert!(retracted.rows_as_of("work/code-review", later).is_empty(), "a retracted row is no precedent");
  }
}
