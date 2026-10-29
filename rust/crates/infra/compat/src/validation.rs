//! FastAPI's request-validation errors and the 422 bodies they render.
//!
//! A request whose parameters or body fail validation gets status 422 and `{"detail": [...]}`, one entry
//! per failed value in declaration order. An entry names its `type`, its `loc` (the request part, the field,
//! and a list index where one applies), pydantic's `msg` and the `input` that failed. Some types add a `ctx`
//! with the bound or the choices. [`Kind`] lists the pydantic error types the routes produce with
//! their texts; [`FieldError`] is one entry. The `parse_*` functions read a scalar query or path value the
//! way pydantic's lax mode does and return the entry for a value that it rejects.
//!
//! The texts follow pydantic 2.12.5 and FastAPI 0.135.2 as the deleted Python environment pinned them.
//! `rust/fixtures/python/validation_422.json`
//! holds the bodies the Python app answers for these inputs, and the fixture test compares them.
//!
//! Parsing a JSON body is not here: pydantic reads the body while it validates it. The JSON decoder's
//! messages (`json_invalid`) come from CPython's `json` module, which these kinds only carry.

use axum::response::Response;
use charliebot_core::pyfmt;
use http::StatusCode;
use serde_json::{Map, Value, json};

use crate::json;
use crate::responses;

/// One step of an error's `loc`: a field name, or a list index (`["body", "tags", 0]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loc {
  Field(String),
  Index(usize),
}

/// A bound of `gt`, `ge`, `lt` or `le`. pydantic keeps the bound's own type: an integer prints as `1`, a float
/// as `1.0` in `ctx`, and the `msg` prints the number as Rust's `Display` writes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Bound {
  Int(i64),
  Float(f64),
}

impl Bound {
  fn text(self) -> String {
    match self {
      Bound::Int(bound) => bound.to_string(),
      Bound::Float(bound) => bound.to_string(),
    }
  }

  fn value(self) -> Value {
    match self {
      Bound::Int(bound) => Value::from(bound),
      Bound::Float(bound) => Value::from(bound),
    }
  }
}

/// A pydantic error type, with the arguments its `msg` and `ctx` print.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
  /// `missing`: a required field or parameter is absent.
  Missing,
  StringType,
  StringTooShort {
    min_length: usize,
  },
  StringTooLong {
    max_length: usize,
  },
  /// `too_short` on a list: fewer items than `min_length`. pydantic reports it after the items validate.
  ListTooShort {
    min_length: usize,
    actual_length: usize,
  },
  /// `too_long` on a list: more items than `max_length`. pydantic reports it before the items validate.
  ListTooLong {
    max_length: usize,
    actual_length: usize,
  },
  IntType,
  IntParsing,
  IntFromFloat,
  /// `int_parsing_size`: a float outside the range of a 64-bit signed integer.
  IntParsingSize,
  FloatType,
  FloatParsing,
  BoolType,
  BoolParsing,
  ListType,
  DictType,
  ModelAttributesType,
  /// `model_type`: a nested model field received a value that is not an object.
  ModelType {
    class_name: &'static str,
  },
  /// `literal_error`: the value is not one of a `Literal[...]` of strings.
  Literal(&'static [&'static str]),
  /// `enum`: the value is not one of an `Enum` class's values.
  Enum(&'static [&'static str]),
  GreaterThan(Bound),
  GreaterThanEqual(Bound),
  LessThan(Bound),
  LessThanEqual(Bound),
  ExtraForbidden,
  /// `json_invalid`: the body is not JSON. Its `loc` ends with the decoder's character offset, and `error` is
  /// the decoder's message.
  JsonInvalid {
    error: String,
  },
  /// `value_error`: a validator raised `ValueError(message)`.
  ValueError {
    message: String,
  },
  /// `union_tag_not_found`: a tagged-union item has no value under its discriminator key.
  UnionTagNotFound {
    discriminator: &'static str,
  },
  /// `union_tag_invalid`: the item's discriminator value names no variant. `tag` is the value's Python `str()`, and
  /// `expected_tags` lists the union's tags in declaration order.
  UnionTagInvalid {
    discriminator: &'static str,
    tag: String,
    expected_tags: Vec<&'static str>,
  },
}

