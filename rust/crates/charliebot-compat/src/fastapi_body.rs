//! FastAPI request-body reading and pydantic model validation.
//!
//! A route reads the body before resolving dependencies, then validates the model after them. The reader preserves
//! raw bytes unless the strict content-type rule selects JSON; JSON bytes use CPython-compatible decoding.
//!
//! ```ignore
//! async fn update(request: Request<Body>) -> Response {
//!   let (parts, stream) = request.into_parts();
//!   let body = match read(&parts.headers, stream).await {
//!     Ok(body) => body,
//!     Err(response) => return *response,
//!   };
//!   let session = match require_session(&parts).await {
//!     Ok(session) => session,
//!     Err(response) => return response,
//!   };
//!   let model = match body.validate(&SESSION_BODY) {
//!     Ok(model) => model,
//!     Err(response) => return *response,
//!   };
//!   update_session(session, model).await
//! }
//! ```

use axum::body::{Body, to_bytes};
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, StatusCode, header};
use serde_json::{Map, Number, Value, json};

use crate::python_json::{self, LoadFailure};
use crate::{responses, validation};

/// The result of FastAPI's body read: no bytes, parsed JSON, or untouched bytes.
#[derive(Debug, Clone, PartialEq)]
pub enum RequestBody {
  Absent,
  Json(Value),
  Raw(Bytes),
}

/// Read the request body with FastAPI's default strict content-type behavior.
pub async fn read(headers: &HeaderMap, body: Body) -> Result<RequestBody, Box<Response>> {
  let raw = match to_bytes(body, usize::MAX).await {
    Ok(raw) => raw,
    Err(error) => {
      tracing::debug!(error = %error, "request body read failed");
      return Err(Box::new(body_parse_error()));
    }
  };
  if raw.is_empty() {
    return Ok(RequestBody::Absent);
  }

  let content_type = headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok());
  if !content_type.is_some_and(is_json_content_type) {
    return Ok(RequestBody::Raw(raw));
  }

  match python_json::loads_classified(&raw) {
    Ok(value) => Ok(RequestBody::Json(value)),
    Err(LoadFailure::JsonDecode { message, position, .. }) => {
      let error = validation::FieldError {
        loc: vec![validation::Loc::Field("body".to_string()), validation::Loc::Index(position)],
        input: json!({}),
        kind: validation::Kind::JsonInvalid { error: message },
      };
      Err(Box::new(validation::response(&[error])))
    }
    Err(error @ (LoadFailure::Decode(_) | LoadFailure::Unrepresentable(_))) => {
      tracing::debug!(error = ?error, "request body JSON decoding failed");
      Err(Box::new(body_parse_error()))
    }
  }
}

/// True when `email.message.Message` would select JSON for this content type.
pub fn is_json_content_type(value: &str) -> bool {
  let media_type = value.split(';').next().unwrap_or_default().trim();
  let mut components = media_type.split('/');
  let Some(main_type) = components.next() else {
    return false;
  };
  let Some(sub_type) = components.next() else {
    return false;
  };
  if components.next().is_some() || main_type.is_empty() || sub_type.is_empty() {
    return false;
  }
  main_type.eq_ignore_ascii_case("application")
    && (sub_type.eq_ignore_ascii_case("json") || sub_type.to_ascii_lowercase().ends_with("+json"))
}

/// A field type supported by the request models in the Python API. Equality is not derived: a nested model holds a
/// `ModelCheck` function pointer, which has no meaningful comparison.
#[derive(Debug, Clone, Copy)]
pub enum FieldType {
  String,
  NullableString,
  Integer,
  NullableInteger,
  Boolean,
  Literal(&'static [&'static str]),
  NullableLiteral(&'static [&'static str]),
  /// A `StrEnum` field: pydantic's `enum` error names the choices.
  Enum(&'static [&'static str]),
  NullableEnum(&'static [&'static str]),
  StringList,
  /// A nested model or null, validated with its own fields. `class_name` is the model's name in its `model_type`
  /// error.
  NullableModel {
    class_name: &'static str,
    model: &'static BodyModel,
  },
}

