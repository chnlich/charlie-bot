//! The plan verbs `present`, `amend`, `approve` and `close` (`src/features/artifacts/plans.py`, `PlanRegistryManager`).
//! Each verb changes one session's `plans.json` under the session's lock: it loads the registry, changes the plans, saves
//! the registry and marks the sidebar dirty, then broadcasts `plan_updated` after the lock is released. `present` and
//! `amend` also check the page with the plan assertion set before they bind it. The reads and the projection belong to
//! `plans`; the check belongs to `artifact_check`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use charliebot_core::{RepoRoot, fsutil, pyfmt, pystr, timefmt};
use charliebot_registry::FeatureContext;
use charliebot_store::sidebar_state;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::artifact_check;
use crate::plan_paths;
use crate::plans::{self, LoadFailure, PLANS_FILE};

/// How a verb fails, as its route answers it.
#[derive(Debug)]
pub(crate) enum VerbError {
  /// A `ValueError` of the verb: `deps.bad_request` answers 400 with the message as `detail`.
  Refused(String),
  /// An exception that is not a `ValueError`: the unhandled-exception 500. The kind names the Python class.
  Failed { kind: &'static str, detail: String },
}

/// The per-session locks of the verbs (`PlanRegistryManager._locks`). A session's lock is made by its first verb and kept.
#[derive(Default)]
pub(crate) struct SessionLocks {
  locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl SessionLocks {
  async fn lock(&self, session_id: &str) -> OwnedMutexGuard<()> {
    let lock = {
      let mut locks = self.locks.lock().expect("the session lock map is not poisoned");
      Arc::clone(locks.entry(session_id.to_string()).or_default())
    };
    lock.lock_owned().await
  }
}

/// The version fields of an `amend` request: the plan it extends, its trigger, its note and its base.
pub(crate) struct Amendment {
  /// The plan to extend, or `None` for the one open lineage.
  pub(crate) plan_id: Option<Value>,
  pub(crate) trigger: String,
  pub(crate) note: String,
  /// The base the version records, or `null`.
  pub(crate) base: Value,
}

/// `present(session_id, file, title, base)`: registers `file` as version 1 of a new plan lineage. Answers `{plan, v, state}`.
pub(crate) async fn present(
  features: &FeatureContext,
  locks: &SessionLocks,
  session_id: &str,
  file: &str,
  title: String,
  base: Value,
) -> Result<Value, VerbError> {
  let path = registry_path(features, session_id);
  let (plan_id, state) = {
    let _guard = locks.lock(session_id).await;
    let mut registry = read_registry(path.clone()).await?;
    let file_relative = validate_new_version_file(features, session_id, file, &registry).await?;
    let plans = plans_of_mut(&mut registry)?;
    let plan_id = next_plan_id(plans)?;
    let version = json!({
      "v": 1,
      "file": file_relative,
      "created_at": timefmt::utc_now_iso(),
      "trigger": "initial",
      "base": base,
      "note": null,
    });
    let plan = json!({"id": plan_id, "title": title, "versions": [version], "takeoff": null, "closed": null});
    let state = plans::derive_state_str(plan_map(&plan)?).map_err(VerbError::Refused)?;
    plans.push(plan);
    write_registry(path, session_id.to_string(), registry).await?;
    (plan_id, state)
  };
  broadcast(features, session_id, &plan_id).await?;
  Ok(json!({"plan": plan_id, "v": 1, "state": state}))
}

/// `amend(session_id, file, note, plan_id, trigger, base)`: appends `file` as the next version of one open lineage.
/// Answers `{plan, v, state}`. The verb refuses a blank note first, before it reads the registry.
pub(crate) async fn amend(
  features: &FeatureContext,
  locks: &SessionLocks,
  session_id: &str,
  file: &str,
  amendment: Amendment,
) -> Result<Value, VerbError> {
  if amendment.note.trim_matches(pystr::is_python_whitespace).is_empty() {
    return Err(VerbError::Refused(
      "amend requires a non-empty --note stating why this version differs from its predecessor".to_string(),
    ));
  }
  let path = registry_path(features, session_id);
  let (plan_id, v, state) = {
    let _guard = locks.lock(session_id).await;
    let mut registry = read_registry(path.clone()).await?;
    let file_relative = validate_new_version_file(features, session_id, file, &registry).await?;
    let plans = plans_of_mut(&mut registry)?;
    let index = resolve_target_plan_for_amend(plans, amendment.plan_id.as_ref())?;
    let plan = plan_map_mut(&mut plans[index])?;
    let v = next_version_number(versions_of(plan)?)?;
    let version = json!({
      "v": v,
      "file": file_relative,
      "created_at": timefmt::utc_now_iso(),
      "trigger": amendment.trigger,
      "base": amendment.base,
      "note": amendment.note,
    });
    let Some(Value::Array(versions)) = plan.get_mut("versions") else {
      unreachable!("the versions list was read above");
    };
    versions.push(version);
    plan.insert("takeoff".to_string(), Value::Null);
    let plan_id = id_of(plan)?.clone();
    let state = plans::derive_state_str(plan).map_err(VerbError::Refused)?;
    write_registry(path, session_id.to_string(), registry).await?;
    (plan_id, v, state)
  };
  broadcast(features, session_id, &plan_id).await?;
  Ok(json!({"plan": plan_id, "v": v, "state": state}))
}

/// `approve(session_id, plan_id)`: records the approval against the latest version of one plan. `plan_id` names the
/// plan, or `None` names the one open plan. Answers `{plan, v, state}`.
pub(crate) async fn approve(
  features: &FeatureContext,
  locks: &SessionLocks,
  session_id: &str,
  plan_id: Option<Value>,
) -> Result<Value, VerbError> {
  let plan = mutate(features, locks, session_id, move |plans| approve_plan(plans, plan_id.as_ref())).await?;
  let plan_id = id_of(&plan)?.clone();
  broadcast(features, session_id, &plan_id).await?;
  let v = plan["takeoff"]["v"].clone();
  let state = plans::derive_state_str(&plan).map_err(VerbError::Refused)?;
  Ok(json!({"plan": plan_id, "v": v, "state": state}))
}

/// `close(session_id, plan_id, close_as)`: terminates one plan as `close_as`. Answers `{plan, state}`.
pub(crate) async fn close(
  features: &FeatureContext,
  locks: &SessionLocks,
  session_id: &str,
  plan_id: Value,
  close_as: String,
) -> Result<Value, VerbError> {
  let changed = plan_id.clone();
  let plan = mutate(features, locks, session_id, move |plans| close_plan(plans, &changed, &close_as)).await?;
  broadcast(features, session_id, &plan_id).await?;
  let state = plans::derive_state_str(&plan).map_err(VerbError::Refused)?;
  Ok(json!({"plan": plan_id, "state": state}))
}

/// Loads the session's registry under its lock, applies `change` to its plans list, and saves the registry. The verb's
/// plan, as `change` returns it, comes back once the lock is released. The file work runs on the blocking pool.
async fn mutate<F>(
  features: &FeatureContext,
  locks: &SessionLocks,
  session_id: &str,
  change: F,
) -> Result<Map<String, Value>, VerbError>
where
  F: FnOnce(&mut Vec<Value>) -> Result<Map<String, Value>, VerbError> + Send + 'static,
{
  let path = registry_path(features, session_id);
  let _guard = locks.lock(session_id).await;
  let session_id = session_id.to_string();
  blocking(move || {
    let mut registry = load(&path)?;
    let plan = change(plans_of_mut(&mut registry)?)?;
    save(&path, &session_id, &registry)?;
    Ok(plan)
  })
  .await
}

/// Runs a file step on the blocking pool. A step that the pool cannot finish is a `RuntimeError` of the verb.
async fn blocking<T, F>(step: F) -> Result<T, VerbError>
where
  F: FnOnce() -> Result<T, VerbError> + Send + 'static,
  T: Send + 'static,
{
  tokio::task::spawn_blocking(step)
    .await
    .map_err(|error| VerbError::Failed { kind: "RuntimeError", detail: error.to_string() })?
}

fn registry_path(features: &FeatureContext, session_id: &str) -> PathBuf {
  features.store.session_dir(session_id).join(PLANS_FILE)
}

/// `_load` on the blocking pool.
async fn read_registry(path: PathBuf) -> Result<Value, VerbError> {
  blocking(move || load(&path)).await
}

/// `_save` on the blocking pool, with the lock still held by the caller.
async fn write_registry(path: PathBuf, session_id: String, registry: Value) -> Result<(), VerbError> {
  blocking(move || save(&path, &session_id, &registry)).await
}

/// `_load`: the raw registry of a session, unknown keys included. An absent file reads as no plans. Bytes that do not
/// decode are the `ValueError` the verb answers 400 with; a file that cannot be read escapes as the 500 Python raises.
fn load(path: &Path) -> Result<Value, VerbError> {
  if !plan_paths::exists(path).map_err(|error| escaped(plans::os_escape(&error, path)))? {
    return Ok(json!({"plans": []}));
  }
  let bytes = fs::read(path).map_err(|error| escaped(plans::os_escape(&error, path)))?;
  plans::decode_registry(&bytes).map_err(|failure| match failure {
    LoadFailure::Caught(text) => VerbError::Refused(text),
    LoadFailure::Escaped(error) => escaped(error),
  })
}

/// `_save`: writes the projected registry, then marks the session's sidebar row dirty. The write creates no directory,
/// so a session directory that is gone fails the save.
fn save(path: &Path, session_id: &str, registry: &Value) -> Result<(), VerbError> {
  let projected = plans::project_registry(plans_of(registry)?).map_err(VerbError::Refused)?;
  fsutil::atomic_write_text(path, &plans::registry_text(&projected))
    .map_err(|error| escaped(plans::os_escape(&error, path)))?;
  sidebar_state::mark_sidebar_dirty(session_id);
  Ok(())
}

/// `_broadcast`: the `plan_updated` frame on the session's channel.
async fn broadcast(features: &FeatureContext, session_id: &str, plan_id: &Value) -> Result<(), VerbError> {
  let frame = json!({"type": "plan_updated", "session_id": session_id, "plan_id": plan_id});
  features
    .store
    .broadcast_only(session_id, frame)
    .await
    .map_err(|error| VerbError::Failed { kind: "RuntimeError", detail: error.to_string() })
}

/// `_validate_new_version_file(session_id, file, data)`: the session-relative path of a file that may back a new version.
/// The file must lie in the session directory, pass the plan assertion set, and back no version yet. The caller holds the
/// session lock and passes the registry it loaded.
async fn validate_new_version_file(
  features: &FeatureContext,
  session_id: &str,
  file: &str,
  registry: &Value,
) -> Result<String, VerbError> {
  let file = plan_paths::posix_normpath(file);
  let file_relative = validate_file_in_session_dir(features, session_id, &file)?;
  let artifact = features.store.session_dir(session_id).join(&file_relative);
  let config =
    features.config.get().map_err(|error| VerbError::Failed { kind: "RuntimeError", detail: error.to_string() })?;
  let repo_root = features
    .startup
    .require::<RepoRoot>()
    .map_err(|error| VerbError::Failed { kind: "RuntimeError", detail: error.to_string() })?;
  let outcomes = artifact_check::run_assertions("plan", artifact, &config, repo_root.path().to_path_buf())
    .await
    .map_err(|error| VerbError::Failed { kind: "RuntimeError", detail: format!("{error:#}") })?;
  let failures: Vec<String> = outcomes
    .iter()
    .filter(|outcome| !outcome.passed)
    .map(|outcome| format!("{}: {}", outcome.name, outcome.detail))
    .collect();
  if !failures.is_empty() {
    return Err(VerbError::Refused(format!(
      "plan artifact fails {} check assertion(s): {}. Measure locally with: charliebot artifact check <artifact.html> --genre plan --assertions-only",
      failures.len(),
      failures.join("; ")
    )));
  }
  if let Some((plan_id, v)) = find_binding_by_file(plans_of(registry)?, &file_relative)? {
    return Err(VerbError::Refused(format!(
      "file {} already bound to plan {} v{}",
      pyfmt::str_repr(&file),
      pyfmt::python_str(&plan_id),
      pyfmt::python_str(&v)
    )));
  }
  Ok(file_relative)
}

/// `_validate_file_in_session_dir(session_id, file)`: the session-relative path of a file that lies in the session
/// directory and exists. The caller passes the normalized spelling, which the messages name.
fn validate_file_in_session_dir(features: &FeatureContext, session_id: &str, file: &str) -> Result<String, VerbError> {
  // `Path.resolve()` raises this ValueError for a NUL byte, before any containment check.
  if file.contains('\0') {
    return Err(VerbError::Refused("lstat: embedded null character in path".to_string()));
  }
  let session_dir = features.store.session_dir(session_id);
  let (candidate, relative) = plan_paths::resolve_plan_file(&session_dir, file);
  let Some(relative) = relative else {
    return Err(VerbError::Refused(format!("file {} resolves outside the session directory", pyfmt::str_repr(file))));
  };
  if !plan_paths::exists(&candidate).map_err(|error| escaped(plans::os_escape(&error, &candidate)))? {
    return Err(VerbError::Refused(format!("file {} not found inside the session directory", pyfmt::str_repr(file))));
  }
  Ok(relative.to_string_lossy().into_owned())
}

/// `_find_binding_by_file(data, file)`: the plan id and version number of the first version whose file, normalized, is
/// `file`. Both sides are normalized, so a registry row spelled `./artifacts/c.html` matches `artifacts/c.html`.
fn find_binding_by_file(plans: &[Value], file: &str) -> Result<Option<(Value, Value)>, VerbError> {
  for plan in plans {
    let plan_fields = plan_map(plan)?;
    for version in versions_of(plan_fields)? {
      let bound = plan_map(version)?.get("file").ok_or_else(|| key_error("file"))?;
      let Value::String(bound) = bound else {
        return Err(type_error(format!("normpath() argument must be str, not {}", plans::py_type_name(bound))));
      };
      if plan_paths::posix_normpath(bound) == file {
        let v = plan_map(version)?.get("v").ok_or_else(|| key_error("v"))?;
        return Ok(Some((id_of(plan_fields)?.clone(), v.clone())));
      }
    }
  }
  Ok(None)
}

/// `_resolve_target_plan_for_amend(data, plan_id)`: the index of the plan an amend extends. A named plan must be open; with
/// no id, the one open lineage is the target.
fn resolve_target_plan_for_amend(plans: &[Value], plan_id: Option<&Value>) -> Result<usize, VerbError> {
  let Some(plan_id) = plan_id else {
    return match open_candidates(plans)?.as_slice() {
      [] => Err(VerbError::Refused("no open lineage to amend; pass --plan to amend an approved one".to_string())),
      [index] => Ok(*index),
      several => Err(VerbError::Refused(format!(
        "amend requires --plan (multiple open lineages: {})",
        candidate_ids(plans, several)?
      ))),
    };
  };
  require_open_plan(plans, plan_id)
}

/// The change of `approve`: the named plan, or the one open plan, gains a takeoff at its latest version.
fn approve_plan(plans: &mut [Value], plan_id: Option<&Value>) -> Result<Map<String, Value>, VerbError> {
  let index = match plan_id {
    Some(plan_id) => {
      let index = require_open_plan(plans, plan_id)?;
      if !absent(plan_map(&plans[index])?, "takeoff") {
        return Err(VerbError::Refused(format!("plan {} is already approved", pyfmt::python_str(plan_id))));
      }
      index
    }
    None => match open_candidates(plans)?.as_slice() {
      [] => return Err(VerbError::Refused("no plan available for approval".to_string())),
      [index] => *index,
      several => {
        return Err(VerbError::Refused(format!(
          "approve requires --plan (multiple candidates: {})",
          candidate_ids(plans, several)?
        )));
      }
    },
  };
  let plan = plan_map_mut(&mut plans[index])?;
  let v = latest_version_number(plan)?;
  plan.insert("takeoff".to_string(), json!({"v": v, "at": timefmt::utc_now_iso()}));
  Ok(plan.clone())
}

/// The change of `close`: the named plan gains a closed mark that names its mode.
fn close_plan(plans: &mut [Value], plan_id: &Value, close_as: &str) -> Result<Map<String, Value>, VerbError> {
  let index = require_plan(plans, plan_id)?;
  let plan = plan_map_mut(&mut plans[index])?;
  if !absent(plan, "closed") {
    return Err(VerbError::Refused(format!(
      "plan {} is already closed ({})",
      pyfmt::python_str(plan_id),
      closed_as_repr(plan)?
    )));
  }
  plan.insert("closed".to_string(), json!({"as": close_as, "at": timefmt::utc_now_iso()}));
  Ok(plan.clone())
}

/// `_require_open_plan(data, plan_id)`: the index of the plan with this id, which must not be closed.
fn require_open_plan(plans: &[Value], plan_id: &Value) -> Result<usize, VerbError> {
  let index = require_plan(plans, plan_id)?;
  let plan = plan_map(&plans[index])?;
  if !absent(plan, "closed") {
    return Err(VerbError::Refused(format!(
      "plan {} is closed ({})",
      pyfmt::python_str(plan_id),
      closed_as_repr(plan)?
    )));
  }
  Ok(index)
}

/// `require_plan(plans, plan_id)`: the index of the plan with this id. The scan stops at the first match, as Python's
/// `next` does, and a plan that is not a dict fails the scan before it.
fn require_plan(plans: &[Value], plan_id: &Value) -> Result<usize, VerbError> {
  for (index, plan) in plans.iter().enumerate() {
    if plan_map(plan)?.get("id") == Some(plan_id) {
      return Ok(index);
    }
  }
  Err(VerbError::Refused(format!("plan {} not found in session", pyfmt::python_str(plan_id))))
}

/// The indices of the plans with neither a closed mark nor a takeoff, in registry order.
fn open_candidates(plans: &[Value]) -> Result<Vec<usize>, VerbError> {
  let mut candidates = Vec::new();
  for (index, plan) in plans.iter().enumerate() {
    let plan = plan_map(plan)?;
    if absent(plan, "closed") && absent(plan, "takeoff") {
      candidates.push(index);
    }
  }
  Ok(candidates)
}

/// The ids of the plans at `candidates`, joined as the refusal messages list them.
fn candidate_ids(plans: &[Value], candidates: &[usize]) -> Result<String, VerbError> {
  let ids = candidates
    .iter()
    .map(|&index| plan_map(&plans[index]).and_then(id_of).map(pyfmt::python_str))
    .collect::<Result<Vec<_>, _>>()?;
  Ok(ids.join(", "))
}

/// `plan["versions"][-1]["v"]`: the number of the latest version. An empty list is the IndexError Python raises.
fn latest_version_number(plan: &Map<String, Value>) -> Result<Value, VerbError> {
  let latest = versions_of(plan)?
    .last()
    .ok_or_else(|| VerbError::Failed { kind: "IndexError", detail: "list index out of range".to_string() })?;
  plan_map(latest)?.get("v").cloned().ok_or_else(|| key_error("v"))
}

/// `max((p["id"] for p in plans), default=0) + 1`: the id a new lineage takes. Plan ids are integers, as the verbs write
/// them. Another type fails the comparison Python's `max` makes, and the verb answers 500.
fn next_plan_id(plans: &[Value]) -> Result<Value, VerbError> {
  let largest =
    max_integer(plans.iter().map(|plan| -> Result<i64, VerbError> { integer_of(id_of(plan_map(plan)?)?) }))?;
  let next = match largest {
    None => 1,
    Some(largest) => largest
      .checked_add(1)
      .ok_or_else(|| VerbError::Failed { kind: "OverflowError", detail: "plan id is too large".to_string() })?,
  };
  Ok(json!(next))
}

/// `max(ver["v"] for ver in versions) + 1`: the number of the next version. Python's `max` of an empty list is the ValueError
/// the verb answers 400 with.
fn next_version_number(versions: &[Value]) -> Result<Value, VerbError> {
  let largest = max_integer(versions.iter().map(|version| -> Result<i64, VerbError> {
    integer_of(plan_map(version)?.get("v").ok_or_else(|| key_error("v"))?)
  }))?;
  let largest = largest.ok_or_else(|| VerbError::Refused("max() iterable argument is empty".to_string()))?;
  let next = largest
    .checked_add(1)
    .ok_or_else(|| VerbError::Failed { kind: "OverflowError", detail: "version number is too large".to_string() })?;
  Ok(json!(next))
}

/// The largest of `values` as Python's `max` finds it, or `None` for no values. The first failing value stops the scan.
fn max_integer(values: impl Iterator<Item = Result<i64, VerbError>>) -> Result<Option<i64>, VerbError> {
  let mut largest = None;
  for value in values {
    let value = value?;
    largest = Some(largest.map_or(value, |largest: i64| largest.max(value)));
  }
  Ok(largest)
}

/// An integer field of the registry. A value of another type fails the comparison Python makes with it.
fn integer_of(value: &Value) -> Result<i64, VerbError> {
  value.as_i64().ok_or_else(|| type_error(format!("expected an integer, got {}", plans::py_type_name(value))))
}

/// The versions of a plan, as the `[...]` lookup and the `.get` of the verbs read them. A list is the only type that holds them.
fn versions_of(plan: &Map<String, Value>) -> Result<&Vec<Value>, VerbError> {
  match plan.get("versions").ok_or_else(|| key_error("versions"))? {
    Value::Array(versions) => Ok(versions),
    other => Err(type_error(format!("versions must be a list, got {}", plans::py_type_name(other)))),
  }
}

/// `plan.get(key) is None`: a missing key and a null value both read as None.
fn absent(plan: &Map<String, Value>, key: &str) -> bool {
  plan.get(key).is_none_or(Value::is_null)
}

/// `plan['closed']['as']!r`: the close mode a closed plan names, in Python's repr.
fn closed_as_repr(plan: &Map<String, Value>) -> Result<String, VerbError> {
  let closed = plan.get("closed").unwrap_or(&Value::Null);
  let Value::Object(closed) = closed else {
    return Err(type_error(format!("'{}' object is not subscriptable", plans::py_type_name(closed))));
  };
  Ok(plans::py_repr(closed.get("as").ok_or_else(|| key_error("as"))?))
}

/// `plan["id"]`: the plan's id, which a plan without one lacks.
fn id_of(plan: &Map<String, Value>) -> Result<&Value, VerbError> {
  plan.get("id").ok_or_else(|| key_error("id"))
}

/// `data["plans"]`: the registry's plans list. A top level that is not a dict, or a registry without a list of plans,
/// fails the lookup Python makes, so the verb answers 500.
fn plans_of(registry: &Value) -> Result<&Vec<Value>, VerbError> {
  let Value::Object(registry) = registry else {
    return Err(type_error(format!("'{}' object is not subscriptable", plans::py_type_name(registry))));
  };
  match registry.get("plans") {
    Some(Value::Array(plans)) => Ok(plans),
    Some(other) => Err(type_error(format!("plans must be a list, got {}", plans::py_type_name(other)))),
    None => Err(key_error("plans")),
  }
}

/// `data["plans"]` as a list to change.
fn plans_of_mut(registry: &mut Value) -> Result<&mut Vec<Value>, VerbError> {
  let Value::Object(registry) = registry else {
    return Err(type_error(format!("'{}' object is not subscriptable", plans::py_type_name(registry))));
  };
  match registry.get_mut("plans") {
    Some(Value::Array(plans)) => Ok(plans),
    Some(other) => Err(type_error(format!("plans must be a list, got {}", plans::py_type_name(other)))),
    None => Err(key_error("plans")),
  }
}

/// A plan or a version as a dict. A value that is not one fails the `.get` or the subscript Python makes on it.
fn plan_map(plan: &Value) -> Result<&Map<String, Value>, VerbError> {
  plan.as_object().ok_or_else(|| attribute_error(plan))
}

fn plan_map_mut(plan: &mut Value) -> Result<&mut Map<String, Value>, VerbError> {
  let kind = plans::py_type_name(plan);
  plan.as_object_mut().ok_or_else(|| attribute_error_named(kind))
}

fn attribute_error(plan: &Value) -> VerbError {
  attribute_error_named(plans::py_type_name(plan))
}

fn attribute_error_named(kind: &str) -> VerbError {
  VerbError::Failed { kind: "AttributeError", detail: format!("'{kind}' object has no attribute 'get'") }
}

fn key_error(key: &str) -> VerbError {
  VerbError::Failed { kind: "KeyError", detail: pyfmt::str_repr(key) }
}

fn type_error(detail: String) -> VerbError {
  VerbError::Failed { kind: "TypeError", detail }
}

fn escaped(error: plans::EscapedError) -> VerbError {
  VerbError::Failed { kind: error.kind, detail: error.detail }
}

#[cfg(test)]
mod tests {
  use std::fs;

