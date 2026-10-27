//! The restart-crossing call contract (`src/runtime/cli/common.py::_request_with_contract`).
//!
//! A connect-phase failure retries with bounded exponential backoff inside a total budget; a
//! sent-but-lost request is never re-issued — the call's readback, when it has one, answers, else
//! the CLI reports `outcome_unknown`. An explicit server rejection (status >= 400) carries the
//! response's `detail`, the version-skew `hint`, and the command's status-to-exit-code mapping.

use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::exit::CliExit;
use crate::json_out;
use crate::skew;
use crate::urlencode::urlencode;
use crate::wire::{Method, TransportError, WireRequest, WireResponse};

/// The clock the retry loop reads. Injected, so a test covers the backoff without waiting.
pub trait Clock {
  /// Seconds on a monotonic scale.
  fn now(&self) -> f64;
  /// Pause for `seconds`, as the retry loop's delay.
  fn sleep(&mut self, seconds: f64);
}

/// The process clock: a monotonic instant and a real thread sleep.
#[derive(Default)]
pub struct WallClock {
  start: std::sync::OnceLock<std::time::Instant>,
}

impl Clock for WallClock {
  fn now(&self) -> f64 {
    // The first read anchors the scale; the loop only ever reads differences.
    self.start.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
  }

  fn sleep(&mut self, seconds: f64) {
    std::thread::sleep(Duration::from_secs_f64(seconds));
  }
}

/// The per-call context: the base URL read from config, the auth headers, and the two bounds.
pub struct CallContext {
  pub base_url: String,
  pub headers: Vec<(String, String)>,
  /// One request's bound (`HTTP_INTERNAL_API_TIMEOUT`).
  pub request_timeout: f64,
  /// The connect-retry budget (`CLI_CONNECT_TOTAL_TIMEOUT`).
  pub connect_total_timeout: f64,
}

/// The readback the sent-but-lost class runs once: `Some` value prints as the result, and its own failure propagates.
pub type Readback<'a> = Box<dyn FnOnce() -> Result<Option<Value>, CliExit> + 'a>;

/// One internal-API call under the contract.
pub struct ApiCall<'a> {
  pub method: Method,
  pub endpoint: String,
  pub payload: Option<Value>,
  pub params: Option<Vec<(String, String)>>,
  pub readback: Option<Readback<'a>>,
  /// Status codes the command maps to an exit code other than 1.
  pub rejection_exit_codes: &'a [(u16, i32)],
}

/// `_request_with_contract`: issue one internal-API call and return its parsed JSON response.
pub fn call_api(
  mut call: ApiCall<'_>,
  ctx: &CallContext,
  transport: &mut dyn FnMut(&WireRequest) -> Result<WireResponse, TransportError>,
  clock: &mut dyn Clock,
) -> Result<Value, CliExit> {
  let url = build_url(&ctx.base_url, &call.endpoint, call.params.as_deref());
  // `json.dumps(payload)` defaults: the request body escapes non-ASCII and uses `", "`/`": "`.
  let body = call.payload.as_ref().map(|payload| {
    charliebot_core::pyfmt::json_dumps(payload, &charliebot_core::pyfmt::PYTHON_JSON_DEFAULT).into_bytes()
  });
  let mut headers = ctx.headers.clone();
  if body.is_some() {
    headers.push(("Content-Type".to_string(), "application/json".to_string()));
  }
  let deadline = clock.now() + ctx.connect_total_timeout;
  let mut attempt: u32 = 0;
  loop {
    let request = WireRequest {
      method: call.method,
      url: url.clone(),
      headers: headers.clone(),
      body: body.clone(),
      timeout: Duration::from_secs_f64(ctx.request_timeout),
    };
    let lost = match transport(&request) {
      Err(TransportError::ConnectPhase(message)) => {
        let remaining = deadline - clock.now();
        if remaining <= 0.0 {
          return Err(contract_exit(json!({ "error": message, "code": "server_unavailable", "effect": "none" })));
        }
        let delay = (0.25 * 2.0f64.powi(attempt as i32)).min(5.0).min(remaining);
        attempt += 1;
        clock.sleep(delay);
        continue;
      }
      Err(TransportError::SentButLost(message)) => message,
      Ok(response) if response.status >= 400 => {
        return Err(rejection_exit(&response, ctx, call.rejection_exit_codes, transport));
      }
      Ok(response) => match serde_json::from_slice::<Value>(&response.body) {
        Ok(value) => return Ok(value),
        // A malformed response body is a sent-but-lost outcome too.
        Err(error) => error.to_string(),
      },
    };
    // Sent but the response was lost or unreadable — never re-issue the call (its effect may have
    // landed). Read back this call's own on-disk artifact instead.
    if let Some(readback) = call.readback.take()
      && let Some(artifact) = readback()?
    {
      return Ok(artifact);
    }
    return Err(outcome_unknown_exit(lost, unknown_effect(call.method)));
  }
}

