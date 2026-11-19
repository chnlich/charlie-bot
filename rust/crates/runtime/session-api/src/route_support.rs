//! Helpers the session route modules share for requests and responses: the blocking-pool store call, the
//! Starlette-style query string, and the store-error response.

use std::borrow::Cow;

use axum::body::Body;
use axum::http::Request;
use axum::response::Response;
use charliebot_compat::validation;
use charliebot_store::{StoreError, StoreResult};

/// The store error as its route's JSON response. `charliebot_registry::task_errors` owns the mapping.
pub(crate) use charliebot_registry::task_errors::store_error;

/// A handler's store work run on the blocking pool, so a slow read holds no async worker.
pub(crate) async fn blocking_store<T, F>(work: F) -> StoreResult<T>
where
  T: Send + 'static,
  F: FnOnce() -> StoreResult<T> + Send + 'static,
{
  tokio::task::spawn_blocking(work).await.map_err(|error| StoreError::new(error.to_string()))?
}

pub(crate) struct Query<'a> {
  values: Vec<(Cow<'a, str>, Cow<'a, str>)>,
}

impl<'a> Query<'a> {
  /// The query string's pairs, in order, decoded with `form_urlencoded::parse` semantics. A `+` reads as a
  /// space, a `%XX` escape decodes with replacement, and a part without a value keeps its blank value.
  ///
  /// A pair that carries neither `%` nor `+` decodes to its own bytes, so it borrows the request instead of taking a
  /// copy of them. The sidebar's status poll names its row set in one `ids` value of several kilobytes and repeats it
  /// every few seconds. Copying that value was most of the route's query cost.
  pub(crate) fn new(request: &'a Request<Body>) -> Self {
    let raw = request.uri().query().unwrap_or_default();
    let values = raw.split('&').filter(|part| !part.is_empty()).map(decode_pair).collect();
    Query { values }
  }

  pub(crate) fn get(&self, key: &str) -> Option<&str> {
    self.values.iter().find(|(name, _)| name.as_ref() == key).map(|(_, value)| value.as_ref())
  }

  pub(crate) fn parse_int(
    &self,
    name: &str,
    default: Option<i64>,
    ge: Option<i64>,
    le: Option<i64>,
  ) -> Result<i64, Box<Response>> {
    match self.get(name) {
      Some(value) => validation::parse_int(validation::Source::Query, name, value, ge, le)
        .map_err(|error| Box::new(validation::response(&[*error]))),
      None => {
        default.ok_or_else(|| Box::new(validation::response(&[validation::missing(validation::Source::Query, name)])))
      }
    }
  }

  pub(crate) fn parse_bool(&self, name: &str, default: bool) -> Result<bool, Box<Response>> {
    match self.get(name) {
      Some(value) => validation::parse_bool(validation::Source::Query, name, value)
        .map_err(|error| Box::new(validation::response(&[*error]))),
      None => Ok(default),
    }
  }
}

/// One raw `key=value` part as its decoded name and value.
///
/// A part without a `%` or a `+` decodes to its own bytes. The split leaves one name and one value, and a part
/// without `=` keeps its blank value. A part that carries either escape takes `form_urlencoded::parse`, which reads
/// the part as one pair and decodes both halves as the whole-string parse did.
fn decode_pair(part: &str) -> (Cow<'_, str>, Cow<'_, str>) {
  if memchr::memchr2(b'%', b'+', part.as_bytes()).is_none() {
    return match part.split_once('=') {
      Some((name, value)) => (Cow::Borrowed(name), Cow::Borrowed(value)),
      None => (Cow::Borrowed(part), Cow::Borrowed("")),
    };
  }
  form_urlencoded::parse(part.as_bytes()).next().expect("a non-empty part parses as one pair")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn query_pairs_decode_plus_and_percent_escapes_and_keep_blanks_in_order() {
    let request =
      Request::builder().uri("/api/sessions/?a=Atlas+%C3%A9&flag=&a=second&plain").body(Body::empty()).unwrap();
    let query = Query::new(&request);
    assert_eq!(decoded(&query), vec![("a", "Atlas é"), ("flag", ""), ("a", "second"), ("plain", "")]);
    assert_eq!(query.get("a"), Some("Atlas é"), "the first occurrence answers");
    assert_eq!(query.get("plain"), Some(""), "a part without `=` keeps its blank value");
  }

  #[test]
  fn a_query_value_without_an_escape_borrows_the_request_and_one_with_an_escape_decodes() {
    let request = Request::builder()
      .uri("/api/sessions/status?ids=a,b,c&q=x%3Dy&plain=a=b&plus=1%2B2")
      .body(Body::empty())
      .unwrap();
    let query = Query::new(&request);
    assert!(matches!(query.values[0].1, Cow::Borrowed("a,b,c")), "a value with no escape borrows the request");
    assert_eq!(query.get("ids"), Some("a,b,c"));
    assert_eq!(query.get("q"), Some("x=y"), "an escaped `=` decodes inside the value");
    assert_eq!(query.get("plain"), Some("a=b"), "the value keeps its later `=`");
    assert_eq!(query.get("plus"), Some("1+2"), "an escaped `+` stays a plus");
    assert_eq!(decoded(&query), vec![("ids", "a,b,c"), ("q", "x=y"), ("plain", "a=b"), ("plus", "1+2")]);
  }

  #[test]
  fn a_request_without_a_query_parses_no_pairs() {
    let request = Request::builder().uri("/api/sessions/").body(Body::empty()).unwrap();
    assert_eq!(Query::new(&request).values.len(), 0);
  }

  #[test]
  fn an_empty_part_between_separators_parses_no_pair() {
    let request = Request::builder().uri("/api/sessions/?&a=1&&b=2&").body(Body::empty()).unwrap();
    assert_eq!(decoded(&Query::new(&request)), vec![("a", "1"), ("b", "2")]);
  }

  fn decoded<'a>(query: &'a Query<'a>) -> Vec<(&'a str, &'a str)> {
    query.values.iter().map(|(name, value)| (name.as_ref(), value.as_ref())).collect()
  }
}
