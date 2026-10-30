//! The 422 bodies against the bodies FastAPI answers for the same requests
//! (`rust/fixtures/golden/validation_422_golden.json`).
//!
//! A query or path case replays its URL through the parsers, with the parameter declarations the fixture
//! lists for its endpoint, and compares the body bytes. A body case rebuilds the errors its JSON body
//! produces for the body model, and compares the same way. Each case's status must match too.

use std::collections::HashMap;

use charliebot_compat::validation::{self, Bound, FieldError, Kind, Loc, Source};
use serde_json::{Value, json};

const FIXTURE: &str = include_str!("../../../../fixtures/golden/validation_422_golden.json");

/// `Literal["thumbs_up", "thumbs_down"]` of the body model.
const THUMBS: &[&str] = &["thumbs_up", "thumbs_down"];

fn fixture() -> Value {
  serde_json::from_str(FIXTURE).expect("the fixture is JSON")
}

fn decode(text: &str) -> String {
  percent_encoding::percent_decode_str(&text.replace('+', " "))
    .decode_utf8()
    .expect("the fixture's values are UTF-8")
    .into_owned()
}

/// The query parameters of `query` as Starlette reads them: each name's last value wins.
fn query_values(query: &str) -> HashMap<String, String> {
  query
    .split('&')
    .filter(|pair| !pair.is_empty())
    .map(|pair| {
      let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
      (decode(name), decode(value))
    })
    .collect()
}

/// The text the URL's path carries for the template's `{name}` segment.
fn path_value(template: &str, url_path: &str, name: &str) -> String {
  let placeholder = format!("{{{name}}}");
  let index = template.split('/').position(|segment| segment == placeholder).expect("the template names the parameter");
  decode(url_path.split('/').nth(index).expect("the URL has the template's segments"))
}

/// The choices as the parsers take them: `'static` strings, leaked for the length of the test process.
fn choices(param: &Value) -> &'static [&'static str] {
  let names: Vec<&'static str> = param["choices"]
    .as_array()
    .expect("a choice parameter lists its choices")
    .iter()
    .map(|choice| &*Box::leak(choice.as_str().expect("choices are strings").to_string().into_boxed_str()))
    .collect();
  Vec::leak(names)
}

/// The errors the endpoint's declared parameters produce for `url`, in declaration order.
fn parameter_errors(template: &str, params: &[Value], url: &str) -> Vec<FieldError> {
  let (url_path, query) = url.split_once('?').unwrap_or((url, ""));
  let values = query_values(query);
  let mut errors = Vec::new();
  for param in params {
    let name = param["name"].as_str().expect("a parameter has a name");
    let source = match param["source"].as_str().expect("a parameter has a source") {
      "query" => Source::Query,
      "path" => Source::Path,
      other => panic!("unknown source {other}"),
    };
    let text = match source {
      Source::Query => values.get(name).cloned(),
      Source::Path => Some(path_value(template, url_path, name)),
    };
    let Some(text) = text else {
      if param["required"] == json!(true) {
        errors.push(validation::missing(source, name));
      }
      continue;
    };
    let outcome: Result<(), Box<FieldError>> = match param["type"].as_str().expect("a parameter has a type") {
      "int" => validation::parse_int(source, name, &text, param["ge"].as_i64(), param["le"].as_i64()).map(drop),
      "bool" => validation::parse_bool(source, name, &text).map(drop),
      "literal" => validation::parse_literal(source, name, &text, choices(param)).map(drop),
      "enum" => validation::parse_enum(source, name, &text, choices(param)).map(drop),
      "str" => Ok(()),
      other => panic!("unknown parameter type {other}"),
    };
    errors.extend(outcome.err().map(|error| *error));
  }
  errors
}

fn at(parts: &[&str]) -> Vec<Loc> {
  std::iter::once(Loc::Field("body".to_string()))
    .chain(parts.iter().map(|part| match part.parse::<usize>() {
      Ok(index) => Loc::Index(index),
      Err(_) => Loc::Field(part.to_string()),
    }))
    .collect()
}

fn error(parts: &[&str], input: Value, kind: Kind) -> FieldError {
  FieldError { loc: at(parts), input, kind }
}