/// One model field in declaration order.
#[derive(Debug, Clone, Copy)]
pub struct Field {
  name: &'static str,
  field_type: FieldType,
  required: bool,
}

impl Field {
  /// Declare a required field.
  pub const fn required(name: &'static str, field_type: FieldType) -> Self {
    Self { name, field_type, required: true }
  }

  /// Declare a field whose Python model has a default.
  pub const fn optional(name: &'static str, field_type: FieldType) -> Self {
    Self { name, field_type, required: false }
  }
}

/// Whether a route's model body parameter itself is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyRequirement {
  Required,
  Optional,
}

/// Whether a model keeps the keys it does not declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtraKeys {
  /// pydantic's default `extra="ignore"`: an undeclared key is dropped.
  Ignore,
  /// `extra="forbid"`: each undeclared key is an `extra_forbidden` error.
  Forbid,
}

/// A model validator. It runs on the validated values once every field passes, and returns the message of the
/// `ValueError` it raises, or `None` when the values are valid.
pub type ModelCheck = fn(&Map<String, Value>) -> Option<String>;

/// A request model's fields, body-parameter requirement, extra-key policy and model validator.
#[derive(Debug, Clone, Copy)]
pub struct BodyModel {
  fields: &'static [Field],
  requirement: BodyRequirement,
  extra: ExtraKeys,
  check: Option<ModelCheck>,
}

impl BodyModel {
  /// Declare a required model body.
  pub const fn required(fields: &'static [Field]) -> Self {
    Self { fields, requirement: BodyRequirement::Required, extra: ExtraKeys::Ignore, check: None }
  }

  /// Declare an optional model body, or a nested model.
  pub const fn optional(fields: &'static [Field]) -> Self {
    Self { fields, requirement: BodyRequirement::Optional, extra: ExtraKeys::Ignore, check: None }
  }

  /// Whether the model declares a field named `name`.
  pub fn declares(&self, name: &str) -> bool {
    self.fields.iter().any(|field| field.name == name)
  }

  /// Refuse undeclared keys, as `extra="forbid"` does.
  pub const fn forbid_extra(mut self) -> Self {
    self.extra = ExtraKeys::Forbid;
    self
  }

  /// Run `check` on the validated values once every field passes.
  pub const fn checked(mut self, check: ModelCheck) -> Self {
    self.check = Some(check);
    self
  }
}

impl RequestBody {
  /// Validate this body against a model and return its declared fields, or the errors FastAPI reports for it.
  pub fn check(&self, model: &BodyModel) -> Result<Option<Value>, Vec<validation::FieldError>> {
    match self {
      // FastAPI passes a JSON `null` body as None, which a required model reports as missing.
      RequestBody::Absent | RequestBody::Json(Value::Null) => match model.requirement {
        BodyRequirement::Required => Err(vec![body_missing_error()]),
        BodyRequirement::Optional => Ok(None),
      },
      RequestBody::Json(Value::Object(object)) => validate_object(object, model, &body_loc()).map(Some),
      RequestBody::Json(value) => Err(vec![model_attributes_error(value.clone())]),
      RequestBody::Raw(bytes) => {
        let input = Value::String(String::from_utf8_lossy(bytes).into_owned());
        Err(vec![model_attributes_error(input)])
      }
    }
  }

  /// Validate this body against a model: its declared fields, or FastAPI's 422 response.
  pub fn validate(&self, model: &BodyModel) -> Result<Option<Value>, Box<Response>> {
    self.check(model).map_err(|errors| Box::new(validation::response(&errors)))
  }
}