  use charliebot_store::{Subscription, session_channel};

  use super::*;
  use crate::test_support::{
    APPROVED_REGISTRY, AWAITING_REGISTRY, CLOSED_REGISTRY, TWO_AWAITING_REGISTRY, plan_page, plan_session,
    session_with_registry, write_plan_artifact,
  };

  fn registry_bytes(features: &FeatureContext, session_id: &str) -> Vec<u8> {
    fs::read(features.store.session_dir(session_id).join(PLANS_FILE)).expect("the registry is on disk")
  }

  async fn next_frame(subscription: &mut Subscription) -> Value {
    serde_json::from_str(&subscription.recv().await.expect("the verb broadcasts a frame")).expect("the frame is JSON")
  }

  fn amendment(plan_id: Option<Value>, note: &str) -> Amendment {
    Amendment { plan_id, trigger: "feedback".to_string(), note: note.to_string(), base: Value::Null }
  }

  /// `present` of `file` as a new lineage titled `P1`, with no base.
  async fn present_p1(features: &FeatureContext, session_id: &str, file: &str) -> Result<Value, VerbError> {
    present(features, &SessionLocks::default(), session_id, file, "P1".to_string(), Value::Null).await
  }

  #[tokio::test]
  async fn present_returns_awaiting_approval() {
    let (_home, features) = plan_session("sess", None);
    write_plan_artifact(&features, "sess", "plan_01.html", &plan_page("Ship the fix."));
    let result = present_p1(&features, "sess", "artifacts/plan_01.html").await;
    assert_eq!(result.expect("the presentation succeeds"), json!({"plan": 1, "v": 1, "state": "awaiting approval"}));
  }