impl Kind {
  /// pydantic's error `type`.
  fn type_name(&self) -> &'static str {
    match self {
      Kind::Missing => "missing",
      Kind::StringType => "string_type",
      Kind::StringTooShort { .. } => "string_too_short",
      Kind::StringTooLong { .. } => "string_too_long",
      Kind::ListTooShort { .. } => "too_short",
      Kind::ListTooLong { .. } => "too_long",
      Kind::IntType => "int_type",
      Kind::IntParsing => "int_parsing",
      Kind::IntFromFloat => "int_from_float",
      Kind::IntParsingSize => "int_parsing_size",
      Kind::FloatType => "float_type",
      Kind::FloatParsing => "float_parsing",
      Kind::BoolType => "bool_type",
      Kind::BoolParsing => "bool_parsing",
      Kind::ListType => "list_type",
      Kind::DictType => "dict_type",
      Kind::ModelAttributesType => "model_attributes_type",
      Kind::ModelType { .. } => "model_type",
      Kind::Literal(_) => "literal_error",
      Kind::Enum(_) => "enum",
      Kind::GreaterThan(_) => "greater_than",
      Kind::GreaterThanEqual(_) => "greater_than_equal",
      Kind::LessThan(_) => "less_than",
      Kind::LessThanEqual(_) => "less_than_equal",
      Kind::ExtraForbidden => "extra_forbidden",
      Kind::JsonInvalid { .. } => "json_invalid",
      Kind::ValueError { .. } => "value_error",
      Kind::UnionTagNotFound { .. } => "union_tag_not_found",
      Kind::UnionTagInvalid { .. } => "union_tag_invalid",
    }
  }

  /// pydantic's error `msg`.
  fn msg(&self) -> String {
    match self {
      Kind::Missing => "Field required".to_string(),
      Kind::StringType => "Input should be a valid string".to_string(),
      Kind::StringTooShort { min_length } => {
        format!("String should have at least {min_length} {}", characters(*min_length))
      }
      Kind::StringTooLong { max_length } => {
        format!("String should have at most {max_length} {}", characters(*max_length))
      }
      Kind::ListTooShort { min_length, actual_length } => {
        format!("List should have at least {min_length} {} after validation, not {actual_length}", items(*min_length))
      }
      Kind::ListTooLong { max_length, actual_length } => {
        format!("List should have at most {max_length} {} after validation, not {actual_length}", items(*max_length))
      }
      Kind::IntType => "Input should be a valid integer".to_string(),
      Kind::IntParsing => "Input should be a valid integer, unable to parse string as an integer".to_string(),
      Kind::IntFromFloat => "Input should be a valid integer, got a number with a fractional part".to_string(),
      Kind::IntParsingSize => "Unable to parse input string as an integer, exceeded maximum size".to_string(),
      Kind::FloatType => "Input should be a valid number".to_string(),
      Kind::FloatParsing => "Input should be a valid number, unable to parse string as a number".to_string(),
      Kind::BoolType => "Input should be a valid boolean".to_string(),
      Kind::BoolParsing => "Input should be a valid boolean, unable to interpret input".to_string(),
      Kind::ListType => "Input should be a valid list".to_string(),
      Kind::DictType => "Input should be a valid dictionary".to_string(),
      Kind::ModelAttributesType => "Input should be a valid dictionary or object to extract fields from".to_string(),
      Kind::ModelType { class_name } => format!("Input should be a valid dictionary or instance of {class_name}"),
      Kind::Literal(choices) | Kind::Enum(choices) => format!("Input should be {}", one_of(choices)),
      Kind::GreaterThan(bound) => format!("Input should be greater than {}", bound.text()),
      Kind::GreaterThanEqual(bound) => format!("Input should be greater than or equal to {}", bound.text()),
      Kind::LessThan(bound) => format!("Input should be less than {}", bound.text()),
      Kind::LessThanEqual(bound) => format!("Input should be less than or equal to {}", bound.text()),
      Kind::ExtraForbidden => "Extra inputs are not permitted".to_string(),
      Kind::JsonInvalid { .. } => "JSON decode error".to_string(),
      Kind::ValueError { message } => format!("Value error, {message}"),
      Kind::UnionTagNotFound { discriminator } => {
        format!("Unable to extract tag using discriminator {}", pyfmt::str_repr(discriminator))
      }
      Kind::UnionTagInvalid { discriminator, tag, expected_tags } => format!(
        "Input tag '{tag}' found using {} does not match any of the expected tags: {}",
        pyfmt::str_repr(discriminator),
        quoted_list(expected_tags)
      ),
    }
  }

  /// pydantic's error `ctx`, for the types that carry one.
  fn ctx(&self) -> Option<Value> {
    match self {
      Kind::StringTooShort { min_length } => Some(json!({"min_length": min_length})),
      Kind::StringTooLong { max_length } => Some(json!({"max_length": max_length})),
      Kind::ListTooShort { min_length, actual_length } => {
        Some(json!({"field_type": "List", "min_length": min_length, "actual_length": actual_length}))
      }
      Kind::ListTooLong { max_length, actual_length } => {
        Some(json!({"field_type": "List", "max_length": max_length, "actual_length": actual_length}))
      }
      Kind::Literal(choices) | Kind::Enum(choices) => Some(json!({"expected": one_of(choices)})),
      Kind::GreaterThan(bound) => Some(json!({"gt": bound.value()})),
      Kind::GreaterThanEqual(bound) => Some(json!({"ge": bound.value()})),
      Kind::LessThan(bound) => Some(json!({"lt": bound.value()})),
      Kind::LessThanEqual(bound) => Some(json!({"le": bound.value()})),
      Kind::JsonInvalid { error } => Some(json!({"error": error})),
      Kind::ModelType { class_name } => Some(json!({"class_name": class_name})),
      // FastAPI's jsonable_encoder writes the ValueError object as an empty mapping (FastAPI 0.135.2).
      Kind::ValueError { .. } => Some(json!({"error": {}})),
      Kind::UnionTagNotFound { discriminator } => Some(json!({"discriminator": pyfmt::str_repr(discriminator)})),
      Kind::UnionTagInvalid { discriminator, tag, expected_tags } => Some(json!({
        "discriminator": pyfmt::str_repr(discriminator),
        "tag": tag,
        "expected_tags": quoted_list(expected_tags),
      })),
      _ => None,
    }
  }
}