/// `"unknown"` for a call that may mutate, `"none"` for a read-only GET.
fn unknown_effect(method: Method) -> &'static str {
  match method {
    Method::Get => "none",
    Method::Post => "unknown",
  }
}

fn outcome_unknown_exit(message: String, effect: &str) -> CliExit {
  contract_exit(json!({ "error": message, "code": "outcome_unknown", "effect": effect }))
}

fn contract_exit(error: Value) -> CliExit {
  CliExit::contract(error, 1)
}

fn build_url(base_url: &str, endpoint: &str, params: Option<&[(String, String)]>) -> String {
  match params.filter(|params| !params.is_empty()) {
    None => format!("{base_url}{endpoint}"),
    Some(params) => format!("{base_url}{endpoint}?{}", urlencode(params)),
  }
}

/// `_exit_server_rejection`: the `detail` when the body carries one, else `HTTP <status> <reason>`;
/// the version-skew hint when a skew shows; the command's mapped exit code.
fn rejection_exit(
  response: &WireResponse,
  ctx: &CallContext,
  rejection_exit_codes: &[(u16, i32)],
  transport: &mut dyn FnMut(&WireRequest) -> Result<WireResponse, TransportError>,
) -> CliExit {
  let fallback = || json!(format!("HTTP {} {}", response.status, response.reason).trim());
  let message = serde_json::from_slice::<Value>(&response.body)
    .ok()
    .and_then(|body| body.get("detail").cloned())
    .unwrap_or_else(fallback);
  let mut error = Map::new();
  error.insert("error".to_string(), message);
  error.insert("code".to_string(), json!("server_error"));
  error.insert("effect".to_string(), json!("none"));
  if let Some(hint) = skew::version_skew_hint(&ctx.base_url, ctx.headers.as_slice(), transport) {
    error.insert("hint".to_string(), json!(hint));
  }
  let exit_code =
    rejection_exit_codes.iter().find(|(status, _)| *status == response.status).map(|(_, code)| *code).unwrap_or(1);
  CliExit::contract(Value::Object(error), exit_code)
}

/// One JSON error line exactly as the CLI prints it, for tests that assert on the shape.
pub fn exit_line(error: &Value) -> String {
  json_out::render(error, None)
}

/// `post_internal_api` on the process transport: base URL and auth headers read per call.
pub fn post(
  endpoint: &str,
  payload: Value,
  readback: Option<Readback<'_>>,
  rejection_exit_codes: &[(u16, i32)],
) -> Result<Value, CliExit> {
  run_call(ApiCall {
    method: Method::Post,
    endpoint: endpoint.to_string(),
    payload: Some(payload),
    params: None,
    readback,
    rejection_exit_codes,
  })
}

/// `get_api` on the process transport: a read-only call, so the sent-but-lost effect is `"none"`.
pub fn get(endpoint: &str, params: Option<Vec<(String, String)>>) -> Result<Value, CliExit> {
  run_call(ApiCall {
    method: Method::Get,
    endpoint: endpoint.to_string(),
    payload: None,
    params,
    readback: None,
    rejection_exit_codes: &[],
  })
}

