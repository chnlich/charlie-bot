//! The responses the Python app's framework layers produce: starlette `JSONResponse`, FastAPI's
//! `HTTPException` rendering, `PlainTextResponse`, `RedirectResponse`, and `ServerErrorMiddleware`'s 500.

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderValue, StatusCode, header};
use percent_encoding::{AsciiSet, utf8_percent_encode};
use serde_json::{Value, json};

use crate::json;
use crate::urlquote::quote_set;

/// The failure behind a 500: the name the `http_request` log line carries in `error` (the Python
/// exception class the same failure raises there) and the detail logged beside it.
#[derive(Debug, Clone)]
pub struct Failure {
  pub kind: String,
  pub detail: String,
}

/// starlette `JSONResponse(content, status_code)`: compact separators, raw UTF-8.
pub fn json(status: StatusCode, content: &Value) -> Response {
  bytes_response(status, "application/json", Bytes::from(json::starlette_bytes(content)))
}

/// `FastJsonResponse(content, status_code)` (src/infra/responses.py): the same media type, orjson's bytes.
pub fn fast_json(status: StatusCode, content: &Value) -> Response {
  bytes_response(status, "application/json", Bytes::from(json::fast_json_bytes(content)))
}

/// FastAPI's rendering of `HTTPException(status_code)`: `{"detail": <reason phrase>}`.
pub fn http_exception(status: StatusCode) -> Response {
  json(status, &json!({ "detail": status.canonical_reason().expect("every raised status has a reason phrase") }))
}

/// FastAPI's 405 for a route whose path matched and whose methods did not.
pub fn method_not_allowed(allow: &str) -> Response {
  let mut response = http_exception(StatusCode::METHOD_NOT_ALLOWED);
  response.headers_mut().insert(header::ALLOW, HeaderValue::from_str(allow).expect("method names are header-safe"));
  response
}

/// starlette `PlainTextResponse(content, status_code)`.
pub fn plain_text(status: StatusCode, content: &str) -> Response {
  bytes_response(status, "text/plain; charset=utf-8", Bytes::from(content.to_string()))
}

/// starlette `HTMLResponse(content)`.
pub fn html(content: String) -> Response {
  bytes_response(StatusCode::OK, "text/html; charset=utf-8", Bytes::from(content))
}

/// starlette `RedirectResponse(url)`: 307, empty body, the URL quoted as starlette quotes it.
pub fn redirect(url: &str) -> Response {
  let location = quote_redirect_url(url);
  Response::builder()
    .status(StatusCode::TEMPORARY_REDIRECT)
    .header(header::CONTENT_LENGTH, "0")
    .header(header::LOCATION, HeaderValue::from_str(&location).expect("a quoted URL is header-safe"))
    .body(Body::empty())
    .expect("a redirect response builds")
}

/// `ServerErrorMiddleware`'s answer to a failed request: 500 `Internal Server Error` as plain text.
/// The failure rides the response as an extension so the request log names it.
pub fn internal_error(kind: &str, detail: String) -> Response {
  tracing::error!(error = kind, detail = %detail, "Exception in ASGI application");
  let mut response = plain_text(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
  response.extensions_mut().insert(Failure { kind: kind.to_string(), detail });
  response
}

fn bytes_response(status: StatusCode, content_type: &'static str, body: Bytes) -> Response {
  Response::builder()
    .status(status)
    .header(header::CONTENT_TYPE, content_type)
    .header(header::CONTENT_LENGTH, body.len())
    .body(Body::from(body))
    .expect("a fixed-header response builds")
}

/// `urllib.parse.quote(url, safe=":/%#?=@[]!$&'()*+,;")`, starlette's `RedirectResponse` quoting.
static REDIRECT_QUOTE: AsciiSet = quote_set(b":/%#?=@[]!$&'()*+,;");

fn quote_redirect_url(url: &str) -> String {
  utf8_percent_encode(url, &REDIRECT_QUOTE).to_string()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn redirect_quoting_keeps_url_syntax_and_escapes_the_rest() {
    assert_eq!(quote_redirect_url("http://h:1/a b/é?x=1&y=%41"), "http://h:1/a%20b/%C3%A9?x=1&y=%41");
  }

  #[test]
  fn redirect_quoting_keeps_the_safe_set_and_unreserved_bytes_and_escapes_controls() {
    assert_eq!(quote_redirect_url("[]!$&'()*+,;:/%#?=@"), "[]!$&'()*+,;:/%#?=@");
    assert_eq!(quote_redirect_url("a~b_c.d-e"), "a~b_c.d-e");
    assert_eq!(quote_redirect_url("\"<>{}|\\^`\n\u{7f}"), "%22%3C%3E%7B%7D%7C%5C%5E%60%0A%7F");
  }
}
