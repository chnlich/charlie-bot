//! The plan registry's read side (`src/features/artifacts/plans.py`): the derived state of a plan, the projection of a
//! registry to the schema it persists and emits, and the tolerant read of a session's `plans.json`.
//!
//! The tolerant read is the one reader of `plans.json` for the plan listing and the sidebar. A corrupt file becomes
//! error entries. Python's read lets a few failures escape as exceptions: a `plans` value that is not iterable, and a
//! number the port cannot hold. Those come back as [`EscapedError`], and each caller fails as Python's does.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::str::Utf8Error;

use anyhow::anyhow;
use charliebot_compat::json_loads;
use charliebot_core::text_repr::{self, JsonStyle};
use serde_json::{Map, Value, json};

use crate::plan_paths;

/// The registry file of a session directory.
pub const PLANS_FILE: &str = "plans.json";

/// The derived state of a plan with no approved version. The sidebar flag matches this spelling.
pub const AWAITING_APPROVAL_STATE: &str = "awaiting approval";
const APPROVED_STATE: &str = "approved";
// A closed plan's state is its close mode's spelling (`constants.py`).
pub(crate) const PLAN_CLOSE_SUPERSEDED: &str = "superseded";
pub(crate) const PLAN_CLOSE_ABANDONED: &str = "abandoned";
pub(crate) const PLAN_CLOSE_COMPLETED: &str = "completed";

/// The plan fields the registry persists, in the order it emits them.
const PLAN_FIELDS: [&str; 5] = ["id", "title", "versions", "takeoff", "closed"];
/// The version fields the registry persists, in the order it emits them.
const VERSION_FIELDS: [&str; 6] = ["v", "file", "created_at", "trigger", "base", "note"];

/// One error entry of the tolerant read: a plan that did not derive, or the file that did not load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError {
  pub session_id: String,
  /// The plan's raw `id`; `null` for a file-level error and for a plan without an id.
  pub plan_id: Value,
  pub error: String,
}

/// The tolerant read's answer: each plan that derived, projected and enriched with its `state`, and each error.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PlansRead {
  pub plans: Vec<Map<String, Value>>,
  pub errors: Vec<ReadError>,
}

impl PlansRead {
  /// The `{"plans": [...], "errors": [...]}` body of the listing route.
  pub fn into_json(self) -> Value {
    let errors: Vec<Value> = self
      .errors
      .into_iter()
      .map(|error| json!({"session_id": error.session_id, "plan_id": error.plan_id, "error": error.error}))
      .collect();
    json!({"plans": self.plans, "errors": errors})
  }
}

/// A Python exception the tolerant read does not catch. It escapes the read and fails the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscapedError {
  /// The Python exception class that raises the failure.
  pub kind: &'static str,
  pub detail: String,
}

impl fmt::Display for EscapedError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}: {}", self.kind, self.detail)
  }
}

/// Why `plans.json` did not load. The read reports a `Caught` failure as the file's error entry.
pub(crate) enum LoadFailure {
  Caught(String),
  Escaped(EscapedError),
}

impl fmt::Display for LoadFailure {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      LoadFailure::Caught(error) => f.write_str(error),
      LoadFailure::Escaped(error) => fmt::Display::fmt(error, f),
    }
  }
}

/// `derive_state_str(plan)`: the display state of a projected plan.
pub fn derive_state_str(plan: &Map<String, Value>) -> Result<&'static str, String> {
  let has_versions = plan.get("versions").and_then(Value::as_array).is_some_and(|versions| !versions.is_empty());
  if !has_versions {
    let id = plan.get("id").unwrap_or(&Value::Null);
    return Err(format!("plan {} has no versions", text_repr::python_str(id)));
  }
  derive_state(plan.get("closed").unwrap_or(&Value::Null), plan.get("takeoff").unwrap_or(&Value::Null))
}

/// `_derive_state(closed, takeoff)`: a closed plan shows its close mode, an unapproved plan awaits approval, and an
/// approved plan shows approved. A value of the wrong type is the error Python raises for it.
fn derive_state(closed: &Value, takeoff: &Value) -> Result<&'static str, String> {
  if !closed.is_null() {
    let Value::Object(closed) = closed else {
      return Err(format!("closed must be a dict or None, got {}", py_type_name(closed)));
    };
    let close_as = closed.get("as").unwrap_or(&Value::Null);
    return match close_as.as_str() {
      Some(PLAN_CLOSE_SUPERSEDED) => Ok(PLAN_CLOSE_SUPERSEDED),
      Some(PLAN_CLOSE_ABANDONED) => Ok(PLAN_CLOSE_ABANDONED),
      Some(PLAN_CLOSE_COMPLETED) => Ok(PLAN_CLOSE_COMPLETED),
      _ => Err(format!("unknown closed.as: {}", py_repr(close_as))),
    };
  }
  if takeoff.is_null() {
    return Ok(AWAITING_APPROVAL_STATE);
  }
  if !takeoff.is_object() {
    return Err(format!("takeoff must be a dict or None, got {}", py_type_name(takeoff)));
  }
  Ok(APPROVED_STATE)
}

