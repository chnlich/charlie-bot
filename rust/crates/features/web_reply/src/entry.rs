//! The web reply package's registrations: the internal router and its path claim, the turn and sidebar contributions,
//! and the web-reply status field of each session.

use std::sync::Arc;

use axum::Router;
use axum::routing::post;
use charliebot_compat::routing::{get_only, post_only, refuse_other_methods};
use charliebot_core::event_types;
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{FeatureContext, Registry, RegistryError};
use charliebot_store::metadata::UtcDateTime;
use serde_json::{Map, Value, json};

use crate::turn_contribution::WebReplyTurnContribution;
use crate::{api, sidebar};

/// `register`: the `/api/internal` router, then the turn and sidebar contributions, then the
/// web-reply status field of each session.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(routes, matches)?;
  registry.turn_contribution(event_types::WEB_REPLY, Arc::new(WebReplyTurnContribution))?;
  registry.sidebar_contribution(event_types::WEB_REPLY, Arc::new(sidebar::WebReplySidebar))?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: event_types::WEB_REPLY,
      on: MetadataFile::Session,
      after: None,
      fields: vec![SlotField { name: event_types::WEB_STATUS, default: serde_json::Value::Null }],
    },
    validate_session_fields,
  )
}

pub(crate) fn validate_session_fields(values: &mut Map<String, Value>) -> Option<String> {
  if let Some(status) = values.get_mut(event_types::WEB_STATUS)
    && !normalize_web_status(status)
  {
    return Some("web_status must be a WebStatus or null".to_string());
  }
  None
}

fn normalize_web_status(value: &mut Value) -> bool {
  if value.is_null() {
    return true;
  }
  let Some(status) = value.as_object_mut() else { return false };
  status.retain(|name, _| ["goal", "now", "remaining", "waiting", "updated_at"].contains(&name.as_str()));
  for name in ["goal", "now"] {
    if status.get(name).is_some_and(|field| {
      !field.is_null() && (!field.is_string() || field.as_str().is_some_and(|text| text.trim().is_empty()))
    }) {
      return false;
    }
    status.entry(name.to_string()).or_insert(Value::Null);
  }
  let remaining = status.entry("remaining".to_string()).or_insert_with(|| json!([]));
  let Some(milestones) = remaining.as_array() else { return false };
  if milestones.iter().any(|milestone| !milestone.as_str().is_some_and(|text| !text.trim().is_empty())) {
    return false;
  }
  let waiting = status.entry("waiting".to_string()).or_insert_with(|| json!([]));
  let Some(items) = waiting.as_array_mut() else { return false };
  for item in items {
    let Some(item) = item.as_object_mut() else { return false };
    if item.keys().any(|name| !["text", "context", "link", "since", "since_exact"].contains(&name.as_str())) {
      return false;
    }
    if !item.get("text").and_then(Value::as_str).is_some_and(|text| !text.trim().is_empty()) {
      return false;
    }
    if item.get("link").is_some_and(|link| !link.is_null() && !link.is_string()) {
      return false;
    }
    match item.get("context") {
      None => {}
      Some(Value::Null) => {
        item.remove("context");
      }
      Some(Value::String(context)) if !context.trim().is_empty() => {}
      Some(_) => return false,
    }
    // `since` and `since_exact` are written by the server; an item stored before they existed carries neither, so a
    // null reads as absent, the same as a key the item leaves out.
    match item.get("since") {
      None => {}
      Some(Value::Null) => {
        item.remove("since");
      }
      Some(Value::String(value)) if UtcDateTime::parse(value).is_ok() => {}
      Some(_) => return false,
    }
    match item.get("since_exact") {
      None => {}
      Some(Value::Null) => {
        item.remove("since_exact");
      }
      Some(Value::Bool(_)) => {}
      Some(_) => return false,
    }
    item.entry("link".to_string()).or_insert(Value::Null);
  }
  let updated_at = status.entry("updated_at".to_string()).or_insert_with(|| Value::String(UtcDateTime::now().to_str()));
  updated_at.as_str().is_some_and(|value| UtcDateTime::parse(value).is_ok())
}

/// The web reply routes, with the feature context as their state. The reply and dialog paths have one method each; the
/// status path answers POST and GET, and the 405 body names the first route of the path, which is the POST route.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/internal/web/reply", post_only(api::post_reply))
    .route("/api/internal/web/reply/unread", post_only(api::post_reply_unread))
    .route("/api/internal/web/reply-check", post_only(api::post_reply_check))
    .route("/api/internal/web/dialog", get_only(api::get_dialog))
    .route("/api/internal/web/status", refuse_other_methods(post(api::post_status).get(api::get_status), "POST"))
    .route("/api/internal/web/status/confirm", post_only(api::post_status_confirm))
    .route("/api/internal/web/status/unread", post_only(api::post_status_unread))
    .route("/api/internal/web/ask-check", post_only(api::post_ask_check))
    .with_state(Arc::clone(features))
}

/// Whether a path is one of the web reply routes, whatever the method. The server's trailing-slash redirect asks this
/// of the slash-toggled path.
fn matches(path: &str) -> bool {
  matches!(
    path,
    "/api/internal/web/reply"
      | "/api/internal/web/reply/unread"
      | "/api/internal/web/reply-check"
      | "/api/internal/web/dialog"
      | "/api/internal/web/status"
      | "/api/internal/web/status/confirm"
      | "/api/internal/web/status/unread"
      | "/api/internal/web/ask-check"
  )
}
