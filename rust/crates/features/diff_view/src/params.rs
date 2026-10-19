//! The query parameters of the `/api/git` routes, read as FastAPI reads them. A missing required name, a value outside
//! `Literal[...]`, or a value that is not a boolean is one 422 entry. A handler collects its entries in declaration
//! order and answers them together.

use std::collections::HashMap;

use charliebot_compat::validation::{self, FieldError, Source};

/// The decoded query string. A name that repeats keeps its last value, as Starlette's `QueryParams.get` does.
pub(crate) type Params = HashMap<String, String>;

/// A required text parameter.
pub(crate) fn text(params: &Params, name: &str) -> Result<String, FieldError> {
  params.get(name).cloned().ok_or_else(|| validation::missing(Source::Query, name))
}

/// An optional text parameter: `None` when the name is absent. An empty value is present.
pub(crate) fn optional_text(params: &Params, name: &str) -> Option<String> {
  params.get(name).cloned()
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
