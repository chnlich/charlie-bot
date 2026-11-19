//! One failed outbound REST call to a platform's HTTP API: its method and path, the HTTP status, the platform's error
//! code, and the platform's message. A channel crate's client raises it and the reply path renders it as a 502
//! refusal. The bot token and the request body never reach it.

use std::fmt;

use charliebot_core::text_repr;

/// One failed REST call: its method, path, HTTP status, the platform's error code (None when the answer carries none)
/// and the human message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
  pub method: String,
  pub path: String,
  pub status: u16,
  pub code: Option<i64>,
  pub message: String,
}

impl fmt::Display for ApiError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let code = self.code.map_or_else(|| "None".to_string(), |code| code.to_string());
    write!(
      formatter,
      "{} {} failed: HTTP {} code={code} message={}",
      self.method,
      self.path,
      self.status,
      text_repr::str_repr(&self.message)
    )
  }
}

impl std::error::Error for ApiError {}
