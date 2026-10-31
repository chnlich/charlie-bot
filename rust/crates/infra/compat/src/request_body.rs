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

//! ## Model options
//!
//! `BodyModel::forbid_extra()` matches pydantic's `extra='forbid'`. A `Field` accepts `ge`, `gt` and `le` bounds
//! for integer and float fields, and `min_length` and `max_length` bounds for string and list fields. Number bounds
//! must use the corresponding `validation::Bound` variant. A bound applies only when an optional field has a value.
//!
//! The error order follows pydantic 2.12.5 (pydantic-core 2.41.5 in the worktree `.venv`): declared fields in
//! declaration order, then forbidden extra keys in input order. A number that violates multiple bounds reports the
//! first violated bound in the order `le`, `ge`, `gt`. Lists report `max_length` before their items and
//! `min_length` after their items.
//!
//! `FieldType::ModelList` declares a `list[M]` field and `FieldType::NullableModelList` a `list[M] | None` field. Each
//! item is validated as the model `M`. FastAPI validates bodies with `from_attributes`, so a non-object item reports
//! `model_attributes_type` at its index, with no context. An item's own errors sit under its index, in the same order
//! as a top-level model's (fields, then extra keys). A value that is not a list reports `list_type` at the field; only
//! the nullable type takes null. The validated list holds each item as a validated object, and a missing optional item
//! field stays absent.
//!
//! `FieldType::NullableTaggedUnionList` declares a `list[A | B | C] | None` field whose items are discriminated on
//! one key, as pydantic's `Field(discriminator=...)` does.
//! Each `TaggedVariant` maps a tag value to its member model, which declares the discriminator as a `Literal` of that
//! tag, so the validated item keeps its tag. A non-object item reports `model_attributes_type` at its index. An object
//! without the discriminator key reports `union_tag_not_found` at its index, and one whose tag matches no variant
//! reports `union_tag_invalid` there, with the tag's Python `str()`. A member's own errors sit under `[index, tag]`: a
//! bad `pid` in a `local_pid` item at index 0 reports `["body", "watch_targets", 0, "local_pid", "pid"]`. The field
//! follows the `ModelList` rules for a non-list value, for null and for the list's bounds.
//!
//! `RequestBody::check` returns the ordered `FieldError`s; `validate` wraps them in FastAPI's 422 response. A missing
//! optional field stays absent from the returned object so the handler applies the Python default.

use std::cmp::Ordering;

use axum::body::{Body, to_bytes};
use axum::response::Response;
use bytes::Bytes;
use charliebot_core::text_repr;
use http::{HeaderMap, StatusCode, header};
use serde_json::{Map, Number, Value, json};

use crate::json_loads::{self, LoadFailure};
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

  match json_loads::loads_classified(&raw) {
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
  NullableFloat,
  Boolean,
  /// A `dict` field whose keys and values are unconstrained.
  Dictionary,
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
  /// A `list[M]` field whose items are each validated as the nested model `M`. `class_name` names `M`, but FastAPI
  /// reports a non-object item as `model_attributes_type`, which names no model, so no error reads it.
  ModelList {
    class_name: &'static str,
    model: &'static BodyModel,
  },
  /// A `list[M] | None` field: null is valid, and a list is validated as `ModelList`.
  NullableModelList {
    model: &'static BodyModel,
  },
  /// A `list[A | B | C] | None` field discriminated on `discriminator`, as pydantic's `Field(discriminator=...)`
  /// does: null is valid, and each item of a list is validated as the variant whose tag it carries.
  NullableTaggedUnionList {
    discriminator: &'static str,
    variants: &'static [TaggedVariant],
  },
}

/// One member of a tagged union: the tag value that selects it, its class name, and the model its fields validate as.
/// `class_name` is read by no error, as with `ModelList`: FastAPI reports a bad item without naming a class.
#[derive(Debug, Clone, Copy)]
pub struct TaggedVariant {
  pub tag: &'static str,
  pub class_name: &'static str,
  pub model: &'static BodyModel,
}