fn run_call(call: ApiCall<'_>) -> Result<Value, CliExit> {
  let ctx = CallContext {
    base_url: crate::base_url::internal_base_url()?,
    headers: crate::headers::internal_api_auth_headers()?,
    request_timeout: crate::timeouts::HTTP_INTERNAL_API_TIMEOUT.as_secs_f64(),
    connect_total_timeout: crate::timeouts::CLI_CONNECT_TOTAL_TIMEOUT.as_secs_f64(),
  };
  call_api(call, &ctx, &mut crate::http::send_http, &mut WallClock::default())
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Write};
  use std::net::{TcpListener, TcpStream};

  use super::*;
  use serde_json::json;

  struct FakeClock {
    now: f64,
    sleeps: Vec<f64>,
  }

  impl FakeClock {
    fn new() -> FakeClock {
      FakeClock { now: 0.0, sleeps: Vec::new() }
    }
  }

  impl Clock for FakeClock {
    fn now(&self) -> f64 {
      self.now
    }

    fn sleep(&mut self, seconds: f64) {
      self.sleeps.push(seconds);
      self.now += seconds;
    }
  }

  fn ctx() -> CallContext {
    CallContext {
      base_url: "http://localhost:49991".to_string(),
      headers: Vec::new(),
      request_timeout: 30.0,
      connect_total_timeout: 2.0,
    }
  }

  fn call(method: Method) -> ApiCall<'static> {
    ApiCall {
      method,
      endpoint: "/api/internal/x".to_string(),
      payload: Some(json!({ "a": 1 })),
      params: None,
      readback: None,
      rejection_exit_codes: &[],
    }
  }

  #[test]
  fn test_connect_never_established_retries_with_backoff_then_exhausts() {
    // Python: tests/test_cli_restart_contract.py, with the clock injected so no wall time passes.
    let mut attempts = 0;
    let refused = "[Errno 111] Connection refused".to_string();
    let mut transport = |_request: &WireRequest| -> Result<WireResponse, TransportError> {
      attempts += 1;
      Err(TransportError::ConnectPhase(refused.clone()))
    };
    let mut clock = FakeClock::new();

    let error = call_api(call(Method::Post), &ctx(), &mut transport, &mut clock).unwrap_err();

    assert_eq!(error.code, 1);
    assert_eq!(
      error.body,
      json!({ "error": "[Errno 111] Connection refused", "code": "server_unavailable", "effect": "none" })
    );
    // Retried, not failed on the first attempt, with the base delay doubling until the remaining
    // budget clamps it, and the sleeps bounded by the total budget.
    assert!(attempts > 1);
    assert_eq!(clock.sleeps[..3], [0.25, 0.5, 1.0]);
    assert_eq!(attempts, clock.sleeps.len() + 1);
    assert!(clock.sleeps.iter().sum::<f64>() <= 2.0);
  }

  #[test]
  fn a_sent_but_lost_post_runs_the_readback_once_and_prints_outcome_unknown_when_it_finds_nothing() {
    let mut readback_runs = 0;
    let mut transport = |_request: &WireRequest| -> Result<WireResponse, TransportError> {
      Err(TransportError::SentButLost("[Errno 104] Connection reset by peer".to_string()))
    };
    let mut clock = FakeClock::new();
    let mut call = call(Method::Post);
    call.readback = Some(Box::new(|| {
      readback_runs += 1;
      Ok(None)
    }));

    let error = call_api(call, &ctx(), &mut transport, &mut clock).unwrap_err();

    assert_eq!(readback_runs, 1);
    assert_eq!(
      error.body,
      json!({ "error": "[Errno 104] Connection reset by peer", "code": "outcome_unknown", "effect": "unknown" })
    );
  }

  #[test]
  fn a_get_that_lost_its_response_reports_effect_none_without_a_readback() {
    let mut transport = |_request: &WireRequest| -> Result<WireResponse, TransportError> {
      Err(TransportError::SentButLost("connection closed".to_string()))
    };
    let mut clock = FakeClock::new();

    let error = call_api(call(Method::Get), &ctx(), &mut transport, &mut clock).unwrap_err();

    assert_eq!(error.body, json!({ "error": "connection closed", "code": "outcome_unknown", "effect": "none" }));
  }

  #[test]
  fn a_readback_value_prints_as_the_result() {
    let mut transport = |_request: &WireRequest| -> Result<WireResponse, TransportError> {
      Err(TransportError::SentButLost("reset".to_string()))
    };
    let mut clock = FakeClock::new();
    let mut call = call(Method::Post);
    call.readback = Some(Box::new(|| Ok(Some(json!({ "session_id": "task-1" })))));

    let value = call_api(call, &ctx(), &mut transport, &mut clock).unwrap();

    assert_eq!(value, json!({ "session_id": "task-1" }));
  }

  #[test]
  fn a_malformed_2xx_body_runs_the_readback_once_and_never_resends() {
    let mut sends = 0;
    let mut transport = |_request: &WireRequest| -> Result<WireResponse, TransportError> {
      sends += 1;
      Ok(WireResponse { status: 200, reason: "OK".to_string(), body: b"not json".to_vec() })
    };
    let mut clock = FakeClock::new();
    let mut call = call(Method::Post);
    call.readback = Some(Box::new(|| Ok(Some(json!({ "session_id": "task-1" })))));

    let value = call_api(call, &ctx(), &mut transport, &mut clock).unwrap();

    assert_eq!(value, json!({ "session_id": "task-1" }));
    assert_eq!(sends, 1);
  }

  #[test]
  fn a_rejection_carries_the_detail_and_the_mapped_exit_code() {
    let mut transport = |_request: &WireRequest| -> Result<WireResponse, TransportError> {
      Ok(WireResponse {
        status: 422,
        reason: "Unprocessable Entity".to_string(),
        body: br#"{"detail":"refused"}"#.to_vec(),
      })
    };
    let mut clock = FakeClock::new();
    let mut call = call(Method::Post);
    call.rejection_exit_codes = &[(422, 2)];

    let error = call_api(call, &ctx(), &mut transport, &mut clock).unwrap_err();

    assert_eq!(error.code, 2);
    assert_eq!(error.body, json!({ "error": "refused", "code": "server_error", "effect": "none" }));
  }

  #[test]
  fn a_rejection_without_detail_names_the_status_line() {
    let mut transport = |_request: &WireRequest| -> Result<WireResponse, TransportError> {
      Ok(WireResponse { status: 404, reason: "Not Found".to_string(), body: b"not json".to_vec() })
    };
    let mut clock = FakeClock::new();

    let error = call_api(call(Method::Get), &ctx(), &mut transport, &mut clock).unwrap_err();

    assert_eq!(error.body["error"], json!("HTTP 404 Not Found"));
  }

  /// The head and body one client sent: read until the head ends, then the body its Content-Length names.
  fn read_post(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
      let read = stream.read(&mut chunk).expect("the request arrives within the read timeout");
      assert!(read > 0, "the client closed before the request ended");
      buffer.extend_from_slice(&chunk[..read]);
      let Some(head_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else { continue };
      let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
      let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
      if buffer.len() >= head_end + 4 + length {
        return String::from_utf8_lossy(&buffer[..head_end + 4 + length]).into_owned();
      }
    }
  }

  #[test]
  fn test_post_sends_json_body_content_type_and_auth_header_over_the_real_client() {
    // Python: tests/test_cli_restart_contract.py. One POST with a query and a JSON body over the real
    // transport: the wire carries the query string, the Content-Type and the body, and no Authorization
    // header when the call carries no credential.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
      let request = read_post(&mut stream);
      let body = br#"{"ok": true}"#;
      let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(head.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
      request
    });
    let ctx = CallContext {
      base_url: format!("http://localhost:{port}"),
      headers: Vec::new(),
      request_timeout: 5.0,
      connect_total_timeout: 2.0,
    };
    let call = ApiCall {
      method: Method::Post,
      endpoint: "/api/internal/x".to_string(),
      payload: Some(json!({ "a": 1 })),
      params: Some(vec![("k".to_string(), "v".to_string())]),
      readback: None,
      rejection_exit_codes: &[],
    };

    let value = call_api(call, &ctx, &mut crate::http::send_http, &mut WallClock::default()).unwrap();

    assert_eq!(value, json!({ "ok": true }));
    let request = server.join().unwrap();
    let lowered = request.to_ascii_lowercase();
    assert!(request.starts_with("POST /api/internal/x?k=v HTTP/1.1\r\n"), "{request}");
    assert!(lowered.contains("\r\ncontent-type: application/json\r\n"), "{request}");
    assert!(!lowered.contains("\r\nauthorization:"), "{request}");
    assert!(request.ends_with(r#"{"a": 1}"#), "{request}");
  }
}
