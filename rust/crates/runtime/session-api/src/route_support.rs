//! Request and response helpers the session route modules share: the blocking-pool store call, the Starlette-style
//! query string, and the store-error response.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use charliebot_compat::{responses, validation};
use charliebot_store::{StoreError, StoreResult};
use serde_json::json;

/// Run a handler's store work on the blocking pool, as Python's handlers run it in `asyncio.to_thread`, so a slow
/// read holds no async worker.
pub(crate) async fn blocking_store<T, F>(work: F) -> StoreResult<T>
where
  T: Send + 'static,
  F: FnOnce() -> StoreResult<T> + Send + 'static,
{
  tokio::task::spawn_blocking(work).await.map_err(|error| StoreError::new(error.to_string()))?
}

/// The store error as the route's JSON response: the mapped status when the error carries one, else the
/// `RuntimeError` internal error the Python handlers raise.
pub(crate) fn store_error(error: StoreError) -> Response {
  if let Some(status) = error.status_code().and_then(|code| StatusCode::from_u16(code).ok()) {
    return responses::json(status, &json!({"detail": error.to_string()}));
  }
  responses::internal_error("RuntimeError", error.to_string())
}

pub(crate) struct Query {
  values: Vec<(String, String)>,
}

impl Query {
  /// The query string's pairs, in order, decoded the way Starlette's `parse_qsl(keep_blank_values=True)`
  /// decodes them behind FastAPI's `Query` (`form_urlencoded::parse` has those semantics): `+` reads as a
  /// space, `%XX` escapes decode with replacement, and a part without a value keeps its blank value.
  pub(crate) fn new(request: &Request<Body>) -> Self {
    let values = form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
      .map(|(key, value)| (key.into_owned(), value.into_owned()))
      .collect();
    Query { values }
  }

  pub(crate) fn get(&self, key: &str) -> Option<&str> {
    self.values.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn query_pairs_decode_plus_and_percent_escapes_and_keep_blanks_in_order() {
    let request =
      Request::builder().uri("/api/sessions/?a=Atlas+%C3%A9&flag=&a=second&plain").body(Body::empty()).unwrap();
    let query = Query::new(&request);
    assert_eq!(
      query.values,
      vec![
        ("a".to_string(), "Atlas é".to_string()),
        ("flag".to_string(), String::new()),
        ("a".to_string(), "second".to_string()),
        ("plain".to_string(), String::new()),
      ]
    );
    assert_eq!(query.get("a"), Some("Atlas é"), "the first occurrence answers");
    assert_eq!(query.get("plain"), Some(""), "a part without `=` keeps its blank value");
  }

  #[test]
  fn a_request_without_a_query_parses_no_pairs() {
    let request = Request::builder().uri("/api/sessions/").body(Body::empty()).unwrap();
    assert_eq!(Query::new(&request).values, Vec::<(String, String)>::new());
  }
}