fn characters(count: usize) -> &'static str {
  if count == 1 { "character" } else { "characters" }
}

fn items(count: usize) -> &'static str {
  if count == 1 { "item" } else { "items" }
}

/// pydantic's expected-tags text: each tag repr-quoted, joined by `, ` with no `or`.
fn quoted_list(choices: &[&str]) -> String {
  choices.iter().map(|choice| pyfmt::str_repr(choice)).collect::<Vec<_>>().join(", ")
}

/// pydantic's expected-choices text: `'a'`, `'a' or 'b'`, `'a', 'b' or 'c'`.
fn one_of(choices: &[&str]) -> String {
  let quoted: Vec<String> = choices.iter().map(|choice| pyfmt::str_repr(choice)).collect();
  match quoted.split_last() {
    None => String::new(),
    Some((last, [])) => last.clone(),
    Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
  }
}

/// One failed value: where it came from, what was received, and what check it failed.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldError {
  pub loc: Vec<Loc>,
  pub input: Value,
  pub kind: Kind,
}

impl FieldError {
  fn entry(&self) -> Value {
    let mut entry = Map::new();
    entry.insert("type".to_string(), json!(self.kind.type_name()));
    entry.insert("loc".to_string(), Value::Array(self.loc.iter().map(loc_value).collect()));
    entry.insert("msg".to_string(), json!(self.kind.msg()));
    entry.insert("input".to_string(), self.input.clone());
    if let Some(ctx) = self.kind.ctx() {
      entry.insert("ctx".to_string(), ctx);
    }
    Value::Object(entry)
  }
}

fn loc_value(loc: &Loc) -> Value {
  match loc {
    Loc::Field(name) => json!(name),
    Loc::Index(index) => json!(index),
  }
}

