//! The pool member a session response shows. A session's stored record keeps the member it last ran on. A response
//! shows that member only while the session's pool still lists it, which the core predicate
//! `BackendsConfig::display_pool_member` decides. The store does not filter: each response site calls these helpers.

use charliebot_core::config::BackendsConfig;
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::{Store, StoreResult};
use serde_json::{Map, Value, json};

use crate::model_pool::POOL_MEMBER_FIELD;

/// Replace the recorded `pool_member` of a rendered session object with the member it shows. An object without that
/// key is left as it is.
pub fn show_display_member(backends: &BackendsConfig, session: &mut Map<String, Value>) {
  if !session.contains_key(POOL_MEMBER_FIELD) {
    return;
  }
  let shown = session
    .get("backend")
    .and_then(Value::as_str)
    .zip(session.get(POOL_MEMBER_FIELD).and_then(Value::as_str))
    .and_then(|(backend, member)| backends.display_pool_member(backend, member));
  session.insert(POOL_MEMBER_FIELD.to_string(), shown.map_or(Value::Null, |member| json!(member)));
}

/// The session's stored metadata form, with the member it shows.
pub fn session_metadata_shown(store: &Store, meta: &SessionMetadata) -> Value {
  let mut value = store.session_metadata_value(meta);
  let object = value.as_object_mut().expect("session metadata serializes to an object");
  show_display_member(&store.config().backends, object);
  value
}

/// The session detail that the detail routes answer with, with the member it shows.
pub fn session_detail_shown(store: &Store, session_id: &str) -> StoreResult<Value> {
  let mut detail = store.session_detail(session_id)?;
  let object = detail.as_object_mut().expect("a session detail is an object");
  show_display_member(&store.config().backends, object);
  Ok(detail)
}