/// `require_plan(plans, plan_id)`: the plan with this id in a registry's plans list.
pub fn require_plan(plans: &[Value], plan_id: i64) -> Result<&Value, String> {
  plans
    .iter()
    .find(|plan| plan.get("id").and_then(Value::as_i64) == Some(plan_id))
    .ok_or_else(|| format!("plan {plan_id} not found in session"))
}

/// `_project_version(ver)`: the version's registry fields, `null` where it has none.
fn project_version(version: &Value) -> Result<Value, String> {
  let Value::Object(version) = version else {
    return Err(format!("version must be a dict, got {}", py_type_name(version)));
  };
  Ok(Value::Object(picked(version, &VERSION_FIELDS)))
}

/// `_project_plan(plan)`: the plan's registry fields, with each version projected.
fn project_plan(plan: &Value) -> Result<Map<String, Value>, String> {
  let Value::Object(plan) = plan else {
    return Err(format!("plan must be a dict, got {}", py_type_name(plan)));
  };
  let mut projected = picked(plan, &PLAN_FIELDS);
  let versions = plan.get("versions").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
  let versions = match versions {
    Value::Array(versions) => versions,
    other => return Err(format!("versions must be a list, got {}", py_type_name(&other))),
  };
  let versions = versions.iter().map(project_version).collect::<Result<Vec<_>, _>>()?;
  projected.insert("versions".to_string(), Value::Array(versions));
  Ok(projected)
}

/// `_project_plan` and `derive_state_str` together: the enriched plan the read returns, or the error it reports.
fn project_and_derive(plan: &Value) -> Result<Map<String, Value>, String> {
  let mut projected = project_plan(plan)?;
  let state = derive_state_str(&projected)?;
  projected.insert("state".to_string(), Value::String(state.to_string()));
  Ok(projected)
}

/// `{field: source.get(field)}` for each field, with `null` for a field the source lacks.
fn picked(source: &Map<String, Value>, fields: &[&str]) -> Map<String, Value> {
  fields.iter().map(|field| (field.to_string(), source.get(*field).cloned().unwrap_or(Value::Null))).collect()
}

/// The session's plan list for item links, read strictly: `None` when the session has no `plans.json`, and the plans of
/// the file otherwise. A file that exists but does not read or parse fails, because the status page's link check must
/// refuse a link it cannot resolve.
pub fn read_session_plans(session_dir: &Path) -> anyhow::Result<Option<Vec<Value>>> {
  let path = session_dir.join(PLANS_FILE);
  let text = match fs::read_to_string(&path) {
    Ok(text) => text,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(anyhow!("{} is unreadable: {error}", path.display())),
  };
  let data: Value = serde_json::from_str(&text)
    .map_err(|error| anyhow!("{} holds an unreadable plan registry: {error}", path.display()))?;
  let plans = data.get("plans").ok_or_else(|| anyhow!("{} has no plans key", path.display()))?;
  let plans = plans.as_array().cloned().ok_or_else(|| anyhow!("{} holds plans that are not a list", path.display()))?;
  Ok(Some(plans))
}

/// `read_plans_tolerant(plans_path, session_id)`: the session's plans as the listing and the sidebar read them.
///
/// A missing file reads as no plans. An unreadable file, invalid UTF-8, invalid JSON, or a top level that is not an
/// object is one file-level error entry. A plan that does not project or derive is skipped with its own entry, and the
/// other plans are still returned. An `Err` is a failure Python's read does not catch.
pub fn read_plans_tolerant(plans_path: &Path, session_id: &str) -> Result<PlansRead, EscapedError> {
  let mut read = PlansRead::default();
  if !plan_paths::exists(plans_path).map_err(|error| os_escape(&error, plans_path))? {
    return Ok(read);
  }
  let data = match load_registry(plans_path) {
    Ok(data) => data,
    Err(LoadFailure::Caught(error)) => {
      read.errors.push(file_error(session_id, error));
      return Ok(read);
    }
    Err(LoadFailure::Escaped(error)) => return Err(error),
  };
  let data = match data {
    Value::Object(data) => data,
    other => {
      let error = format!("registry top level is {}, expected dict", py_type_name(&other));
      read.errors.push(file_error(session_id, error));
      return Ok(read);
    }
  };
  let plans = match data.get("plans") {
    Some(plans) => python_iter(plans)?,
    None => Vec::new(),
  };
  for plan in plans {
    match project_and_derive(&plan) {
      Ok(enriched) => read.plans.push(enriched),
      Err(error) => {
        read.errors.push(ReadError { session_id: session_id.to_string(), plan_id: plan_id_of(&plan), error })
      }
    }
  }
  Ok(read)
}