/// The 422 body: `{"detail": [...]}` with one entry per error.
pub fn detail(errors: &[FieldError]) -> Value {
  json!({"detail": errors.iter().map(FieldError::entry).collect::<Vec<Value>>()})
}

/// The 422 body's bytes: FastAPI answers with a starlette `JSONResponse`.
pub fn detail_bytes(errors: &[FieldError]) -> Vec<u8> {
  json::starlette_bytes(&detail(errors))
}

/// FastAPI's answer to a request with `errors`: status 422 and the `detail` body.
pub fn response(errors: &[FieldError]) -> Response {
  responses::json(StatusCode::UNPROCESSABLE_ENTITY, &detail(errors))
}

/// Where a parameter's text comes from; it is the first step of the parameter's `loc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
  Path,
  Query,
}

impl Source {
  fn loc(self, name: &str) -> Vec<Loc> {
    let part = match self {
      Source::Path => "path",
      Source::Query => "query",
    };
    vec![Loc::Field(part.to_string()), Loc::Field(name.to_string())]
  }
}

fn failure(source: Source, name: &str, text: &str, kind: Kind) -> Box<FieldError> {
  Box::new(FieldError { loc: source.loc(name), input: Value::String(text.to_string()), kind })
}

/// The entry for a required parameter that the request does not carry. Its `input` is `null`.
pub fn missing(source: Source, name: &str) -> FieldError {
  FieldError { loc: source.loc(name), input: Value::Null, kind: Kind::Missing }
}

/// An `int` parameter's text, read as pydantic's lax mode reads a string, then checked against `ge` and `le`.
///
/// A value beyond the i64 range that violates a bound reports that bound. A value beyond i64 that violates
/// neither has no i64 to return, so it fails as `int_parsing`; no route parameter needs more than i64.
pub fn parse_int(
  source: Source,
  name: &str,
  text: &str,
  ge: Option<i64>,
  le: Option<i64>,
) -> Result<i64, Box<FieldError>> {
  let value = lax_int(text).ok_or_else(|| failure(source, name, text, Kind::IntParsing))?;
  if let Some(bound) = ge.filter(|bound| value < i128::from(*bound)) {
    return Err(failure(source, name, text, Kind::GreaterThanEqual(Bound::Int(bound))));
  }
  if let Some(bound) = le.filter(|bound| value > i128::from(*bound)) {
    return Err(failure(source, name, text, Kind::LessThanEqual(Bound::Int(bound))));
  }
  i64::try_from(value).map_err(|_| failure(source, name, text, Kind::IntParsing))
}

/// pydantic's lax string-to-int: an optional sign, digits with single underscores between them, and an
/// optional fraction of plain zeros (`"1.0"` reads as 1, `"1.0_0"` does not). Surrounding whitespace is
/// ignored. Digits beyond i128 saturate; the caller compares them with an i64 bound, so the comparison stays right.
fn lax_int(text: &str) -> Option<i128> {
  let text = text.trim();
  let (sign, unsigned) = match text.strip_prefix('-') {
    Some(rest) => (-1, rest),
    None => (1, text.strip_prefix('+').unwrap_or(text)),
  };
  let (whole, fraction) = match unsigned.split_once('.') {
    Some((whole, fraction)) => (whole, Some(fraction)),
    None => (unsigned, None),
  };
  let digits = digit_groups(whole)?;
  if let Some(fraction) = fraction
    && (fraction.is_empty() || fraction.bytes().any(|digit| digit != b'0'))
  {
    return None;
  }
  let magnitude =
    digits.bytes().fold(0i128, |value, digit| value.saturating_mul(10).saturating_add(i128::from(digit - b'0')));
  Some(sign * magnitude)
}

/// The digits of `text` with its underscores removed, when `text` is digit groups joined by single underscores.
fn digit_groups(text: &str) -> Option<String> {
  let bytes = text.as_bytes();
  let valid = match (bytes.first(), bytes.last()) {
    (Some(first), Some(last)) => {
      first.is_ascii_digit()
        && last.is_ascii_digit()
        && !text.contains("__")
        && bytes.iter().all(|byte| byte.is_ascii_digit() || *byte == b'_')
    }
    _ => false,
  };
  valid.then(|| text.replace('_', ""))
}