impl FieldType {
  /// Whether a number bound fits the field: an integer field takes `Bound::Int`, a float field `Bound::Float`.
  const fn takes_number_bound(self, bound: validation::Bound) -> bool {
    matches!(
      (self, bound),
      (FieldType::Integer | FieldType::NullableInteger, validation::Bound::Int(_))
        | (FieldType::NullableFloat, validation::Bound::Float(_))
    )
  }

  /// Whether a length bound fits the field: a string or a list.
  const fn takes_length_bound(self) -> bool {
    matches!(
      self,
      FieldType::String
        | FieldType::NullableString
        | FieldType::StringList
        | FieldType::ModelList { .. }
        | FieldType::NullableModelList { .. }
        | FieldType::NullableTaggedUnionList { .. }
    )
  }
}

/// One model field in declaration order.
#[derive(Debug, Clone, Copy)]
pub struct Field {
  name: &'static str,
  field_type: FieldType,
  required: bool,
  bounds: Bounds,
}

/// The bounds a `Field(...)` declares; `None` for each bound the field does not declare.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bounds {
  gt: Option<validation::Bound>,
  ge: Option<validation::Bound>,
  le: Option<validation::Bound>,
  min_length: Option<usize>,
  max_length: Option<usize>,
}

impl Bounds {
  const NONE: Bounds = Bounds { gt: None, ge: None, le: None, min_length: None, max_length: None };
}

impl Field {
  /// Declare a required field.
  pub const fn required(name: &'static str, field_type: FieldType) -> Self {
    Self { name, field_type, required: true, bounds: Bounds::NONE }
  }

  /// Declare a field whose Python model has a default.
  pub const fn optional(name: &'static str, field_type: FieldType) -> Self {
    Self { name, field_type, required: false, bounds: Bounds::NONE }
  }

  /// Declare pydantic's `gt`: the value must be greater than the bound.
  pub const fn gt(self, bound: validation::Bound) -> Self {
    assert!(self.field_type.takes_number_bound(bound), "a number bound must match the field's int or float type");
    Self { bounds: Bounds { gt: Some(bound), ..self.bounds }, ..self }
  }

  /// Declare pydantic's `ge`: the value must be at least the bound.
  pub const fn ge(self, bound: validation::Bound) -> Self {
    assert!(self.field_type.takes_number_bound(bound), "a number bound must match the field's int or float type");
    Self { bounds: Bounds { ge: Some(bound), ..self.bounds }, ..self }
  }

  /// Declare pydantic's `le`: the value must be at most the bound.
  pub const fn le(self, bound: validation::Bound) -> Self {
    assert!(self.field_type.takes_number_bound(bound), "a number bound must match the field's int or float type");
    Self { bounds: Bounds { le: Some(bound), ..self.bounds }, ..self }
  }

  /// Declare pydantic's `min_length`: a string has at least this many characters, a list at least this many items.
  pub const fn min_length(self, min_length: usize) -> Self {
    assert!(self.field_type.takes_length_bound(), "min_length applies to a string or list field");
    Self { bounds: Bounds { min_length: Some(min_length), ..self.bounds }, ..self }
  }