/// `plans_path.read_text(encoding="utf-8")` and `json.loads`: the registry document, or the failure that decides the
/// read's answer.
pub(crate) fn load_registry(plans_path: &Path) -> Result<Value, LoadFailure> {
  let bytes = fs::read(plans_path).map_err(|error| LoadFailure::Caught(os_error_text(&error, plans_path)))?;
  decode_registry(&bytes)
}

/// `read_text(encoding="utf-8")` of the file's bytes, then `json.loads`: the registry document, or the decode failure.
pub(crate) fn decode_registry(bytes: &[u8]) -> Result<Value, LoadFailure> {
  let text = std::str::from_utf8(bytes).map_err(|error| LoadFailure::Caught(utf8_error_text(&error, bytes)))?;
  // Python's text-mode read translates every line ending to "\n", and the JSON error positions count that text.
  let text = text.replace("\r\n", "\n").replace('\r', "\n");
  json_loads::loads_text(&text).map_err(|error| match error {
    json_loads::LoadError::Value(text) => LoadFailure::Caught(text),
    json_loads::LoadError::Unrepresentable(what) => {
      LoadFailure::Escaped(EscapedError { kind: "ValueError", detail: what })
    }
  })
}

/// `_project_registry(data)`: the registry a save writes. Each plan is projected to the persisted fields, and the top
/// level keeps only `plans`, so keys the schema does not know are dropped on save.
pub(crate) fn project_registry(plans: &[Value]) -> Result<Value, String> {
  let projected = plans.iter().map(|plan| project_plan(plan).map(Value::Object)).collect::<Result<Vec<_>, _>>()?;
  Ok(json!({"plans": projected}))
}

/// `json_utils.write_json_atomically(path, registry, indent=2)`: the bytes of a registry file. The fork copy and the
/// plan verbs both write through it, so a registry has one spelling on disk.
pub(crate) fn registry_text(registry: &Value) -> String {
  text_repr::json_dumps(registry, &REGISTRY_JSON)
}

/// `json.dumps(registry, ensure_ascii=False, indent=2)`: the registry's JSON style.
const REGISTRY_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ",", key_separator: ": ", indent: Some(2) };

/// The items `for item in value` yields for a registry list. A dict yields its keys and a string its characters. A
/// number, `null` or a boolean is not iterable, which raises `TypeError` out of the read.
fn python_iter(value: &Value) -> Result<Vec<Value>, EscapedError> {
  match value {
    Value::Array(items) => Ok(items.clone()),
    Value::Object(members) => Ok(members.keys().map(|key| Value::String(key.clone())).collect()),
    Value::String(text) => Ok(text.chars().map(|character| Value::String(character.to_string())).collect()),
    other => {
      Err(EscapedError { kind: "TypeError", detail: format!("'{}' object is not iterable", py_type_name(other)) })
    }
  }
}

/// The `plan_id` of an error entry: the plan's raw `id` when the plan is a dict that has one.
fn plan_id_of(plan: &Value) -> Value {
  match plan {
    Value::Object(plan) => plan.get("id").cloned().unwrap_or(Value::Null),
    _ => Value::Null,
  }
}

fn file_error(session_id: &str, error: String) -> ReadError {
  ReadError { session_id: session_id.to_string(), plan_id: Value::Null, error }
}

/// An `OSError` the read does not catch, as `Path.exists()` raises it.
pub(crate) fn os_escape(error: &io::Error, path: &Path) -> EscapedError {
  let kind = match error.kind() {
    io::ErrorKind::PermissionDenied => "PermissionError",
    _ => "OSError",
  };
  EscapedError { kind, detail: os_error_text(error, path) }
}

/// Python's `str(OSError)` for a failed call on `path`: `[Errno N] strerror: 'path'`.
fn os_error_text(error: &io::Error, path: &Path) -> String {
  let errno = error.raw_os_error().expect("a filesystem call fails with an errno");
  let message = error.to_string();
  let strerror = message.strip_suffix(&format!(" (os error {errno})")).unwrap_or(message.as_str());
  format!("[Errno {errno}] {strerror}: {}", text_repr::str_repr(&path.to_string_lossy()))
}