/// A `bool` parameter's text, read as pydantic's lax mode reads it: a case-insensitive word or digit, with no
/// surrounding whitespace.
pub fn parse_bool(source: Source, name: &str, text: &str) -> Result<bool, Box<FieldError>> {
  match text.to_ascii_lowercase().as_str() {
    "1" | "on" | "t" | "true" | "y" | "yes" => Ok(true),
    "0" | "off" | "f" | "false" | "n" | "no" => Ok(false),
    _ => Err(failure(source, name, text, Kind::BoolParsing)),
  }
}

/// A `float` value's text, read as pydantic's lax mode reads a string: surrounding whitespace is ignored and an
/// underscore may join two digits. A non-finite text (`nan`, `inf`) fails as `float_parsing`, because the port
/// refuses non-finite floats (`rust/docs/PORT_STATUS.md`).
pub fn parse_float(source: Source, name: &str, text: &str) -> Result<f64, Box<FieldError>> {
  let parsed = if underscores_join_digits(text) { text.trim().replace('_', "").parse::<f64>().ok() } else { None };
  parsed.filter(|value| value.is_finite()).ok_or_else(|| failure(source, name, text, Kind::FloatParsing))
}

/// Whether each underscore in `text` sits between two ASCII digits, the places Python's `float` accepts one.
fn underscores_join_digits(text: &str) -> bool {
  let bytes = text.as_bytes();
  bytes.iter().enumerate().all(|(index, byte)| {
    *byte != b'_'
      || (index > 0 && bytes[index - 1].is_ascii_digit() && bytes.get(index + 1).is_some_and(u8::is_ascii_digit))
  })
}

/// A `Literal[...]` string parameter: the text must equal one of `choices`.
pub fn parse_literal(
  source: Source,
  name: &str,
  text: &str,
  choices: &'static [&'static str],
) -> Result<&'static str, Box<FieldError>> {
  choices
    .iter()
    .copied()
    .find(|choice| *choice == text)
    .ok_or_else(|| failure(source, name, text, Kind::Literal(choices)))
}

/// An `Enum` parameter: the text must equal one of the enum's `choices` (its values).
pub fn parse_enum(
  source: Source,
  name: &str,
  text: &str,
  choices: &'static [&'static str],
) -> Result<&'static str, Box<FieldError>> {
  choices.iter().copied().find(|choice| *choice == text).ok_or_else(|| failure(source, name, text, Kind::Enum(choices)))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn lax_int_reads_what_pydantic_reads() {
    let accepted = [(" 12 ", 12), ("+5", 5), ("1_000", 1000), ("1.0", 1), ("-0", 0), ("1_0.0", 10), ("00012", 12)];
    for (text, value) in accepted {
      assert_eq!(lax_int(text), Some(value), "{text:?}");
    }
    for text in ["1.5", "1.", ".5", "1e3", "1.0_0", "0x10", "1__0", "_1", "", "-", "\u{661}\u{662}"] {
      assert_eq!(lax_int(text), None, "{text:?}");
    }
  }

  #[test]
  fn lax_float_reads_what_pydantic_reads() {
    let accepted = [(" 0.5 ", 0.5), ("1_0.5", 10.5), ("+2", 2.0), ("1.", 1.0), (".5", 0.5), ("1e3", 1000.0)];
    for (text, value) in accepted {
      assert_eq!(parse_float(Source::Query, "x", text).ok(), Some(value), "{text:?}");
    }
    for text in ["abc", "", "nan", "inf", "1__0", "_1", "1_", "1_.5", "1._5"] {
      assert!(parse_float(Source::Query, "x", text).is_err(), "{text:?}");
    }
  }

  #[test]
  fn choices_print_as_pydantic_joins_them() {
    assert_eq!(one_of(&["a"]), "'a'");
    assert_eq!(one_of(&["a", "b"]), "'a' or 'b'");
    assert_eq!(one_of(&["a", "b", "c"]), "'a', 'b' or 'c'");
  }
}