fn validate_object(
  object: &Map<String, Value>,
  model: &BodyModel,
  loc: &[validation::Loc],
) -> Result<Value, Vec<validation::FieldError>> {
  let mut values = Map::new();
  let mut errors = Vec::new();
  for field in model.fields {
    let field_loc = child_loc(loc, field.name);
    match object.get(field.name) {
      Some(value) => match validate_field(field, value, &field_loc) {
        Ok(value) => {
          values.insert(field.name.to_string(), value);
        }
        Err(mut field_errors) => errors.append(&mut field_errors),
      },
      None if field.required => {
        errors.push(validation::FieldError { loc: field_loc, input: Value::Null, kind: validation::Kind::Missing })
      }
      None => {}
    }
  }
  if model.extra == ExtraKeys::Forbid {
    for (key, value) in object {
      if !model.fields.iter().any(|field| field.name == key.as_str()) {
        errors.push(validation::FieldError {
          loc: child_loc(loc, key),
          input: value.clone(),
          kind: validation::Kind::ExtraForbidden,
        });
      }
    }
  }
  if !errors.is_empty() {
    return Err(errors);
  }
  match model.check.and_then(|check| check(&values)) {
    Some(message) => Err(vec![validation::FieldError {
      loc: loc.to_vec(),
      input: Value::Object(object.clone()),
      kind: validation::Kind::ValueError { message },
    }]),
    None => Ok(Value::Object(values)),
  }
}

fn validate_field(field: &Field, value: &Value, loc: &[validation::Loc]) -> Result<Value, Vec<validation::FieldError>> {
  let failure = |kind| Err(vec![validation::FieldError { loc: loc.to_vec(), input: value.clone(), kind }]);
  match field.field_type {
    FieldType::String if value.is_string() => Ok(value.clone()),
    FieldType::NullableString if value.is_null() || value.is_string() => Ok(value.clone()),
    FieldType::String | FieldType::NullableString => failure(validation::Kind::StringType),
    FieldType::Integer | FieldType::NullableInteger => {
      match integer_value(value, matches!(field.field_type, FieldType::NullableInteger)) {
        Ok(value) => Ok(value),
        Err(kind) => failure(kind),
      }
    }
    FieldType::Boolean => match boolean_value(value) {
      Ok(value) => Ok(value),
      Err(kind) => failure(kind),
    },
    FieldType::Literal(choices) => match choice_value(value, choices) {
      Some(value) => Ok(value),
      None => failure(validation::Kind::Literal(choices)),
    },
    FieldType::NullableLiteral(_) if value.is_null() => Ok(Value::Null),
    FieldType::NullableLiteral(choices) => match choice_value(value, choices) {
      Some(value) => Ok(value),
      None => failure(validation::Kind::Literal(choices)),
    },
    FieldType::Enum(choices) => match choice_value(value, choices) {
      Some(value) => Ok(value),
      None => failure(validation::Kind::Enum(choices)),
    },
    FieldType::NullableEnum(_) if value.is_null() => Ok(Value::Null),
    FieldType::NullableEnum(choices) => match choice_value(value, choices) {
      Some(value) => Ok(value),
      None => failure(validation::Kind::Enum(choices)),
    },
    FieldType::StringList => match value.as_array() {
      Some(items) => {
        let mut errors = Vec::new();
        for (index, item) in items.iter().enumerate() {
          if !item.is_string() {
            let mut item_loc = loc.to_vec();
            item_loc.push(validation::Loc::Index(index));
            errors.push(validation::FieldError {
              loc: item_loc,
              input: item.clone(),
              kind: validation::Kind::StringType,
            });
          }
        }
        if errors.is_empty() { Ok(value.clone()) } else { Err(errors) }
      }
      None => failure(validation::Kind::ListType),
    },
    FieldType::NullableModel { class_name, model } => match value {
      Value::Null => Ok(Value::Null),
      Value::Object(object) => validate_object(object, model, loc),
      _ => failure(validation::Kind::ModelType { class_name }),
    },
  }
}

/// The string value when it is one of `choices`.
fn choice_value(value: &Value, choices: &[&str]) -> Option<Value> {
  value.as_str().filter(|text| choices.contains(text)).map(Value::from)
}

