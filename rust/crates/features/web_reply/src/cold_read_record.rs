//! The one record of a command-side cold read: the `cold_read_check` event, its route, and the mapping that keeps the
//! legacy `web_reply_check` and `web_ask_check` records readable.
//!
//! A recorded check is named by its row in `charliebot_tasks::cold_read::REGISTRY`: `reply`, `ask` or `settle`. Every
//! writer writes one `cold_read_check` event carrying that name, and every reader asks
//! [`cold_read_record_name`] what a stored record was, so no log is migrated: a `web_reply_check` record reads as
//! `reply` and a `web_ask_check` record reads as `ask`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::response::Response;
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, RequestBody, text_field};
use charliebot_compat::validation::{self, FieldError, Kind, Loc};
use charliebot_core::event_types::{COLD_READ_CHECK, WEB_ASK_CHECK, WEB_REPLY_CHECK};
use charliebot_registry::FeatureContext;
use charliebot_registry::caller;
use charliebot_registry::colleague_gate::refuse_colleague_work;
use charliebot_store::CompactValue;
use charliebot_tasks::cold_read;
use serde_json::{Map, Value, json};

use crate::reply::{self, WebReplyError};
use crate::status;

/// The event fields the record-name mapping reads, so the JSON form and the store's compact form share one mapping.
pub trait RecordFields {
  fn record_type(&self) -> Option<&str>;
  fn record_name_field(&self) -> Option<&str>;
}

impl RecordFields for Value {
  fn record_type(&self) -> Option<&str> {
    self.get("type").and_then(Value::as_str)
  }

  fn record_name_field(&self) -> Option<&str> {
    self.get("name").and_then(Value::as_str)
  }
}

impl RecordFields for CompactValue {
  fn record_type(&self) -> Option<&str> {
    self.get("type").and_then(CompactValue::as_str)
  }

  fn record_name_field(&self) -> Option<&str> {
    self.get("name").and_then(CompactValue::as_str)
  }
}

/// The check a cold-read record belongs to: the event's own `name` for a `cold_read_check`, and the mapped name of a
/// legacy type. `None` for any other event.
pub fn cold_read_record_name<E: RecordFields + ?Sized>(event: &E) -> Option<&str> {
  match event.record_type()? {
    COLD_READ_CHECK => event.record_name_field(),
    WEB_REPLY_CHECK => Some(cold_read::REPLY),
    WEB_ASK_CHECK => Some(cold_read::ASK),
    _ => None,
  }
}

/// The `content` line of each recorded check's event.
fn content(name: &str) -> &'static str {
  match name {
    cold_read::REPLY => "web reply check",
    cold_read::ASK => "Cold read of a Waiting on you item",
    cold_read::SETTLE => "Cold read of the kept Waiting on you items",
    other => unreachable!("no cold-read record is named {other}"),
  }
}

/// One cold read to record: the check's name, the digest field and value it read, its backend and its answer, with the
/// reply's findings and run position when it read a reply.
pub struct CheckRecord<'a> {
  pub name: &'a str,
  pub backend: &'a str,
  pub answer: Option<&'a str>,
  pub digest_field: &'a str,
  pub digest: &'a str,
  pub findings: Option<&'a [String]>,
  pub run: Option<(&'a str, usize)>,
}

/// Record one `cold_read_check` event and name its check.
pub async fn record(features: &FeatureContext, session_id: &str, record: CheckRecord<'_>) -> Result<(), WebReplyError> {
  let mut event = Map::new();
  event.insert("type".to_string(), json!(COLD_READ_CHECK));
  event.insert("content".to_string(), json!(content(record.name)));
  event.insert("name".to_string(), json!(record.name));
  event.insert(record.digest_field.to_string(), json!(record.digest));
  event.insert("backend".to_string(), json!(record.backend));
  if let Some(answer) = record.answer {
    event.insert("answer".to_string(), json!(answer));
  }
  if let Some(findings) = record.findings {
    event.insert("findings".to_string(), json!(findings));
  }
  if let Some((run_id, reply_index)) = record.run {
    event.insert("run_id".to_string(), json!(run_id));
    event.insert("reply_index".to_string(), json!(reply_index));
  }
  features.store.persist_and_broadcast(session_id, Value::Object(event)).await?;
  tracing::info!(
    session = %session_id,
    name = record.name,
    backend = record.backend,
    "cold_read_check_recorded"
  );
  Ok(())
}

/// `POST /api/internal/web/cold-read-check`: records one cold read of a reply, a Waiting on you item, or the kept
/// items of a status set.
pub async fn post_cold_read_check(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let model = match ColdReadCheck::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  let identity = match caller::resolve_caller(&parts.headers, &features) {
    Ok(identity) => identity,
    Err(response) => return response,
  };
  if let Some(refusal) = refuse_colleague_work(&features, &identity).await {
    return refusal;
  }
  let run_id = identity.claims().map(|claims| claims.run_id.as_str());
  answer(dispatch(&features, &model, run_id).await)
}

/// The validated body of one `cold_read_check` request.
struct ColdReadCheck {
  session_id: String,
  name: String,
  backend: String,
  text: Option<String>,
  item: Option<Value>,
  findings: Option<Vec<String>>,
  answer: Option<String>,
}