/// The errors that the body model `Item` reports for the body case `name`, in field order.
fn body_errors(name: &str) -> Vec<FieldError> {
  match name {
    "missing_fields" => vec![error(&["name"], json!({}), Kind::Missing), error(&["count"], json!({}), Kind::Missing)],
    "wrong_types" => vec![
      error(&["name"], json!(5), Kind::StringType),
      error(&["count"], json!("x"), Kind::IntParsing),
      error(&["ratio"], json!(0), Kind::GreaterThan(Bound::Float(0.0))),
      error(&["flag"], json!("maybe"), Kind::BoolParsing),
      error(&["tags"], json!("t"), Kind::ListType),
      error(&["meta"], json!([]), Kind::DictType),
      error(&["choice"], json!("meh"), Kind::Literal(THUMBS)),
    ],
    "name_short" => vec![error(&["name"], json!("a"), Kind::StringTooShort { min_length: 2 })],
    "name_long" => vec![error(&["name"], json!("abcd"), Kind::StringTooLong { max_length: 3 })],
    "name_number" => vec![error(&["name"], json!(1.5), Kind::StringType)],
    "extra_field" => vec![error(&["zzz"], json!(1), Kind::ExtraForbidden)],
    "count_fraction" => vec![error(&["count"], json!(1.5), Kind::IntFromFloat)],
    "count_null" => vec![error(&["count"], Value::Null, Kind::IntType)],
    "ratio_high" => vec![error(&["ratio"], json!(2.0), Kind::LessThanEqual(Bound::Float(1.0)))],
    "ratio_string" => vec![error(&["ratio"], json!("x"), Kind::FloatParsing)],
    "ratio_list" => vec![error(&["ratio"], json!([]), Kind::FloatType)],
    "ratio_big" => vec![error(&["ratio"], json!(1e21), Kind::LessThanEqual(Bound::Float(1.0)))],
    "meta_bad_value" => vec![error(&["meta", "k"], json!("x"), Kind::IntParsing)],
    "meta_fraction" => vec![error(&["meta", "k"], json!(1.5), Kind::IntFromFloat)],
    "meta_null" => vec![error(&["meta"], Value::Null, Kind::DictType)],
    "tags_bad_item" => vec![error(&["tags", "0"], json!(1), Kind::StringType)],
    "tags_null" => vec![error(&["tags"], Value::Null, Kind::ListType)],
    "choice_number" => vec![error(&["choice"], json!(1), Kind::Literal(THUMBS))],
    "flag_string" => vec![error(&["flag"], json!("maybe"), Kind::BoolParsing)],
    "note_blank" => vec![error(&["note"], json!("   "), Kind::ValueError { message: "must not be blank".to_string() })],
    "list_body" => vec![error(&[], json!([1, 2]), Kind::ModelAttributesType)],
    "string_body" => vec![error(&[], json!("x"), Kind::ModelAttributesType)],
    "invalid_json_at_one" => vec![error(
      &["1"],
      json!({}),
      Kind::JsonInvalid { error: "Expecting property name enclosed in double quotes".to_string() },
    )],
    "invalid_json_mid_body" => {
      vec![error(&["26"], json!({}), Kind::JsonInvalid { error: "Expecting value".to_string() })]
    }
    "empty_body" => vec![error(&[], Value::Null, Kind::Missing)],
    "ratio_tiny" | "accepted" => Vec::new(),
    other => panic!("the body case {other} has no expected errors"),
  }
}

/// The 422 body's bytes, as `validation::response` renders them.
fn rendered(errors: &[FieldError]) -> String {
  String::from_utf8(charliebot_compat::json::starlette_bytes(&validation::detail(errors)))
    .expect("the 422 body is UTF-8")
}

#[test]
fn query_and_path_bodies_match_fastapi() {
  let fixture = fixture();
  let endpoints = fixture["endpoints"].as_object().expect("the fixture lists endpoints");
  let cases = fixture["query_cases"].as_array().expect("the fixture lists query cases");
  assert!(cases.len() > 80, "the fixture covers every parser's edge cases");
  for case in cases {
    let url = case["url"].as_str().expect("a case has a url");
    let template = case["endpoint"].as_str().expect("a case names its endpoint");
    let params = endpoints[template].as_array().expect("the endpoint declares parameters");
    let errors = parameter_errors(template, params, url);
    match case["status"].as_u64().expect("a case has a status") {
      422 => {
        assert_eq!(rendered(&errors), case["body"].as_str().expect("a 422 case has its body"), "422 body of {url}")
      }
      200 => assert!(errors.is_empty(), "{url} is valid, but the parsers returned {errors:?}"),
      other => panic!("{url}: FastAPI answered status {other}"),
    }
  }
}

#[test]
fn body_bodies_match_fastapi() {
  let fixture = fixture();
  let cases = fixture["body_cases"].as_array().expect("the fixture lists body cases");
  for case in cases {
    let name = case["name"].as_str().expect("a body case has a name");
    let errors = body_errors(name);
    match case["status"].as_u64().expect("a case has a status") {
      422 => {
        assert_eq!(rendered(&errors), case["body"].as_str().expect("a 422 case has its body"), "422 body of {name}")
      }
      200 => assert!(errors.is_empty(), "{name} is valid, but the rebuilt errors are {errors:?}"),
      other => panic!("{name}: FastAPI answered status {other}"),
    }
  }
}