fn integer_value(value: &Value, nullable: bool) -> Result<Value, validation::Kind> {
  if value.is_null() && nullable {
    return Ok(Value::Null);
  }
  match value {
    Value::Bool(value) => Ok(Value::Number(Number::from(if *value { 1 } else { 0 }))),
    Value::Number(number) if number.is_i64() || number.is_u64() => Ok(value.clone()),
    Value::Number(number) => match number.as_f64() {
      Some(number) if number.fract() == 0.0 => {
        if number.abs() < 9_223_372_036_854_775_808.0 {
          Ok(Value::Number(Number::from(number as i64)))
        } else {
          Err(validation::Kind::IntParsingSize)
        }
      }
      Some(_) => Err(validation::Kind::IntFromFloat),
      None => Err(validation::Kind::IntType),
    },
    Value::String(text) => validation::parse_int(validation::Source::Query, "body", text, None, None)
      .map(|value| Value::Number(Number::from(value)))
      .map_err(|error| error.kind),
    _ => Err(validation::Kind::IntType),
  }
}

fn boolean_value(value: &Value) -> Result<Value, validation::Kind> {
  match value {
    Value::Bool(_) => Ok(value.clone()),
    Value::String(text) => {
      validation::parse_bool(validation::Source::Query, "body", text).map(Value::Bool).map_err(|error| error.kind)
    }
    Value::Number(number) if number.as_i64() == Some(0) || number.as_u64() == Some(0) => Ok(Value::Bool(false)),
    Value::Number(number) if number.as_i64() == Some(1) || number.as_u64() == Some(1) => Ok(Value::Bool(true)),
    Value::Number(number) if number.as_f64() == Some(0.0) => Ok(Value::Bool(false)),
    Value::Number(number) if number.as_f64() == Some(1.0) => Ok(Value::Bool(true)),
    Value::Number(_) => Err(validation::Kind::BoolParsing),
    _ => Err(validation::Kind::BoolType),
  }
}

fn body_loc() -> Vec<validation::Loc> {
  vec![validation::Loc::Field("body".to_string())]
}

fn child_loc(loc: &[validation::Loc], name: &str) -> Vec<validation::Loc> {
  let mut child = loc.to_vec();
  child.push(validation::Loc::Field(name.to_string()));
  child
}

fn body_missing_error() -> validation::FieldError {
  validation::FieldError { loc: body_loc(), input: Value::Null, kind: validation::Kind::Missing }
}

fn model_attributes_error(input: Value) -> validation::FieldError {
  validation::FieldError { loc: body_loc(), input, kind: validation::Kind::ModelAttributesType }
}

#[cfg(test)]
mod nullable_literal_tests {
  use axum::body::to_bytes;
  use axum::http::StatusCode;
  use serde_json::{Value, json};

  use super::{BodyModel, Field, FieldType, RequestBody};

  const FIELDS: &[Field] = &[Field::required("rating", FieldType::NullableLiteral(&["thumbs_up", "thumbs_down"]))];
  const MODEL: BodyModel = BodyModel::required(FIELDS);

  #[test]
  fn nullable_literal_accepts_null() {
    let value = RequestBody::Json(json!({"rating": null})).validate(&MODEL).unwrap().unwrap();
    assert_eq!(value, json!({"rating": null}));
  }

  #[tokio::test]
  async fn nullable_literal_rejects_other_values_with_literal_error_shape() {
    let response = RequestBody::Json(json!({"rating": "neutral"})).validate(&MODEL).unwrap_err();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["detail"][0]["type"], "literal_error");
    assert_eq!(value["detail"][0]["loc"], json!(["body", "rating"]));
  }
}

fn body_parse_error() -> Response {
  responses::json(StatusCode::BAD_REQUEST, &json!({"detail": "There was an error parsing the body"}))
}

#[cfg(test)]
mod tests {
  use super::*;

  const NESTED_FIELDS: &[Field] = &[Field::optional("goal", FieldType::String)];
  const NESTED: BodyModel = BodyModel::optional(NESTED_FIELDS).forbid_extra();
  const CHOICE_FIELDS: &[Field] = &[
    Field::optional("mode", FieldType::NullableLiteral(&["manager", "worker"])),
    Field::optional("kind", FieldType::NullableEnum(&["implement", "verify"])),
    Field::optional("nested", FieldType::NullableModel { class_name: "Nested", model: &NESTED }),
  ];
  const CHOICES: BodyModel = BodyModel::optional(CHOICE_FIELDS).forbid_extra();