  /// A file bound by one version refuses a second binding under its other spelling, the relative or the absolute one.
  async fn cross_format_duplicate(first_absolute: bool, amend_second: bool) {
    let (_home, features) = plan_session("sess", None);
    let relative = write_plan_artifact(&features, "sess", "plan_01.html", &plan_page("Ship the fix."));
    let absolute = fs::canonicalize(features.store.session_dir("sess").join(&relative))
      .expect("the artifact resolves")
      .to_string_lossy()
      .into_owned();
    let (first, second) = if first_absolute { (&absolute, &relative) } else { (&relative, &absolute) };
    let locks = SessionLocks::default();
    present(&features, &locks, "sess", first, "P1".to_string(), Value::Null)
      .await
      .expect("the first registration succeeds");
    let error = if amend_second {
      amend(&features, &locks, "sess", second, amendment(Some(json!(1)), "why changed")).await
    } else {
      present(&features, &locks, "sess", second, "P2".to_string(), Value::Null).await
    };
    let message = format!("file {} already bound to plan 1 v1", pyfmt::str_repr(&plan_paths::posix_normpath(second)));
    assert!(matches!(&error, Err(VerbError::Refused(detail)) if *detail == message), "{error:?}");
  }

  #[tokio::test]
  async fn cross_format_duplicate_present_after_a_relative_present() {
    cross_format_duplicate(false, false).await;
  }

