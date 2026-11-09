//! The query parameters of the `/api/git` routes. A missing required name, a value outside the allowed literals,
//! or a value that is not a boolean is one 422 entry. A handler collects its entries in declaration
//! order and answers them together.

use std::collections::HashMap;

use charliebot_compat::validation::{self, FieldError, Source};

/// The decoded query string. A name that repeats keeps its last value.
pub(crate) type Params = HashMap<String, String>;

/// A required text parameter.
pub(crate) fn text(params: &Params, name: &str) -> Result<String, FieldError> {
  params.get(name).cloned().ok_or_else(|| validation::missing(Source::Query, name))
}

/// An optional text parameter: `None` when the name is absent. An empty value is present.
pub(crate) fn optional_text(params: &Params, name: &str) -> Option<String> {
  params.get(name).cloned()
}

/// A required `Literal[...]` parameter.
pub(crate) fn one_of(
  params: &Params,
  name: &str,
  choices: &'static [&'static str],
) -> Result<&'static str, FieldError> {
  match params.get(name) {
    Some(value) => validation::parse_literal(Source::Query, name, value, choices).map_err(|error| *error),
    None => Err(validation::missing(Source::Query, name)),
  }
}

/// An `int` parameter, at least `ge`.
pub(crate) fn uint(params: &Params, name: &str, ge: i64) -> Result<i64, FieldError> {
  match params.get(name) {
    Some(text) => validation::parse_int(Source::Query, name, text, Some(ge), None).map_err(|error| *error),
    None => Err(validation::missing(Source::Query, name)),
  }
}

/// A `Literal[...]` parameter with its default when absent.
pub(crate) fn literal(
  params: &Params,
  name: &str,
  default: &'static str,
  choices: &'static [&'static str],
) -> Result<&'static str, FieldError> {
  match params.get(name) {
    Some(value) => validation::parse_literal(Source::Query, name, value, choices).map_err(|error| *error),
    None => Ok(default),
  }
}

/// A `bool` parameter with its default when absent.
pub(crate) fn flag(params: &Params, name: &str, default: bool) -> Result<bool, FieldError> {
  match params.get(name) {
    Some(value) => validation::parse_bool(Source::Query, name, value).map_err(|error| *error),
    None => Ok(default),
  }
}

/// Keeps a parameter's value, or records its error in declaration order.
pub(crate) fn take<T>(outcome: Result<T, FieldError>, errors: &mut Vec<FieldError>) -> Option<T> {
  match outcome {
    Ok(value) => Some(value),
    Err(error) => {
      errors.push(error);
      None
    }
  }
}