impl ColdReadCheck {
  /// Validates the body against `COLD_READ_CHECK_BODY`, then refuses the fields the named check does not carry and
  /// requires the fields it needs.
  fn validate(body: &RequestBody) -> Result<Self, Box<Response>> {
    let mut fields = body.declared_fields(&COLD_READ_CHECK_BODY)?;
    let name = text_field(&fields, "name");
    let forbidden: &[&str] = if name == cold_read::REPLY { &["item"] } else { &["text", "findings"] };
    let required: &[&str] = if name == cold_read::REPLY { &["text", "findings"] } else { &["item", "answer"] };
    let mut errors = Vec::new();
    for field in forbidden {
      if let Some(value) = fields.remove(*field) {
        errors.push(FieldError {
          loc: vec![Loc::Field("body".to_string()), Loc::Field((*field).to_string())],
          input: value,
          kind: Kind::ExtraForbidden,
        });
      }
    }
    for field in required {
      if fields.get(*field).is_none_or(Value::is_null) {
        errors.push(FieldError {
          loc: vec![Loc::Field("body".to_string()), Loc::Field((*field).to_string())],
          input: Value::Null,
          kind: Kind::Missing,
        });
      }
    }
    if !errors.is_empty() {
      return Err(Box::new(validation::response(&errors)));
    }
    let text = optional_string(&mut fields, "text");
    let item = fields.remove("item");
    let findings = match fields.remove("findings") {
      Some(Value::Array(items)) => Some(
        items
          .into_iter()
          .map(|item| match item {
            Value::String(text) => text,
            _ => unreachable!("a validated findings item is a string"),
          })
          .collect(),
      ),
      Some(Value::Null) | None => None,
      Some(_) => unreachable!("a validated findings is a list"),
    };
    let answer = optional_string(&mut fields, "answer");
    Ok(Self { session_id: text_field(&fields, "session_id"), name, backend: text_field(&fields, "backend"), text, item, findings, answer })
  }
}

/// The string a nullable-string field holds, or `None` when it is absent or null.
fn optional_string(fields: &mut Map<String, Value>, name: &str) -> Option<String> {
  match fields.remove(name) {
    Some(Value::String(text)) => Some(text),
    Some(Value::Null) | None => None,
    Some(_) => unreachable!("a validated {name} is a string or null"),
  }
}

/// Record the check the body names.
async fn dispatch(
  features: &FeatureContext,
  model: &ColdReadCheck,
  run_id: Option<&str>,
) -> Result<Value, WebReplyError> {
  match model.name.as_str() {
    cold_read::REPLY => {
      reply::record_reply_check(
        features,
        &model.session_id,
        model.text.as_deref().expect("a validated reply check carries its text"),
        &model.backend,
        model.findings.as_deref().expect("a validated reply check carries its findings"),
        model.answer.as_deref(),
        run_id,
      )
      .await
    }
    name => {
      status::record_item_check(
        features,
        &model.session_id,
        name,
        model.item.as_ref().expect("a validated item check carries its item"),
        &model.backend,
        model.answer.as_deref().expect("a validated item check carries its answer"),
      )
      .await
    }
  }
}

/// A handler's answer: the body with 200, a refusal with its status and detail, or the unhandled-exception 500.
fn answer(result: Result<Value, WebReplyError>) -> Response {
  match result {
    Ok(body) => charliebot_compat::responses::json(http::StatusCode::OK, &body),
    Err(WebReplyError::Refused { status, detail }) => {
      charliebot_compat::responses::json(status, &json!({"detail": detail}))
    }
    Err(WebReplyError::Failed(error)) => {
      charliebot_compat::responses::internal_error("RuntimeError", format!("{error:#}"))
    }
  }
}

/// The fields of one `ColdReadCheckRequest` in declaration order.
const COLD_READ_CHECK_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required("name", FieldType::Literal(cold_read::RECORDED_NAMES)),
  Field::required("backend", FieldType::String),
  Field::optional("text", FieldType::NullableString),
  Field::optional("item", FieldType::Dictionary),
  Field::optional("findings", FieldType::StringList),
  Field::optional("answer", FieldType::NullableString),
];
const COLD_READ_CHECK_BODY: BodyModel = BodyModel::required(COLD_READ_CHECK_FIELDS).forbid_extra();

#[cfg(test)]
mod tests {
  use charliebot_core::event_types::COLD_READ_CHECK;
  use serde_json::json;

  use super::*;

  #[test]
  fn the_mapping_names_a_unified_record_and_both_legacy_types() {
    let unified = json!({"type": COLD_READ_CHECK, "name": cold_read::SETTLE});
    assert_eq!(cold_read_record_name(&unified), Some(cold_read::SETTLE));
    let reply = json!({"type": WEB_REPLY_CHECK, "reply_sha256": "d"});
    assert_eq!(cold_read_record_name(&reply), Some(cold_read::REPLY));
    let ask = json!({"type": WEB_ASK_CHECK, "item_sha256": "d"});
    assert_eq!(cold_read_record_name(&ask), Some(cold_read::ASK));
    assert_eq!(cold_read_record_name(&json!({"type": "web_status"})), None);
    assert_eq!(cold_read_record_name(&json!({"type": COLD_READ_CHECK})), None, "a unified record names its check");
  }

  #[test]
  fn the_mapping_reads_the_stores_compact_form_the_same_way() {
    let unified = CompactValue::from_value(&json!({"type": COLD_READ_CHECK, "name": cold_read::ASK}));
    assert_eq!(cold_read_record_name(&unified), Some(cold_read::ASK));
    let reply = CompactValue::from_value(&json!({"type": WEB_REPLY_CHECK}));
    assert_eq!(cold_read_record_name(&reply), Some(cold_read::REPLY));
  }
}