  #[tokio::test]
  async fn cross_format_duplicate_present_after_an_absolute_present() {
    cross_format_duplicate(true, false).await;
  }

  #[tokio::test]
  async fn cross_format_duplicate_amend_after_a_relative_present() {
    cross_format_duplicate(false, true).await;
  }

  #[tokio::test]
  async fn cross_format_duplicate_amend_after_an_absolute_present() {
    cross_format_duplicate(true, true).await;
  }

  #[tokio::test]
  async fn present_refuses_a_file_outside_the_session_directory() {
    let (home, features) = plan_session("sess", None);
    let outside = home.path().join("outside.html");
    fs::write(&outside, plan_page("Ship the fix.")).expect("the file is written");
    let error = present_p1(&features, "sess", &outside.to_string_lossy()).await.unwrap_err();
    let file = plan_paths::posix_normpath(&outside.to_string_lossy());
    assert!(
      matches!(&error, VerbError::Refused(detail) if *detail == format!("file {} resolves outside the session directory", pyfmt::str_repr(&file))),
      "{error:?}"
    );
  }

  #[tokio::test]
  async fn present_refuses_a_nul_byte_as_python_resolve_does() {
    let (_home, features) = plan_session("sess", None);
    let error = present_p1(&features, "sess", "artifacts/a\0b.html").await.unwrap_err();
    assert!(matches!(&error, VerbError::Refused(detail) if detail == "lstat: embedded null character in path"));
  }