  fn field_error(loc: &[&str], input: Value, kind: validation::Kind) -> validation::FieldError {
    validation::FieldError {
      loc: loc.iter().map(|part| validation::Loc::Field(part.to_string())).collect(),
      input,
      kind,
    }
  }

  fn check(value: Value, model: &BodyModel) -> Vec<validation::FieldError> {
    RequestBody::Json(value).check(model).unwrap_err()
  }

  #[test]
  fn content_type_uses_case_insensitive_main_type_and_subtype() {
    assert!(is_json_content_type("application/json"));
    assert!(is_json_content_type("APPLICATION/JSON; charset=utf-8"));
    assert!(is_json_content_type("Application/Vnd.Api+Json; version=2"));
    assert!(!is_json_content_type("text/json; charset=utf-8"));
    assert!(!is_json_content_type("application/jsonp"));
    assert!(!is_json_content_type("application/json/extra"));
  }

  #[test]
  fn nullable_choices_accept_null_and_refuse_other_values() {
    let accepted = RequestBody::Json(json!({"mode": null, "kind": null})).check(&CHOICES).unwrap();
    assert_eq!(accepted, Some(json!({"mode": null, "kind": null})));
    let errors = check(json!({"mode": "other", "kind": "other"}), &CHOICES);
    assert_eq!(
      errors,
      vec![
        field_error(&["body", "mode"], json!("other"), validation::Kind::Literal(&["manager", "worker"])),
        field_error(&["body", "kind"], json!("other"), validation::Kind::Enum(&["implement", "verify"])),
      ]
    );
  }

  #[test]
  fn forbidden_extra_keys_follow_the_field_errors() {
    let errors = check(json!({"zzz": 1, "mode": 3}), &CHOICES);
    assert_eq!(
      errors,
      vec![
        field_error(&["body", "mode"], json!(3), validation::Kind::Literal(&["manager", "worker"])),
        validation::FieldError {
          loc: vec![validation::Loc::Field("body".to_string()), validation::Loc::Field("zzz".to_string())],
          input: json!(1),
          kind: validation::Kind::ExtraForbidden,
        },
      ]
    );
  }

  #[test]
  fn nested_model_errors_carry_the_nested_location() {
    let errors = check(json!({"nested": {"goal": 1, "zzz": 2}}), &CHOICES);
    assert_eq!(
      errors,
      vec![
        field_error(&["body", "nested", "goal"], json!(1), validation::Kind::StringType),
        validation::FieldError {
          loc: ["body", "nested", "zzz"].iter().map(|part| validation::Loc::Field(part.to_string())).collect(),
          input: json!(2),
          kind: validation::Kind::ExtraForbidden,
        },
      ]
    );
    assert_eq!(
      check(json!({"nested": "text"}), &CHOICES),
      vec![field_error(&["body", "nested"], json!("text"), validation::Kind::ModelType { class_name: "Nested" })]
    );
    assert_eq!(RequestBody::Json(json!({"nested": null})).check(&CHOICES).unwrap(), Some(json!({"nested": null})));
  }

  const CHECKED: BodyModel = BodyModel::optional(CHOICE_FIELDS).checked(refuse_worker_kind);

  fn refuse_worker_kind(values: &Map<String, Value>) -> Option<String> {
    (values.get("mode") == Some(&json!("worker")) && values.get("kind").is_some())
      .then(|| "a worker takes no kind".to_string())
  }

  #[test]
  fn a_model_check_reports_a_value_error_at_the_model_location() {
    let errors = check(json!({"mode": "worker", "kind": "verify"}), &CHECKED);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].loc, vec![validation::Loc::Field("body".to_string())]);
    assert_eq!(errors[0].kind, validation::Kind::ValueError { message: "a worker takes no kind".to_string() });
    assert_eq!(
      RequestBody::Json(json!({"mode": "manager"})).check(&CHECKED).unwrap(),
      Some(json!({"mode": "manager"}))
    );
  }
}
