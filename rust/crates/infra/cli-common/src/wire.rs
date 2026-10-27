//! The wire types the contract, the transport and the version-skew hint share: one request, one
//! response, and the phase a failure happened in. A leaf module, so `client`, `http` and `skew`
//! depend on it instead of on each other.

use std::time::Duration;

/// One internal-API method; no CLI path uses another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
  Get,
  Post,
}

/// One HTTP request on the wire, as the contract's transport sees it.
#[derive(Debug, Clone)]
pub struct WireRequest {
  pub method: Method,
  pub url: String,
  pub headers: Vec<(String, String)>,
  pub body: Option<Vec<u8>>,
  pub timeout: Duration,
}

/// One HTTP response: the status the contract's rejection check reads, plus the body.
#[derive(Debug, Clone)]
pub struct WireResponse {
  pub status: u16,
  pub reason: String,
  pub body: Vec<u8>,
}

/// A transport failure, split by the phase it failed in: a connect that never established
/// (`ConnectPhase`, retry is safe) and everything after the first byte left (`SentButLost`).
#[derive(Debug, Clone, PartialEq)]
pub enum TransportError {
  ConnectPhase(String),
  SentButLost(String),
}

impl TransportError {
  /// The error message the CLI's JSON object carries.
  pub fn message(&self) -> &str {
    match self {
      TransportError::ConnectPhase(message) | TransportError::SentButLost(message) => message,
    }
  }
}