  #[tokio::test]
  async fn present_rejects_goal_over_budget_with_measured_value() {
    let (_home, features) = plan_session("sess", None);
    write_plan_artifact(&features, "sess", "plan_01.html", &plan_page(&"x".repeat(241)));
    let error = present_p1(&features, "sess", "artifacts/plan_01.html").await.unwrap_err();
    assert!(
      matches!(&error, VerbError::Refused(detail) if detail.contains("241 weighted chars (budget 240)")),
      "{error:?}"
    );
  }

  #[tokio::test]
  async fn present_names_every_failed_assertion_with_the_measure_hint() {
    let (_home, features) = plan_session("sess", None);
    let broken = plan_page("Ship the fix.")
      .replace("<span class=\"n\">4</span>", "<span class=\"n\">9</span>")
      .replace("<div class=\"foot\"><p>How to respond.</p></div>", "");
    write_plan_artifact(&features, "sess", "plan_01.html", &broken);
    let error = present_p1(&features, "sess", "artifacts/plan_01.html").await.unwrap_err();
    let VerbError::Refused(detail) = &error else { panic!("a failed check is a refusal: {error:?}") };
    assert!(detail.starts_with("plan artifact fails 2 check assertion(s): sections-numbered: "), "{detail}");
    assert!(detail.contains("; foot-present: "), "{detail}");
    assert!(
      detail
        .ends_with(". Measure locally with: charliebot artifact check <artifact.html> --genre plan --assertions-only"),
      "{detail}"
    );
  }

