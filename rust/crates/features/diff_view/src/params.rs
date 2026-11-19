//! The query parameters of the `/api/git` routes. A missing required name, a value outside the allowed literals,
//! or a value that is not a boolean is one 422 entry. A handler collects its entries in declaration
//! order and answers them together.

use std::collections::HashMap;

use charliebot_compat::validation::{self, FieldError, Source};

/// The decoded query string. A name that repeats keeps its last value.
pub(crate) type Params = HashMap<String, String>;

/// The `mode` a diff range compares by: `three-dot` diffs from the merge base, `two-dot` from the base commit.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RangeMode {
  ThreeDot,
  TwoDot,
}

impl RangeMode {
  /// Every mode, in the order an error names them.
  const ALL: [Self; 2] = [Self::ThreeDot, Self::TwoDot];

  /// The literals the `mode` parameter accepts, in the order an error names them.
  pub(crate) const LITERALS: &'static [&'static str] = &[Self::ThreeDot.as_str(), Self::TwoDot.as_str()];

  /// The mode a validated `mode` literal names. A value outside [`Self::LITERALS`] broke validation's contract.
  pub(crate) fn of_literal(literal: &str) -> Self {
    match Self::ALL.into_iter().find(|mode| mode.as_str() == literal) {
      Some(mode) => mode,
      None => unreachable!("mode literal {literal} outside the validated set"),
    }
  }

  /// The literal the `mode` parameter and the response name this mode by.
  pub(crate) const fn as_str(self) -> &'static str {
    match self {
      Self::ThreeDot => "three-dot",
      Self::TwoDot => "two-dot",
    }
  }
}

/// The side of a range a `/api/git/diff/lines` read takes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Side {
  Base,
  Head,
}

impl Side {
  /// Every side, in the order an error names them.
  const ALL: [Self; 2] = [Self::Base, Self::Head];

  /// The literals the `side` parameter accepts, in the order an error names them.
  pub(crate) const LITERALS: &'static [&'static str] = &[Self::Base.as_str(), Self::Head.as_str()];

  /// The side a validated `side` literal names. A value outside [`Self::LITERALS`] broke validation's contract.
  pub(crate) fn of_literal(literal: &str) -> Self {
    match Self::ALL.into_iter().find(|side| side.as_str() == literal) {
      Some(side) => side,
      None => unreachable!("side literal {literal} outside the validated set"),
    }
  }

  /// The literal the `side` parameter names this side by.
  pub(crate) const fn as_str(self) -> &'static str {
    match self {
      Self::Base => "base",
      Self::Head => "head",
    }
  }
}

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

/// A parameter's value when it parsed, with its error appended to `errors` when it did not.
pub(crate) fn take<T>(outcome: Result<T, FieldError>, errors: &mut Vec<FieldError>) -> Option<T> {
  match outcome {
    Ok(value) => Some(value),
    Err(error) => {
      errors.push(error);
      None
    }
  }
}