/// Python's `UnicodeDecodeError` text for the UTF-8 failure at `error` in `bytes`.
fn utf8_error_text(error: &Utf8Error, bytes: &[u8]) -> String {
  let start = error.valid_up_to();
  let (end, reason) = match error.error_len() {
    Some(length) => {
      let lead_byte = matches!(bytes[start], 0xC2..=0xF4);
      let reason = if lead_byte { "invalid continuation byte" } else { "invalid start byte" };
      (start + length, reason)
    }
    None => (bytes.len(), "unexpected end of data"),
  };
  if end - start == 1 {
    format!("'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}", bytes[start])
  } else {
    format!("'utf-8' codec can't decode bytes in position {start}-{}: {reason}", end - 1)
  }
}

/// The Python type name of a JSON value, as `type(x).__name__` gives it for the value `json.loads` produced.
pub(crate) fn py_type_name(value: &Value) -> &'static str {
  match value {
    Value::Null => "NoneType",
    Value::Bool(_) => "bool",
    Value::Number(number) if number.is_f64() => "float",
    Value::Number(_) => "int",
    Value::String(_) => "str",
    Value::Array(_) => "list",
    Value::Object(_) => "dict",
  }
}

/// `repr(value)` of a JSON value that `json.loads` produced: strings quoted, the rest as `str` prints them.
pub(crate) fn py_repr(value: &Value) -> String {
  match value {
    Value::String(text) => text_repr::str_repr(text),
    other => text_repr::python_str(other),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn read_bytes(bytes: &[u8]) -> Result<PlansRead, EscapedError> {
    let dir = tempfile::tempdir().expect("a temporary session directory is available");
    let path = dir.path().join(PLANS_FILE);
    fs::write(&path, bytes).expect("the registry is written");
    read_plans_tolerant(&path, "sess")
  }

  fn file_entry(error: &str) -> ReadError {
    ReadError { session_id: "sess".to_string(), plan_id: Value::Null, error: error.to_string() }
  }

  fn plan_entry(plan_id: Value, error: &str) -> ReadError {
    ReadError { session_id: "sess".to_string(), plan_id, error: error.to_string() }
  }

  #[test]
  fn corrupt_registries_yield_the_error_entries_python_yields() {
    let cases: [(&str, Vec<ReadError>); 9] = [
      (
        "{not valid json",
        vec![file_entry("Expecting property name enclosed in double quotes: line 1 column 2 (char 1)")],
      ),
      ("[1, 2]", vec![file_entry("registry top level is list, expected dict")]),
      (
        r#"{"plans": {"a": 1, "b": 2}}"#,
        vec![file_entry("plan must be a dict, got str"), file_entry("plan must be a dict, got str")],
      ),
      (
        r#"{"plans": [5, "x", null, true, 1.5, [1], {"id": 9}]}"#,
        vec![
          file_entry("plan must be a dict, got int"),
          file_entry("plan must be a dict, got str"),
          file_entry("plan must be a dict, got NoneType"),
          file_entry("plan must be a dict, got bool"),
          file_entry("plan must be a dict, got float"),
          file_entry("plan must be a dict, got list"),
          plan_entry(json!(9), "plan 9 has no versions"),
        ],
      ),
      (r#"{"plans": [{"id": 1, "versions": []}]}"#, vec![plan_entry(json!(1), "plan 1 has no versions")]),
      (r#"{"plans": [{"id": 2, "versions": [5]}]}"#, vec![plan_entry(json!(2), "version must be a dict, got int")]),
      (
        r#"{"plans": [{"id": 3, "versions": [{"v": 1}], "closed": {"as": "bogus"}}]}"#,
        vec![plan_entry(json!(3), "unknown closed.as: 'bogus'")],
      ),
      (
        r#"{"plans": [{"id": 4, "versions": [{"v": 1}], "takeoff": "yes"}]}"#,
        vec![plan_entry(json!(4), "takeoff must be a dict or None, got str")],
      ),
      (
        r#"{"plans": [{"id": "x y", "versions": "x"}]}"#,
        vec![plan_entry(json!("x y"), "versions must be a list, got str")],
      ),
    ];
    for (text, errors) in cases {
      let read = read_bytes(text.as_bytes()).expect("a corrupt registry does not escape the read");
      assert_eq!(read, PlansRead { plans: Vec::new(), errors }, "registry {text}");
    }
  }

  #[test]
  fn a_plan_error_keeps_the_raw_id_and_the_other_plans() {
    let read = read_bytes(
      br#"{"plans": [{"id": {"a": "it's"}, "versions": []}, {"id": 5, "versions": [{"v": 1}], "closed": {"as": "it's\n"}}]}"#,
    )
    .expect("a corrupt registry does not escape the read");
    assert_eq!(
      read.errors,
      vec![
        plan_entry(json!({"a": "it's"}), "plan {'a': \"it's\"} has no versions"),
        plan_entry(json!(5), "unknown closed.as: \"it's\\n\""),
      ]
    );
    assert!(read.plans.is_empty());
  }

  #[test]
  fn a_plans_value_that_is_not_iterable_escapes_the_read_as_python_raises_it() {
    let escaped = read_bytes(br#"{"plans": null}"#).expect_err("Python's read raises TypeError here");
    assert_eq!(escaped, EscapedError { kind: "TypeError", detail: "'NoneType' object is not iterable".to_string() });
  }

  #[test]
  fn undecodable_bytes_name_the_position_as_python_does() {
    let start = read_bytes(b"\xff{}").expect("a decode failure is an error entry");
    assert_eq!(
      start.errors,
      vec![file_entry("'utf-8' codec can't decode byte 0xff in position 0: invalid start byte")]
    );
    let continuation = read_bytes(b"{\"plans\": \"\xe4\x41\"}").expect("a decode failure is an error entry");
    assert_eq!(
      continuation.errors,
      vec![file_entry("'utf-8' codec can't decode byte 0xe4 in position 11: invalid continuation byte")]
    );
    let truncated = read_bytes(b"{\"plans\": \"\xe4\xb8").expect("a decode failure is an error entry");
    assert_eq!(
      truncated.errors,
      vec![file_entry("'utf-8' codec can't decode bytes in position 11-12: unexpected end of data")]
    );
  }

  #[test]
  fn a_missing_registry_reads_as_no_plans() {
    let dir = tempfile::tempdir().expect("a temporary session directory is available");
    let read = read_plans_tolerant(&dir.path().join(PLANS_FILE), "sess").expect("a missing file is not an error");
    assert_eq!(read, PlansRead::default());
  }

  #[test]
  fn plans_are_projected_to_the_registry_schema_with_their_derived_state() {
    let read = read_bytes(
      br#"{"plans": [
        {"id": 1, "title": "P1", "unknown": "dropped", "takeoff": null, "closed": null,
         "versions": [{"v": 1, "file": "artifacts/a.html", "created_at": "t", "trigger": "initial",
                       "base": null, "note": null, "extra": 1}]},
        {"id": 2, "title": "P2", "versions": [{"v": 1, "file": "artifacts/b.html"}],
         "takeoff": {"v": 1, "at": "t"}, "closed": null},
        {"id": 3, "versions": [{"v": 1}], "closed": {"as": "completed", "at": "t"}, "takeoff": null},
        {"id": 4, "versions": [{"v": 1}], "closed": {"as": "superseded"}, "takeoff": {"v": 1}}
      ], "extra_top": 1}"#,
    )
    .expect("the registry reads");
    assert!(read.errors.is_empty());
    let states: Vec<&str> = read.plans.iter().map(|plan| plan["state"].as_str().expect("a state string")).collect();
    assert_eq!(states, ["awaiting approval", "approved", "completed", "superseded"]);
    assert_eq!(
      read.into_json(),
      json!({
        "plans": [
          {
            "id": 1, "title": "P1", "takeoff": null, "closed": null, "state": "awaiting approval",
            "versions": [{"v": 1, "file": "artifacts/a.html", "created_at": "t", "trigger": "initial",
                          "base": null, "note": null}]
          },
          {
            "id": 2, "title": "P2", "takeoff": {"v": 1, "at": "t"}, "closed": null, "state": "approved",
            "versions": [{"v": 1, "file": "artifacts/b.html", "created_at": null, "trigger": null,
                          "base": null, "note": null}]
          },
          {
            "id": 3, "title": null, "takeoff": null, "closed": {"as": "completed", "at": "t"},
            "state": "completed",
            "versions": [{"v": 1, "file": null, "created_at": null, "trigger": null, "base": null, "note": null}]
          },
          {
            "id": 4, "title": null, "takeoff": {"v": 1}, "closed": {"as": "superseded"}, "state": "superseded",
            "versions": [{"v": 1, "file": null, "created_at": null, "trigger": null, "base": null, "note": null}]
          }
        ],
        "errors": []
      })
    );
  }
}