  #[tokio::test]
  async fn present_refuses_a_page_with_a_raw_tsv_link_before_registering_it() {
    let (_home, features) = plan_session("sess", None);
    let page = plan_page(r#"The result is <a href="https://example.invalid/result.tsv">here</a>."#);
    write_plan_artifact(&features, "sess", "plan_01.html", &page);

    let error = present_p1(&features, "sess", "artifacts/plan_01.html").await.unwrap_err();

    let VerbError::Refused(detail) = error else { panic!("the raw link is refused: {error:?}") };
    assert!(detail.starts_with("plan artifact fails 1 check assertion(s): link-renderable: "), "{detail}");
    assert!(detail.contains("result.tsv"), "{detail}");
    assert!(detail.contains("block"), "{detail}");
    assert!(detail.contains("link an HTML page, or the directory that holds the file"), "{detail}");
    assert!(!features.store.session_dir("sess").join(PLANS_FILE).exists());
  }

  #[tokio::test]
  async fn amend_requires_non_empty_note() {
    let (_home, features) = plan_session("sess", None);
    write_plan_artifact(&features, "sess", "plan_01.html", &plan_page("Ship the fix."));
    present_p1(&features, "sess", "artifacts/plan_01.html").await.expect("the first registration succeeds");
    let f2 = write_plan_artifact(&features, "sess", "plan_02.html", &plan_page("Ship the fix."));
    let error =
      amend(&features, &SessionLocks::default(), "sess", &f2, amendment(Some(json!(1)), "")).await.unwrap_err();
    assert!(
      matches!(&error, VerbError::Refused(detail) if detail.starts_with("amend requires a non-empty --note")),
      "{error:?}"
    );
  }

  #[tokio::test]
  async fn amend_requires_non_empty_note_whitespace() {
    let (_home, features) = plan_session("sess", None);
    write_plan_artifact(&features, "sess", "plan_01.html", &plan_page("Ship the fix."));
    present_p1(&features, "sess", "artifacts/plan_01.html").await.expect("the first registration succeeds");
    let f2 = write_plan_artifact(&features, "sess", "plan_02.html", &plan_page("Ship the fix."));
    let error =
      amend(&features, &SessionLocks::default(), "sess", &f2, amendment(Some(json!(1)), "   ")).await.unwrap_err();
    assert!(
      matches!(&error, VerbError::Refused(detail) if detail.starts_with("amend requires a non-empty --note")),
      "{error:?}"
    );
  }

  #[tokio::test]
  async fn plans_json_shape_matches_schema() {
    let (_home, features) = plan_session("sess", None);
    write_plan_artifact(&features, "sess", "plan_01.html", &plan_page("Ship the fix."));
    let base = json!({"repo": "r", "branch": "b", "sha": "s"});
    present(&features, &SessionLocks::default(), "sess", "artifacts/plan_01.html", "P1".to_string(), base.clone())
      .await
      .expect("the presentation succeeds");
    let data: Value = serde_json::from_slice(&registry_bytes(&features, "sess")).expect("the registry is JSON");
    assert_eq!(data.as_object().expect("the registry is a dict").keys().collect::<Vec<_>>(), ["plans"]);
    let plan = &data["plans"][0];
    assert_eq!(
      plan.as_object().expect("a plan is a dict").keys().collect::<Vec<_>>(),
      ["id", "title", "versions", "takeoff", "closed"]
    );
    let version = &plan["versions"][0];
    assert_eq!(
      version.as_object().expect("a version is a dict").keys().collect::<Vec<_>>(),
      ["v", "file", "created_at", "trigger", "base", "note"]
    );
    assert_eq!(version["v"], 1);
    assert_eq!(version["file"], "artifacts/plan_01.html");
    assert_eq!(version["trigger"], "initial");
    assert_eq!(version["base"], base);
    assert_eq!(version["note"], Value::Null);
    assert_eq!(plan["takeoff"], Value::Null);
    assert_eq!(plan["closed"], Value::Null);
  }

  #[tokio::test]
  async fn present_broadcasts_the_frame_and_marks_the_sidebar() {
    let (_home, features) = plan_session("frames-present", None);
    write_plan_artifact(&features, "frames-present", "plan_01.html", &plan_page("Ship the fix."));
    let mut frames = features.store.subscribe(&session_channel("frames-present"));
    present_p1(&features, "frames-present", "artifacts/plan_01.html").await.expect("the presentation succeeds");
    assert_eq!(
      next_frame(&mut frames).await,
      json!({"type": "plan_updated", "session_id": "frames-present", "plan_id": 1})
    );
    assert!(sidebar_state::is_dirty("frames-present"));
  }

  #[tokio::test]
  async fn amend_appends_a_version_and_clears_the_takeoff() {
    let (_home, features) = plan_session("sess", Some(APPROVED_REGISTRY));
    let f2 = write_plan_artifact(&features, "sess", "plan_02.html", &plan_page("Ship the fix."));
    let result = amend(&features, &SessionLocks::default(), "sess", &f2, amendment(Some(json!(1)), "split it"))
      .await
      .expect("the amendment succeeds");
    assert_eq!(result, json!({"plan": 1, "v": 2, "state": "awaiting approval"}));
    let data: Value = serde_json::from_slice(&registry_bytes(&features, "sess")).expect("the registry is JSON");
    assert_eq!(data["plans"][0]["takeoff"], Value::Null);
    assert_eq!(data["plans"][0]["versions"][1]["note"], "split it");
  }

  #[tokio::test]
  async fn amend_refuses_without_an_open_lineage_or_with_several() {
    let (_home, features) = plan_session("closed", Some(CLOSED_REGISTRY));
    let f2 = write_plan_artifact(&features, "closed", "plan_02.html", &plan_page("Ship the fix."));
    let error = amend(&features, &SessionLocks::default(), "closed", &f2, amendment(None, "why")).await.unwrap_err();
    assert!(
      matches!(&error, VerbError::Refused(detail) if detail == "no open lineage to amend; pass --plan to amend an approved one"),
      "{error:?}"
    );
    let (_home, features) = plan_session("two", Some(TWO_AWAITING_REGISTRY));
    let f3 = write_plan_artifact(&features, "two", "plan_03.html", &plan_page("Ship the fix."));
    let error = amend(&features, &SessionLocks::default(), "two", &f3, amendment(None, "why")).await.unwrap_err();
    assert!(
      matches!(&error, VerbError::Refused(detail) if detail == "amend requires --plan (multiple open lineages: 1, 2)"),
      "{error:?}"
    );
  }

  #[tokio::test]
  async fn approve_returns_approved() {
    let (_home, features) = session_with_registry("sess", AWAITING_REGISTRY);
    let result = approve(&features, &SessionLocks::default(), "sess", None).await.expect("the approval succeeds");
    assert_eq!(result, json!({"plan": 1, "v": 1, "state": "approved"}));
  }

  #[tokio::test]
  async fn close_superseded_and_abandoned() {
    let (_home, features) = session_with_registry("sess", TWO_AWAITING_REGISTRY);
    let locks = SessionLocks::default();
    let first = close(&features, &locks, "sess", json!(1), "superseded".to_string()).await;
    assert_eq!(first.expect("the first close succeeds"), json!({"plan": 1, "state": "superseded"}));
    let second = close(&features, &locks, "sess", json!(2), "abandoned".to_string()).await;
    assert_eq!(second.expect("the second close succeeds"), json!({"plan": 2, "state": "abandoned"}));
  }

  #[tokio::test]
  async fn closing_already_closed_rejected() {
    let (_home, features) = session_with_registry("sess", AWAITING_REGISTRY);
    let locks = SessionLocks::default();
    close(&features, &locks, "sess", json!(1), "superseded".to_string()).await.expect("the first close succeeds");
    let error = close(&features, &locks, "sess", json!(1), "abandoned".to_string()).await.unwrap_err();
    assert!(matches!(&error, VerbError::Refused(message) if message == "plan 1 is already closed ('superseded')"));
  }

  #[tokio::test]
  async fn approve_refuses_each_plan_it_cannot_approve_with_the_python_message() {
    let cases = [
      (TWO_AWAITING_REGISTRY, None, "approve requires --plan (multiple candidates: 1, 2)"),
      (CLOSED_REGISTRY, None, "no plan available for approval"),
      (APPROVED_REGISTRY, Some(json!(1)), "plan 1 is already approved"),
      (CLOSED_REGISTRY, Some(json!(1)), "plan 1 is closed ('completed')"),
      (AWAITING_REGISTRY, Some(json!(9)), "plan 9 not found in session"),
    ];
    for (index, (text, plan_id, message)) in cases.into_iter().enumerate() {
      let session_id = format!("refusal-{index}");
      let (_home, features) = session_with_registry(&session_id, text);
      let before = registry_bytes(&features, &session_id);
      let error = approve(&features, &SessionLocks::default(), &session_id, plan_id).await.unwrap_err();
      assert!(matches!(&error, VerbError::Refused(detail) if detail == message), "{error:?}");
      assert_eq!(registry_bytes(&features, &session_id), before, "a refused approval writes nothing");
      assert!(!sidebar_state::is_dirty(&session_id), "a refused approval marks nothing");
    }
  }

  #[tokio::test]
  async fn approve_broadcasts_the_frame_and_marks_the_sidebar() {
    let (_home, features) = session_with_registry("frames-approve", AWAITING_REGISTRY);
    let mut frames = features.store.subscribe(&session_channel("frames-approve"));
    assert!(!sidebar_state::is_dirty("frames-approve"));
    approve(&features, &SessionLocks::default(), "frames-approve", None).await.expect("the approval succeeds");
    assert_eq!(
      next_frame(&mut frames).await,
      json!({"type": "plan_updated", "session_id": "frames-approve", "plan_id": 1})
    );
    assert!(sidebar_state::is_dirty("frames-approve"));
  }

  #[tokio::test]
  async fn close_broadcasts_the_frame_and_marks_the_sidebar() {
    let (_home, features) = session_with_registry("frames-close", AWAITING_REGISTRY);
    let mut frames = features.store.subscribe(&session_channel("frames-close"));
    assert!(!sidebar_state::is_dirty("frames-close"));
    close(&features, &SessionLocks::default(), "frames-close", json!(1), "completed".to_string())
      .await
      .expect("the close succeeds");
    assert_eq!(
      next_frame(&mut frames).await,
      json!({"type": "plan_updated", "session_id": "frames-close", "plan_id": 1})
    );
    assert!(sidebar_state::is_dirty("frames-close"));
  }
}
