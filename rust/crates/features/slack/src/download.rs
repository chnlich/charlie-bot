//! `charliebot slack download`: write one Slack-hosted file to a path the caller names.
//!
//! Slack serves a message file at the `url_private` or `url_private_download` of its `files[]` entry, on
//! `https://files.slack.com`. A token without the `files:read` scope, or one that cannot see the file, gets an
//! HTML sign-in page or a redirect instead of the bytes. This module detects that answer, fails with exit 1 and
//! writes nothing, so the output path never holds a partial or sign-in file.

use std::path::Path;

use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::request::{self, Request};
use charliebot_compat::mimetypes_table::TEXT_HTML;
use serde_json::json;

use crate::token::TokenKind;

/// The longest body prefix inspected for an HTML sign-in page, in bytes.
const HTML_SNIFF_BYTES: usize = 15;

/// The Slack file at `url`, written to `output`: the body is fetched with `token`, and the result is the number of
/// bytes written.
pub fn download(url: &str, output: &Path, kind: TokenKind, token: &str) -> Result<usize, CliExit> {
  let request = Request::new(reqwest::Method::GET, url);
  let response = request::send(&request.bearer(token), &[token])?;
  let body = file_body(&response, kind, token)?;
  std::fs::write(output, &body)
    .map_err(|error| CliExit::error(format!("could not write {}: {error}", output.display())))?;
  Ok(body.len())
}

/// The response body when the answer is the file; a sign-in page, a redirect or an API error is a loud failure.
fn file_body(response: &request::Response, kind: TokenKind, token: &str) -> Result<Vec<u8>, CliExit> {
  if (300..400).contains(&response.status) {
    return Err(not_the_file(response.status, "Slack redirected the request", kind));
  }
  if !(200..300).contains(&response.status) {
    return Err(http_not_the_file(response, kind, token));
  }
  if is_html(response) {
    return Err(not_the_file(response.status, "Slack answered with a sign-in page", kind));
  }
  if let Some(error) = slack_error(response) {
    let cause = format!("Slack answered with the API error {}", request::redact(&error, &[token]));
    return Err(not_the_file(response.status, &cause, kind));
  }
  Ok(response.body.clone())
}

/// A `text/html` answer or a body that opens an HTML document: Slack's sign-in page standing in for the file.
fn is_html(response: &request::Response) -> bool {
  if response.content_type.as_deref().is_some_and(|value| value.to_ascii_lowercase().starts_with(TEXT_HTML)) {
    return true;
  }
  let head: Vec<u8> =
    response.body.iter().skip_while(|byte| byte.is_ascii_whitespace()).take(HTML_SNIFF_BYTES).copied().collect();
  let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
  head.starts_with("<!doctype html") || head.starts_with("<html")
}

/// Slack answers a file host request with `"ok": false` for a token or file it will not serve.
fn slack_error(response: &request::Response) -> Option<String> {
  let trimmed: &[u8] = {
    let start = response.body.iter().position(|byte| !byte.is_ascii_whitespace())?;
    &response.body[start..]
  };
  if trimmed.first() != Some(&b'{') {
    return None;
  }
  let payload: serde_json::Value = serde_json::from_slice(trimmed).ok()?;
  if payload.get("ok").and_then(serde_json::Value::as_bool) != Some(false) {
    return None;
  }
  Some(payload.get("error").and_then(serde_json::Value::as_str).unwrap_or("unknown").to_string())
}

/// A non-2xx answer other than a redirect, with the redacted body and the likely cause.
fn http_not_the_file(response: &request::Response, kind: TokenKind, token: &str) -> CliExit {
  let cause = format!("Slack answered with HTTP status {}", response.status);
  let response_text = request::redact(&String::from_utf8_lossy(&response.body), &[token]);
  CliExit::contract(
    json!({
      "status": response.status,
      "response": response_text,
      "error": scope_hint(&cause, kind),
    }),
    1,
  )
}

/// The exit-1 failure that names the likely cause: a missing `files:read` scope or a file the token cannot see.
fn not_the_file(status: u16, cause: &str, kind: TokenKind) -> CliExit {
  CliExit::contract(json!({ "status": status, "error": scope_hint(cause, kind) }), 1)
}

fn scope_hint(cause: &str, kind: TokenKind) -> String {
  format!(
    "{cause} instead of the file; slack.{} may lack the files:read scope, or the token cannot see this file.",
    kind.key()
  )
}