  /// Declare pydantic's `max_length`: a string has at most this many characters, a list at most this many items.
  pub const fn max_length(self, max_length: usize) -> Self {
    assert!(self.field_type.takes_length_bound(), "max_length applies to a string or list field");
    Self { bounds: Bounds { max_length: Some(max_length), ..self.bounds }, ..self }
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
  let validated = match field.field_type {
    FieldType::String if value.is_string() => Ok(value.clone()),
    FieldType::NullableString if value.is_null() || value.is_string() => Ok(value.clone()),
    FieldType::String | FieldType::NullableString => failure(validation::Kind::StringType),
    FieldType::Integer | FieldType::NullableInteger => {
      match integer_value(value, matches!(field.field_type, FieldType::NullableInteger)) {
        Ok(value) => Ok(value),
        Err(kind) => failure(kind),
      }
    }
    FieldType::NullableFloat => match float_value(value, matches!(field.field_type, FieldType::NullableFloat)) {
      Ok(value) => Ok(value),
      Err(kind) => failure(kind),
    },
    FieldType::Boolean => match boolean_value(value) {
      Ok(value) => Ok(value),
      Err(kind) => failure(kind),
    },
    FieldType::Dictionary if value.is_object() => Ok(value.clone()),
    FieldType::Dictionary => failure(validation::Kind::DictType),
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
    FieldType::StringList => list_field(field, value, loc, string_item),
    FieldType::ModelList { model, .. } => model_list(field, value, loc, model),
    FieldType::NullableModelList { .. } if value.is_null() => Ok(Value::Null),
    FieldType::NullableModelList { model, .. } => model_list(field, value, loc, model),
    FieldType::NullableTaggedUnionList { .. } if value.is_null() => Ok(Value::Null),
    FieldType::NullableTaggedUnionList { discriminator, variants } => {
      tagged_union_list(field, value, loc, discriminator, variants)
    }
    FieldType::NullableModel { class_name, model } => match value {
      Value::Null => Ok(Value::Null),
      Value::Object(object) => validate_object(object, model, loc),
      _ => failure(validation::Kind::ModelType { class_name }),
    },
  };
  validated.and_then(|checked| match field.bounds.violation(&checked) {
    Some(kind) => failure(kind),
    None => Ok(checked),
  })
}

/// A `list[M]` field: each item is an object validated as `model`. A non-object item reports `model_attributes_type`
/// at its index, as FastAPI's body validation does.
fn model_list(
  field: &Field,
  value: &Value,
  loc: &[validation::Loc],
  model: &BodyModel,
) -> Result<Value, Vec<validation::FieldError>> {
  list_field(field, value, loc, |item, item_loc| match item {
    Value::Object(object) => validate_object(object, model, item_loc),
    _ => Err(vec![validation::FieldError {
      loc: item_loc.to_vec(),
      input: item.clone(),
      kind: validation::Kind::ModelAttributesType,
    }]),
  })
}

/// A `list[A | B | C]` field: each item is validated by the variant its discriminator names.
fn tagged_union_list(
  field: &Field,
  value: &Value,
  loc: &[validation::Loc],
  discriminator: &'static str,
  variants: &'static [TaggedVariant],
) -> Result<Value, Vec<validation::FieldError>> {
  list_field(field, value, loc, |item, item_loc| tagged_union_item(item, item_loc, discriminator, variants))
}

/// A tagged-union item. A non-object item reports `model_attributes_type`. An object without the discriminator key, or
/// with a tag that names no variant, reports its tag error at the item. A tag that names a variant validates the object
/// as that variant's model, with its errors under `[index, tag]`.
fn tagged_union_item(
  item: &Value,
  loc: &[validation::Loc],
  discriminator: &'static str,
  variants: &'static [TaggedVariant],
) -> Result<Value, Vec<validation::FieldError>> {
  let failure = |kind| Err(vec![validation::FieldError { loc: loc.to_vec(), input: item.clone(), kind }]);
  let Value::Object(object) = item else {
    return failure(validation::Kind::ModelAttributesType);
  };
  let Some(tag) = object.get(discriminator) else {
    return failure(validation::Kind::UnionTagNotFound { discriminator });
  };
  match tag.as_str().and_then(|text| variants.iter().find(|variant| variant.tag == text)) {
    Some(variant) => {
      let mut variant_loc = loc.to_vec();
      variant_loc.push(validation::Loc::Field(variant.tag.to_string()));
      validate_object(object, variant.model, &variant_loc)
    }
    None => failure(validation::Kind::UnionTagInvalid {
      discriminator,
      tag: text_repr::value_str(tag),
      expected_tags: variants.iter().map(|variant| variant.tag).collect(),
    }),
  }
}

/// A `list[str]` item: a string, or a `string_type` error at the item's index.
fn string_item(item: &Value, loc: &[validation::Loc]) -> Result<Value, Vec<validation::FieldError>> {
  if item.is_string() {
    return Ok(item.clone());
  }
  Err(vec![validation::FieldError { loc: loc.to_vec(), input: item.clone(), kind: validation::Kind::StringType }])
}

/// A list field. pydantic checks `max_length` before the items and `min_length` after them, and a list with an
/// invalid item reports only the items' errors. `item` validates each item at its index; the list holds the items'
/// validated values in order.
fn list_field(
  field: &Field,
  value: &Value,
  loc: &[validation::Loc],
  item: impl Fn(&Value, &[validation::Loc]) -> Result<Value, Vec<validation::FieldError>>,
) -> Result<Value, Vec<validation::FieldError>> {
  let failure = |kind| Err(vec![validation::FieldError { loc: loc.to_vec(), input: value.clone(), kind }]);
  let Some(items) = value.as_array() else {
    return failure(validation::Kind::ListType);
  };
  if let Some(max_length) = field.bounds.max_length
    && items.len() > max_length
  {
    return failure(validation::Kind::ListTooLong { max_length, actual_length: items.len() });
  }
  let mut values = Vec::with_capacity(items.len());
  let mut errors = Vec::new();
  for (index, child) in items.iter().enumerate() {
    let mut item_loc = loc.to_vec();
    item_loc.push(validation::Loc::Index(index));
    match item(child, &item_loc) {
      Ok(item_value) => values.push(item_value),
      Err(mut item_errors) => errors.append(&mut item_errors),
    }
  }
  if !errors.is_empty() {
    return Err(errors);
  }
  if let Some(min_length) = field.bounds.min_length
    && items.len() < min_length
  {
    return failure(validation::Kind::ListTooShort { min_length, actual_length: items.len() });
  }
  Ok(Value::Array(values))
}

impl Bounds {
  /// The first bound that a scalar value violates, or `None` when it passes. A null passes every bound.
  ///
  /// A number is compared in its bound's type. Pydantic reports `le`, then `ge` and `gt`.
  fn violation(&self, value: &Value) -> Option<validation::Kind> {
    match value {
      Value::String(text) => {
        let length = text.chars().count();
        if let Some(min_length) = self.min_length
          && length < min_length
        {
          return Some(validation::Kind::StringTooShort { min_length });
        }
        if let Some(max_length) = self.max_length
          && length > max_length
        {
          return Some(validation::Kind::StringTooLong { max_length });
        }
        None
      }
      Value::Number(number) => {
        if let Some(bound) = self.le
          && order(number, bound) == Ordering::Greater
        {
          return Some(validation::Kind::LessThanEqual(bound));
        }
        if let Some(bound) = self.ge
          && order(number, bound) == Ordering::Less
        {
          return Some(validation::Kind::GreaterThanEqual(bound));
        }
        if let Some(bound) = self.gt
          && order(number, bound) != Ordering::Greater
        {
          return Some(validation::Kind::GreaterThan(bound));
        }
        None
      }
      _ => None,
    }
  }
}

/// How a validated number compares with a bound. An integer field compares exactly, as i128. A float field compares
/// as f64, where `-0.0` equals `0.0`, as in pydantic.
fn order(number: &Number, bound: validation::Bound) -> Ordering {
  match bound {
    validation::Bound::Int(bound) => {
      let value = number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))
        .expect("an integer field holds an integer");
      value.cmp(&i128::from(bound))
    }
    validation::Bound::Float(bound) => {
      number.as_f64().expect("a JSON number reads as f64").partial_cmp(&bound).expect("a finite number compares")
    }
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

/// A float field's value, read as pydantic's lax mode reads it: a number, a boolean or a numeric string.
fn float_value(value: &Value, nullable: bool) -> Result<Value, validation::Kind> {
  if value.is_null() && nullable {
    return Ok(Value::Null);
  }
  let number = match value {
    Value::Bool(value) => {
      if *value {
        1.0
      } else {
        0.0
      }
    }
    Value::Number(number) => number.as_f64().expect("a JSON number reads as f64"),
    Value::String(text) => {
      validation::parse_float(validation::Source::Query, "body", text).map_err(|error| error.kind)?
    }
    _ => return Err(validation::Kind::FloatType),
  };
  Ok(Value::Number(Number::from_f64(number).expect("a float field's number is finite")))
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
