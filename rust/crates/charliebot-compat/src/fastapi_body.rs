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

/// A field type supported by the request models in the Python API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
  String,
  NullableString,
  Integer,
  NullableInteger,
  Boolean,
  Literal(&'static [&'static str]),
  StringList,
}

/// One model field in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// A request model's fields and body-parameter requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyModel {
  fields: &'static [Field],
  requirement: BodyRequirement,
}

impl BodyModel {
  /// Declare a required model body.
  pub const fn required(fields: &'static [Field]) -> Self {
    Self { fields, requirement: BodyRequirement::Required }
  }

  /// Declare an optional model body.
  pub const fn optional(fields: &'static [Field]) -> Self {
    Self { fields, requirement: BodyRequirement::Optional }
  }
}

impl RequestBody {
  /// Validate this body against a model and return its declared fields.
  pub fn validate(&self, model: &BodyModel) -> Result<Option<Value>, Box<Response>> {
    match self {
      // FastAPI passes a JSON `null` body as None, which a required model reports as missing.
      RequestBody::Absent | RequestBody::Json(Value::Null) => match model.requirement {
        BodyRequirement::Required => Err(Box::new(validation::response(&[body_missing_error()]))),
        BodyRequirement::Optional => Ok(None),
      },
      RequestBody::Json(Value::Object(object)) => validate_object(object, model).map(Some),
      RequestBody::Json(value) => Err(model_attributes_error(value.clone())),
      RequestBody::Raw(bytes) => {
        let input = Value::String(String::from_utf8_lossy(bytes).into_owned());
        Err(model_attributes_error(input))
      }
    }
  }
}

fn validate_object(object: &Map<String, Value>, model: &BodyModel) -> Result<Value, Box<Response>> {
  let mut values = Map::new();
  let mut errors = Vec::new();
  for field in model.fields {
    match object.get(field.name) {
      Some(value) => match validate_field(field, value) {
        Ok(value) => {
          values.insert(field.name.to_string(), value);
        }
        Err(mut field_errors) => errors.append(&mut field_errors),
      },
      None if field.required => errors.push(field_error(field.name, None, Value::Null, validation::Kind::Missing)),
      None => {}
    }
  }
  if errors.is_empty() { Ok(Value::Object(values)) } else { Err(Box::new(validation::response(&errors))) }
}

fn validate_field(field: &Field, value: &Value) -> Result<Value, Vec<validation::FieldError>> {
  let failure = |kind| Err(vec![field_error(field.name, None, value.clone(), kind)]);
  match field.field_type {
    FieldType::String if value.is_string() => Ok(value.clone()),
    FieldType::NullableString if value.is_null() || value.is_string() => Ok(value.clone()),
    FieldType::String | FieldType::NullableString => failure(validation::Kind::StringType),
    FieldType::Integer | FieldType::NullableInteger => {
      match integer_value(value, field.field_type == FieldType::NullableInteger) {
        Ok(value) => Ok(value),
        Err(kind) => failure(kind),
      }
    }
    FieldType::Boolean => match boolean_value(value) {
      Ok(value) => Ok(value),
      Err(kind) => failure(kind),
    },
    FieldType::Literal(choices) => match value.as_str() {
      Some(value) if choices.contains(&value) => Ok(value.into()),
      _ => failure(validation::Kind::Literal(choices)),
    },
    FieldType::StringList => match value.as_array() {
      Some(items) => {
        let mut errors = Vec::new();
        for (index, item) in items.iter().enumerate() {
          if !item.is_string() {
            errors.push(field_error(field.name, Some(index), item.clone(), validation::Kind::StringType));
          }
        }
        if errors.is_empty() { Ok(value.clone()) } else { Err(errors) }
      }
      None => failure(validation::Kind::ListType),
    },
  }
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

fn field_error(field: &str, index: Option<usize>, input: Value, kind: validation::Kind) -> validation::FieldError {
  let mut loc = vec![validation::Loc::Field("body".to_string()), validation::Loc::Field(field.to_string())];
  if let Some(index) = index {
    loc.push(validation::Loc::Index(index));
  }
  validation::FieldError { loc, input, kind }
}

fn body_missing_error() -> validation::FieldError {
  validation::FieldError {
    loc: vec![validation::Loc::Field("body".to_string())],
    input: Value::Null,
    kind: validation::Kind::Missing,
  }
}

fn model_attributes_error(input: Value) -> Box<Response> {
  Box::new(validation::response(&[validation::FieldError {
    loc: vec![validation::Loc::Field("body".to_string())],
    input,
    kind: validation::Kind::ModelAttributesType,
  }]))
}

fn body_parse_error() -> Response {
  responses::json(StatusCode::BAD_REQUEST, &json!({"detail": "There was an error parsing the body"}))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn content_type_uses_case_insensitive_main_type_and_subtype() {
    assert!(is_json_content_type("application/json"));
    assert!(is_json_content_type("APPLICATION/JSON; charset=utf-8"));
    assert!(is_json_content_type("Application/Vnd.Api+Json; version=2"));
    assert!(!is_json_content_type("text/json; charset=utf-8"));
    assert!(!is_json_content_type("application/jsonp"));
    assert!(!is_json_content_type("application/json/extra"));
  }
}
