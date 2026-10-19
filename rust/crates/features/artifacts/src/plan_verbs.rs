//! The plan verbs `approve` and `close` (`src/features/artifacts/plans.py`, `PlanRegistryManager`). Each verb changes one
//! lineage in a session's `plans.json`: it loads the registry under the session's lock, changes the plan, saves the
//! registry and marks the sidebar dirty, then broadcasts `plan_updated` after the lock is released. The reads and the
//! projection belong to `plans`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use charliebot_core::{fsutil, pyfmt, timefmt};
use charliebot_registry::FeatureContext;
use charliebot_store::sidebar_state;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, OwnedMutexGuard};

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

/// Loads the session's registry under its lock, applies `change` to its plans list, and saves the list. The verb's
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
  let path = features.store.session_dir(session_id).join(PLANS_FILE);
  let _guard = locks.lock(session_id).await;
  let session_id = session_id.to_string();
  tokio::task::spawn_blocking(move || -> Result<Map<String, Value>, VerbError> {
    let mut registry = load(&path)?;
    let plans = plans_of(&mut registry)?;
    let plan = change(plans)?;
    save(&path, &session_id, plans)?;
    Ok(plan)
  })
  .await
  .map_err(|error| VerbError::Failed { kind: "RuntimeError", detail: error.to_string() })?
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

/// `data["plans"]`: the registry's plans list. A top level that is not a dict, or a registry without a list of plans,
/// fails the lookup Python makes, so the verb answers 500.
fn plans_of(registry: &mut Value) -> Result<&mut Vec<Value>, VerbError> {
  let Value::Object(registry) = registry else {
    return Err(type_error(format!("'{}' object is not subscriptable", plans::py_type_name(registry))));
  };
  match registry.get_mut("plans") {
    Some(Value::Array(plans)) => Ok(plans),
    Some(other) => Err(type_error(format!("plans must be a list, got {}", plans::py_type_name(other)))),
    None => Err(key_error("plans")),
  }
}

/// `_save`: writes the projected registry, then marks the session's sidebar row dirty. The write creates no directory,
/// so a session directory that is gone fails the save.
fn save(path: &Path, session_id: &str, plans: &[Value]) -> Result<(), VerbError> {
  let projected = plans::project_registry(plans).map_err(VerbError::Refused)?;
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

/// The change of `approve`: the named plan, or the one open plan, gains a takeoff at its latest version.
fn approve_plan(plans: &mut [Value], plan_id: Option<&Value>) -> Result<Map<String, Value>, VerbError> {
  let index = match plan_id {
    Some(plan_id) => {
      let index = require_plan(plans, plan_id)?;
      let plan = plan_map(&plans[index])?;
      if !absent(plan, "closed") {
        return Err(VerbError::Refused(format!(
          "plan {} is closed ({})",
          pyfmt::python_str(plan_id),
          closed_as_repr(plan)?
        )));
      }
      if !absent(plan, "takeoff") {
        return Err(VerbError::Refused(format!("plan {} is already approved", pyfmt::python_str(plan_id))));
      }
      index
    }
    None => single_candidate(plans)?,
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

/// The one plan a bare `approve` names: the only plan with neither a closed mark nor a takeoff.
fn single_candidate(plans: &[Value]) -> Result<usize, VerbError> {
  let mut candidates = Vec::new();
  for (index, plan) in plans.iter().enumerate() {
    let plan = plan_map(plan)?;
    if absent(plan, "closed") && absent(plan, "takeoff") {
      candidates.push(index);
    }
  }
  match candidates.as_slice() {
    [] => Err(VerbError::Refused("no plan available for approval".to_string())),
    [index] => Ok(*index),
    several => {
      let ids = several
        .iter()
        .map(|&index| plan_map(&plans[index]).and_then(id_of).map(pyfmt::python_str))
        .collect::<Result<Vec<_>, _>>()?;
      Err(VerbError::Refused(format!("approve requires --plan (multiple candidates: {})", ids.join(", "))))
    }
  }
}

/// `plan["versions"][-1]["v"]`: the number of the latest version. An empty list is the IndexError Python raises.
fn latest_version_number(plan: &Map<String, Value>) -> Result<Value, VerbError> {
  let versions = match plan.get("versions").ok_or_else(|| key_error("versions"))? {
    Value::Array(versions) => versions,
    other => return Err(type_error(format!("versions must be a list, got {}", plans::py_type_name(other)))),
  };
  let latest = versions
    .last()
    .ok_or_else(|| VerbError::Failed { kind: "IndexError", detail: "list index out of range".to_string() })?;
  let Value::Object(latest) = latest else {
    return Err(type_error(format!("version must be a dict, got {}", plans::py_type_name(latest))));
  };
  latest.get("v").cloned().ok_or_else(|| key_error("v"))
}

/// `plan['closed']['as']!r`: the close mode a closed plan names, in Python's repr.
fn closed_as_repr(plan: &Map<String, Value>) -> Result<String, VerbError> {
  let closed = plan.get("closed").unwrap_or(&Value::Null);
  let Value::Object(closed) = closed else {
    return Err(type_error(format!("'{}' object is not subscriptable", plans::py_type_name(closed))));
  };
  Ok(plans::py_repr(closed.get("as").ok_or_else(|| key_error("as"))?))
}

/// `plan.get(key) is None`: a missing key and a null value both read as None.
fn absent(plan: &Map<String, Value>, key: &str) -> bool {
  plan.get(key).is_none_or(Value::is_null)
}

/// `plan["id"]`: the plan's id, which a plan without one lacks.
fn id_of(plan: &Map<String, Value>) -> Result<&Value, VerbError> {
  plan.get("id").ok_or_else(|| key_error("id"))
}

/// A plan as a dict. A value that is not one fails the `.get` Python calls on it.
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
    APPROVED_REGISTRY, AWAITING_REGISTRY, CLOSED_REGISTRY, TWO_AWAITING_REGISTRY, session_with_registry,
  };

  fn registry_bytes(features: &FeatureContext, session_id: &str) -> Vec<u8> {
    fs::read(features.store.session_dir(session_id).join(PLANS_FILE)).expect("the registry is on disk")
  }

  async fn next_frame(subscription: &mut Subscription) -> Value {
    serde_json::from_str(&subscription.recv().await.expect("the verb broadcasts a frame")).expect("the frame is JSON")
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
