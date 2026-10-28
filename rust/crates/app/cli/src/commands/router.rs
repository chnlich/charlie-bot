//! The backend router (`src/features/router/routing.py` and its CLI wrapper).
//!
//! `charliebot router` answers one backend-choice question per process: the caller writes one JSON
//! object on stdin and reads one back on stdout.
//!
//!   {"role": "implement", "text": "<the work>", "candidates": [{"id": "claude-opus", "cooling_until": null}]}
//!   {"backends": ["claude-opus"], "reason": "design p=0.86"}
//!
//! Design work gets the `strong` ids that are also candidates, in `strong` order; routine work gets
//! an empty list, which tells the caller to keep using its pool. `<charliebot home>/router.yaml` is
//! read on every call, so an edit takes effect at the next choice; the Jev key comes from the
//! `credentials.yaml` section that `jev.credential` names. The caller bounds one routing call at
//! [`timeouts::ROUTER_TIMEOUT_SECONDS`], which is why one attempt is bounded at 5 s and the single
//! retry after an HTTP 429 waits 10 s: two attempts plus the wait fit inside that budget.
//!
//! The answer is the only thing printed on stdout. Every failure — a broken `router.yaml`, a missing
//! Jev key, malformed stdin, a Jev error, a second HTTP 429 — writes `router: <message>` on stderr
//! and exits 1; the command never prints a fallback answer.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::json_out;
use charliebot_core::credentials;
use charliebot_core::home;
use charliebot_core::paths;
use charliebot_core::pyfmt::{self, JsonStyle};
use charliebot_core::timeouts;
use clap::{CommandFactory, Parser};
use serde_json::{Value, json};
use serde_yaml::{Mapping, Value as YamlValue};

const ROUTER_FILENAME: &str = "router.yaml";

/// A thread's text keeps only this many trailing characters: a thread grows without bound, Jev's input cap does not.
const THREAD_TEXT_LIMIT: usize = 20_000;

const ROUTINE: &str = "routine";
const DESIGN: &str = "design";
const DESIGN_THRESHOLD: f64 = 0.5;

/// The two Jev bounds the caller's [`timeouts::ROUTER_TIMEOUT_SECONDS`] budget shapes: one attempt
/// and the pause before the single 429 retry. Tests inject smaller values so no test sleeps the
/// real 10 s wait or waits out the real 5 s attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
  pub request_timeout: Duration,
  pub rate_limit_wait: Duration,
}

#[derive(Parser)]
#[command(
  name = "router",
  about = "Choose backends for one CharlieBot task: ask Jev whether the work is routine or design work"
)]
struct RouterCli {}

/// Parse the arguments and run one routing call. Parser errors print and exit on their own
/// (argparse-compatible: help 0, usage error 2).
pub fn main(args: &[OsString]) {
  let command = with_infer_long_args(RouterCli::command());
  let argv = std::iter::once(OsString::from("charliebot router")).chain(args.iter().cloned());
  if let Err(error) = command.try_get_matches_from(argv) {
    error.exit();
  }
  let mut raw = String::new();
  if let Err(error) = std::io::stdin().read_to_string(&mut raw) {
    eprintln!("router: {error}");
    std::process::exit(1);
  }
  let limits = Limits {
    request_timeout: Duration::from_secs(timeouts::ROUTER_JEV_REQUEST_TIMEOUT),
    rate_limit_wait: Duration::from_secs(timeouts::ROUTER_JEV_RATE_LIMIT_WAIT),
  };
  match route(&raw, &limits, &mut |pause| std::thread::sleep(pause)) {
    Ok(answer) => print!("{}", json_out::render(&answer, None)),
    Err(message) => {
      eprintln!("router: {message}");
      std::process::exit(1);
    }
  }
}

/// `routing.route`: config first, then the stdin request, so a broken `router.yaml` wins over a
/// broken request, as in Python.
fn route(raw: &str, limits: &Limits, sleep: &mut dyn FnMut(Duration)) -> Result<Value, String> {
  let home_dir = home::charliebot_home_dir().map_err(|error| error.to_string())?;
  route_in_home(&home_dir, raw, limits, sleep)
}

fn route_in_home(home: &Path, raw: &str, limits: &Limits, sleep: &mut dyn FnMut(Duration)) -> Result<Value, String> {
  let config = load_config(&config_path(home))?;
  let request = read_request(raw)?;
  let probability = design_probability(home, &config, &request.text, limits, sleep)?;
  let (backends, reason) = choose(&config, &request, probability);
  Ok(json!({ "backends": backends, "reason": reason }))
}

/// `<charliebot home>/router.yaml`, the router's own configuration file.
fn config_path(home: &Path) -> PathBuf {
  home.join(ROUTER_FILENAME)
}

/// One validated `router.yaml`: where Jev lives, the rule text, and the strong backends in order.
struct RouterConfig {
  url: String,
  model: String,
  credential: String,
  instructions: String,
  routine: String,
  design: String,
  strong: Vec<String>,
}

/// One decoded stdin object; a thread's text arrives already cut to [`THREAD_TEXT_LIMIT`].
struct RouteRequest {
  text: String,
  candidate_ids: Vec<String>,
}

fn field_error(path_text: &str, field: &str, expectation: &str) -> String {
  format!("{path_text}: {field} must be {expectation}")
}

fn required_text(mapping: &Mapping, field: &str, path_text: &str, prefix: &str) -> Result<String, String> {
  match mapping.get(YamlValue::String(field.to_string())) {
    Some(YamlValue::String(text)) if !text.trim().is_empty() => Ok(text.trim().to_string()),
    _ => Err(field_error(path_text, &format!("{prefix}{field}"), "a non-empty string")),
  }
}

fn strong_backends(mapping: &Mapping, path_text: &str) -> Result<Vec<String>, String> {
  let expectation = "a non-empty list of backend ids";
  let Some(YamlValue::Sequence(items)) = mapping.get(YamlValue::String("strong".to_string())) else {
    return Err(field_error(path_text, "strong", expectation));
  };
  if items.is_empty() {
    return Err(field_error(path_text, "strong", expectation));
  }
  let mut strong = Vec::new();
  for (index, backend) in items.iter().enumerate() {
    match backend {
      YamlValue::String(text) if !text.trim().is_empty() => strong.push(text.trim().to_string()),
      _ => return Err(field_error(path_text, &format!("strong[{index}]"), "a backend id")),
    }
  }
  Ok(strong)
}

/// Read and validate one `router.yaml`; a missing file or a malformed field names the file and the field.
fn load_config(path: &Path) -> Result<RouterConfig, String> {
  let path_text = paths::display(path);
  let text = match std::fs::read_to_string(path) {
    Ok(text) => text,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      return Err(format!("{path_text}: missing; the router reads its Jev address, rule text and strong list from it"));
    }
    Err(error) => return Err(format!("{path_text}: {error}")),
  };
  let document: YamlValue = serde_yaml::from_str(&text).map_err(|error| format!("{path_text}: {error}"))?;
  let expectation = "a mapping with jev, instructions, routine, design and strong";
  let Some(document) = document.as_mapping() else {
    return Err(field_error(&path_text, "the document", expectation));
  };
  let Some(YamlValue::Mapping(jev)) = document.get(YamlValue::String("jev".to_string())) else {
    return Err(field_error(&path_text, "jev", "a mapping"));
  };
  Ok(RouterConfig {
    url: required_text(jev, "url", &path_text, "jev.")?,
    model: required_text(jev, "model", &path_text, "jev.")?,
    credential: required_text(jev, "credential", &path_text, "jev.")?,
    instructions: required_text(document, "instructions", &path_text, "")?,
    routine: required_text(document, "routine", &path_text, "")?,
    design: required_text(document, "design", &path_text, "")?,
    strong: strong_backends(document, &path_text)?,
  })
}

/// Decode one stdin object; a thread's text keeps only its last [`THREAD_TEXT_LIMIT`] characters.
fn read_request(raw: &str) -> Result<RouteRequest, String> {
  let document: Value = serde_json::from_str(raw).map_err(|error| format!("stdin: not one JSON object: {error}"))?;
  let Some(document) = document.as_object() else {
    return Err("stdin: the request must be one JSON object".to_string());
  };
  let role = match document.get("role") {
    Some(Value::String(role)) if !role.trim().is_empty() => role.clone(),
    _ => return Err("stdin.role: must be a non-empty string".to_string()),
  };
  let text = match document.get("text") {
    Some(Value::String(text)) => text.clone(),
    _ => return Err("stdin.text: must be a string".to_string()),
  };
  let Some(Value::Array(candidates)) = document.get("candidates") else {
    return Err("stdin.candidates: must be a list of {id, cooling_until} objects".to_string());
  };
  let mut candidate_ids = Vec::new();
  for (index, candidate) in candidates.iter().enumerate() {
    let identifier = candidate.as_object().and_then(|candidate| candidate.get("id"));
    match identifier {
      Some(Value::String(identifier)) if !identifier.trim().is_empty() => candidate_ids.push(identifier.clone()),
      _ => return Err(format!("stdin.candidates[{index}].id: must be a non-empty string")),
    }
  }
  let text = if role == "thread" { last_chars(&text, THREAD_TEXT_LIMIT) } else { text };
  Ok(RouteRequest { text, candidate_ids })
}

/// `text[-limit:]` in characters, not bytes.
fn last_chars(text: &str, limit: usize) -> String {
  let count = text.chars().count();
  if count <= limit {
    return text.to_string();
  }
  text.chars().skip(count - limit).collect()
}

/// Jev's choice request: one question named `kind` with the routine and design criteria.
fn jev_body(config: &RouterConfig, text: &str) -> Value {
  json!({
    "model": config.model,
    "state": text,
    "questions": {
      "kind": {
        "type": "choice",
        "instructions": config.instructions,
        "criteria": { ROUTINE: config.routine, DESIGN: config.design },
      },
    },
  })
}

/// POST one Jev request, waiting out a single HTTP 429; every other failure is an error message.
fn post(
  url: &str,
  body: &Value,
  api_key: &str,
  limits: &Limits,
  sleep: &mut dyn FnMut(Duration),
) -> Result<Value, String> {
  charliebot_cli_common::http::ensure_crypto_provider();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| format!("{url}: request failed: {error}"))?;
  runtime.block_on(async {
    let client = reqwest::Client::builder()
      .timeout(limits.request_timeout)
      .build()
      .map_err(|error| format!("{url}: request failed: {error}"))?;
    let mut response = client
      .post(url)
      .bearer_auth(api_key)
      .json(body)
      .send()
      .await
      .map_err(|error| format!("{url}: request failed: {}", request_error(&error)))?;
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
      sleep(limits.rate_limit_wait);
      response = client
        .post(url)
        .bearer_auth(api_key)
        .json(body)
        .send()
        .await
        .map_err(|error| format!("{url}: request failed: {}", request_error(&error)))?;
    }
    let status = response.status().as_u16();
    let body_text = response.text().await.unwrap_or_default();
    if status != 200 {
      return Err(format!("{url}: HTTP {status}: {body_text}"));
    }
    let payload: Value =
      serde_json::from_str(&body_text).map_err(|_| format!("{url}: the response body is not JSON: {body_text}"))?;
    if !payload.is_object() {
      return Err(format!("{url}: the response is not one JSON object: {body_text}"));
    }
    Ok(payload)
  })
}

/// `str(httpx.HTTPError)` has no Rust twin; reqwest's Display carries the URL and the cause.
fn request_error(error: &reqwest::Error) -> String {
  error.to_string()
}

/// Python `json.dumps(payload)`: escaped non-ASCII, `", "` between items.
fn dumps_payload(payload: &Value) -> String {
  pyfmt::json_dumps(
    payload,
    &JsonStyle { ensure_ascii: true, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None },
  )
}

/// The `answers.kind.probabilities.design` number of one Jev response. A JSON boolean is not a number.
fn answer_design_probability(url: &str, payload: &Value) -> Result<f64, String> {
  let value = payload
    .get("answers")
    .and_then(|answer| answer.get("kind"))
    .and_then(|question| question.get("probabilities"))
    .and_then(|probabilities| probabilities.get(DESIGN));
  match value {
    Some(Value::Number(number)) => match number.as_f64() {
      Some(probability) => Ok(probability),
      None => {
        Err(format!("{url}: the answer has no answers.kind.probabilities.design number: {}", dumps_payload(payload)))
      }
    },
    _ => Err(format!("{url}: the answer has no answers.kind.probabilities.design number: {}", dumps_payload(payload))),
  }
}

/// Ask Jev once (one 429 retry) how likely *text* is design work.
fn design_probability(
  home: &Path,
  config: &RouterConfig,
  text: &str,
  limits: &Limits,
  sleep: &mut dyn FnMut(Duration),
) -> Result<f64, String> {
  let loaded = credentials::load_credentials(home).map_err(|error| error.to_string())?;
  let api_key = loaded.require(&config.credential, "api_key").map_err(|error| error.to_string())?;
  answer_design_probability(&config.url, &post(&config.url, &jev_body(config, text), &api_key, limits, sleep)?)
}

/// The backends this request gets, in order, and the one-line reason for them.
fn choose(config: &RouterConfig, request: &RouteRequest, probability: f64) -> (Vec<String>, String) {
  let reason = |kind: &str| format!("{kind} p={}", pyfmt::format_f(probability, 2));
  if probability <= DESIGN_THRESHOLD {
    return (Vec::new(), reason(ROUTINE));
  }
  let backends = config.strong.iter().filter(|backend| request.candidate_ids.contains(*backend)).cloned().collect();
  (backends, reason(DESIGN))
}

#[cfg(test)]
mod tests {
  use std::collections::VecDeque;
  use std::io::Write;
  use std::net::{TcpListener, TcpStream};
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::sync::{Arc, Mutex};

  use super::*;

  const INSTRUCTIONS: &str = "Which kind of work is this?";
  const ROUTINE_RULE: &str = "Routine work: fixes, tests and short actions.";
  const DESIGN_RULE: &str = "Design work: a new mechanism across modules.";
  const STRONG: [&str; 2] = ["strong-alpha", "strong-beta"];
  const POOL: &str = "pool-gamma";
  const API_KEY: &str = "test-key";

  /// The bounds the tests inject: a real attempt bound that still outruns the stub, and a wait no
  /// test sleeps out for real.
  fn test_limits() -> Limits {
    Limits { request_timeout: Duration::from_secs(2), rate_limit_wait: Duration::from_millis(20) }
  }

  /// Each test runs behind this ceiling so a hang fails the test instead of the suite.
  fn with_timeout<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let handle = std::thread::spawn(body);
    loop {
      if handle.is_finished() {
        return handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
      }
      if std::time::Instant::now() >= deadline {
        panic!("the test exceeded its 10 s timeout");
      }
      std::thread::sleep(Duration::from_millis(20));
    }
  }

  /// A Jev endpoint that records each request body and answers from a scripted reply list.
  struct FakeJev {
    url: String,
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
    authorizations: Arc<Mutex<Vec<String>>>,
    replies: Arc<Mutex<VecDeque<(u16, Value)>>>,
    stop: Arc<AtomicBool>,
    listener: TcpListener,
  }

  struct Shared {
    requests: Arc<Mutex<Vec<Value>>>,
    authorizations: Arc<Mutex<Vec<String>>>,
    replies: Arc<Mutex<VecDeque<(u16, Value)>>>,
  }

  impl FakeJev {
    fn start(replies: Vec<(u16, Value)>) -> FakeJev {
      let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
      let port = listener.local_addr().unwrap().port();
      let shared = Shared {
        requests: Arc::new(Mutex::new(Vec::new())),
        authorizations: Arc::new(Mutex::new(Vec::new())),
        replies: Arc::new(Mutex::new(replies.into())),
      };
      let accept = listener.try_clone().unwrap();
      let stop = Arc::new(AtomicBool::new(false));
      let thread_shared = Shared {
        requests: shared.requests.clone(),
        authorizations: shared.authorizations.clone(),
        replies: shared.replies.clone(),
      };
      let thread_stop = stop.clone();
      std::thread::spawn(move || {
        for stream in accept.incoming() {
          if thread_stop.load(Ordering::Relaxed) {
            break;
          }
          let Ok(stream) = stream else { break };
          serve(stream, &thread_shared);
        }
      });
      FakeJev {
        url: format!("http://127.0.0.1:{port}/v1/systemone"),
        port,
        requests: shared.requests,
        authorizations: shared.authorizations,
        replies: shared.replies,
        stop,
        listener,
      }
    }

    fn requests(&self) -> Vec<Value> {
      self.requests.lock().unwrap().clone()
    }

    fn authorizations(&self) -> Vec<String> {
      self.authorizations.lock().unwrap().clone()
    }

    /// The scripted replies not yet served: a test that expects a failure consumes none.
    fn unserved(&self) -> usize {
      self.replies.lock().unwrap().len()
    }
  }

  impl Drop for FakeJev {
    fn drop(&mut self) {
      // The accept loop holds its own listener clone, so closing alone would not end it: raise the
      // stop flag and open one throwaway connection to wake the blocked accept.
      self.stop.store(true, Ordering::Relaxed);
      let _ = TcpStream::connect(("127.0.0.1", self.port));
      drop(self.listener.try_clone().unwrap());
    }
  }

  /// One request = one connection: the stub answers with `Connection: close`.
  fn serve(mut stream: TcpStream, shared: &Shared) {
    let mut buffered = Vec::new();
    let header_end = loop {
      let mut chunk = [0u8; 4096];
      let Ok(read) = stream.read(&mut chunk) else { return };
      if read == 0 {
        return;
      }
      buffered.extend_from_slice(&chunk[..read]);
      if let Some(position) = buffered.windows(4).position(|window| window == b"\r\n\r\n") {
        break position + 4;
      }
    };
    let head = String::from_utf8_lossy(&buffered[..header_end]).into_owned();
    let content_length = head
      .lines()
      .find_map(|line| {
        let lowered = line.to_ascii_lowercase();
        lowered.strip_prefix("content-length:").and_then(|value| value.trim().parse::<usize>().ok())
      })
      .unwrap_or(0);
    let mut body = buffered[header_end..].to_vec();
    while body.len() < content_length {
      let mut chunk = [0u8; 4096];
      let Ok(read) = stream.read(&mut chunk) else { return };
      if read == 0 {
        break;
      }
      body.extend_from_slice(&chunk[..read]);
    }
    let request: Value = serde_json::from_slice(&body).expect("the router sends one JSON object");
    shared.requests.lock().unwrap().push(request);
    let authorization = head
      .lines()
      .find_map(|line| {
        if line.to_ascii_lowercase().starts_with("authorization:") {
          Some(line["authorization:".len()..].trim().to_string())
        } else {
          None
        }
      })
      .unwrap_or_default();
    shared.authorizations.lock().unwrap().push(authorization);
    let (status, reply) = shared.replies.lock().unwrap().pop_front().expect("the test scripted one reply per request");
    let reply_body = serde_json::to_vec(&reply).unwrap();
    let response = format!(
      "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      reply_body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&reply_body);
    let _ = stream.flush();
  }

  fn answer(design: f64) -> (u16, Value) {
    (
      200,
      json!({ "answers": { "kind": { "choice": "design", "probabilities": { "routine": 1.0 - design, "design": design } } } }),
    )
  }

  struct TestHome {
    home: PathBuf,
    _temp: tempfile::TempDir,
  }

  impl TestHome {
    fn new() -> TestHome {
      let temp = tempfile::tempdir().unwrap();
      TestHome { home: temp.path().to_path_buf(), _temp: temp }
    }
  }

  fn write_credentials(home: &TestHome, section: &str) {
    std::fs::write(home.home.join("credentials.yaml"), format!("{section}:\n  api_key: {API_KEY}\n")).unwrap();
  }

  fn write_router(home: &TestHome, url: &str, strong: &[&str]) {
    let strong_lines: Vec<String> = strong.iter().map(|backend| format!("  - {backend}\n")).collect();
    std::fs::write(
      home.home.join(ROUTER_FILENAME),
      format!(
        "jev:\n  url: {url}\n  model: jev-test\n  credential: jev\ninstructions: \"{INSTRUCTIONS}\"\nroutine: \"{ROUTINE_RULE}\"\ndesign: \"{DESIGN_RULE}\"\nstrong:\n{}",
        strong_lines.join("")
      ),
    )
    .unwrap();
  }

  fn request(role: &str, text: &str, candidates: &[&str]) -> String {
    let entries: Vec<Value> =
      candidates.iter().map(|identifier| json!({ "id": identifier, "cooling_until": null })).collect();
    serde_json::to_string(&json!({ "role": role, "text": text, "candidates": entries })).unwrap()
  }

  fn run(home: &TestHome, payload: &str, jev: &FakeJev, replies: Vec<(u16, Value)>) -> Result<Value, String> {
    jev.replies.lock().unwrap().extend(replies);
    let mut pauses = Vec::new();
    let result = route_in_home(&home.home, payload, &test_limits(), &mut |pause| pauses.push(pause));
    if result.is_ok() {
      assert_eq!(pauses, Vec::<Duration>::new(), "a success path sleeps no rate-limit wait");
    }
    result
  }

  #[test]
  fn design_work_returns_the_strong_backends_that_are_candidates_in_strong_order() {
    with_timeout(|| {
      let home = TestHome::new();
      write_credentials(&home, "jev");
      let jev = FakeJev::start(Vec::new());
      write_router(&home, &jev.url, &STRONG);

      let payload = request("implement", "Design a new caching layer.", &[STRONG[1], POOL, STRONG[0]]);
      let answer = run(&home, &payload, &jev, vec![answer(0.86)]).unwrap();

      assert_eq!(answer, json!({ "backends": ["strong-alpha", "strong-beta"], "reason": "design p=0.86" }));
      let requests = jev.requests();
      assert_eq!(requests.len(), 1);
      let body = &requests[0];
      assert_eq!(body["model"], "jev-test");
      assert_eq!(body["state"], "Design a new caching layer.");
      assert_eq!(body["questions"]["kind"]["type"], "choice");
      assert_eq!(body["questions"]["kind"]["instructions"], INSTRUCTIONS);
      assert_eq!(body["questions"]["kind"]["criteria"], json!({ "routine": ROUTINE_RULE, "design": DESIGN_RULE }));
      assert_eq!(jev.authorizations(), vec![format!("Bearer {API_KEY}")]);
    });
  }

  #[test]
  fn routine_work_returns_no_backends() {
    with_timeout(|| {
      let home = TestHome::new();
      write_credentials(&home, "jev");
      let jev = FakeJev::start(Vec::new());
      write_router(&home, &jev.url, &STRONG);

      let payload = request("implement", "Add a test for the parser.", &[STRONG[0], POOL]);
      let answer = run(&home, &payload, &jev, vec![answer(0.12)]).unwrap();

      assert_eq!(answer, json!({ "backends": [], "reason": "routine p=0.12" }));
    });
  }

  #[test]
  fn one_rate_limit_waits_then_returns_the_answer() {
    with_timeout(|| {
      let home = TestHome::new();
      write_credentials(&home, "jev");
      let jev = FakeJev::start(Vec::new());
      write_router(&home, &jev.url, &STRONG);
      let limits = test_limits();

      let payload = request("implement", "Add a test for the parser.", &STRONG);
      jev.replies.lock().unwrap().extend(vec![(429, json!({ "error": "rate limited" })), answer(0.86)]);
      let pauses = Arc::new(Mutex::new(Vec::new()));
      let recorded = pauses.clone();
      let result = route_in_home(&home.home, &payload, &limits, &mut |pause| recorded.lock().unwrap().push(pause));

      assert_eq!(result.unwrap(), json!({ "backends": ["strong-alpha", "strong-beta"], "reason": "design p=0.86" }));
      assert_eq!(jev.requests().len(), 2);
      assert_eq!(*pauses.lock().unwrap(), vec![limits.rate_limit_wait]);
    });
  }

  #[test]
  fn two_rate_limits_exit_non_zero_with_the_status_and_body() {
    with_timeout(|| {
      let home = TestHome::new();
      write_credentials(&home, "jev");
      let jev = FakeJev::start(Vec::new());
      write_router(&home, &jev.url, &STRONG);

      let payload = request("implement", "Add a test for the parser.", &STRONG);
      let error = run(
        &home,
        &payload,
        &jev,
        vec![(429, json!({ "error": "rate limited" })), (429, json!({ "error": "still limited" }))],
      )
      .unwrap_err();

      assert!(error.contains("429"), "{error}");
      assert!(error.contains("still limited"), "{error}");
      assert_eq!(jev.unserved(), 0);
    });
  }

  #[test]
  fn another_http_error_exits_non_zero_without_printing_an_answer() {
    with_timeout(|| {
      let home = TestHome::new();
      write_credentials(&home, "jev");
      let jev = FakeJev::start(Vec::new());
      write_router(&home, &jev.url, &STRONG);

      let payload = request("implement", "Add a test for the parser.", &STRONG);
      let error = run(&home, &payload, &jev, vec![(500, json!({ "error": "gateway down" }))]).unwrap_err();

      assert!(error.contains("500"), "{error}");
      assert!(error.contains("gateway down"), "{error}");
    });
  }

  #[test]
  fn an_answer_without_a_design_probability_exits_non_zero() {
    with_timeout(|| {
      let home = TestHome::new();
      write_credentials(&home, "jev");
      let jev = FakeJev::start(Vec::new());
      write_router(&home, &jev.url, &STRONG);

      let payload = request("implement", "Add a test for the parser.", &STRONG);
      let error = run(&home, &payload, &jev, vec![(200, json!({ "answers": { "other": {} } }))]).unwrap_err();

      assert!(error.contains("the answer has no answers.kind.probabilities.design number"), "{error}");
    });
  }

  #[test]
  fn a_thread_text_is_cut_to_its_last_twenty_thousand_characters() {
    with_timeout(|| {
      let home = TestHome::new();
      write_credentials(&home, "jev");
      let jev = FakeJev::start(Vec::new());
      write_router(&home, &jev.url, &STRONG);
      let long_text = format!("{}THREAD-END", "x".repeat(25_000));

      let thread_payload = request("thread", &long_text, &STRONG);
      run(&home, &thread_payload, &jev, vec![answer(0.12)]).unwrap();
      let implement_payload = request("implement", &long_text, &STRONG);
      run(&home, &implement_payload, &jev, vec![answer(0.12)]).unwrap();

      let requests = jev.requests();
      let expected_tail: String = long_text.chars().skip(25_010 - THREAD_TEXT_LIMIT).collect();
      assert_eq!(requests[0]["state"], expected_tail);
      assert_eq!(requests[0]["state"].as_str().unwrap().chars().count(), THREAD_TEXT_LIMIT);
      assert_eq!(requests[1]["state"], long_text);
    });
  }

  #[test]
  fn an_edit_to_router_yaml_takes_effect_on_the_next_call() {
    with_timeout(|| {
      let home = TestHome::new();
      write_credentials(&home, "jev");
      let jev = FakeJev::start(Vec::new());
      write_router(&home, &jev.url, &[STRONG[0]]);

      let payload = request("implement", "Add a test for the parser.", &STRONG);
      let first = run(&home, &payload, &jev, vec![answer(0.86)]).unwrap();
      assert_eq!(first["backends"], json!(["strong-alpha"]));

      write_router(&home, &jev.url, &[STRONG[1]]);
      let second = run(&home, &payload, &jev, vec![answer(0.86)]).unwrap();
      assert_eq!(second["backends"], json!(["strong-beta"]));
    });
  }

  #[test]
  fn a_malformed_router_yaml_exits_non_zero_naming_the_file_and_field() {
    with_timeout(|| {
      let home = TestHome::new();
      write_credentials(&home, "jev");
      let path = home.home.join(ROUTER_FILENAME);
      std::fs::write(&path, "jev:\n  model: jev-test\n  credential: jev\nstrong:\n  - strong-alpha\n").unwrap();

      let payload = request("implement", "Add a test for the parser.", &STRONG);
      let error = route_in_home(&home.home, &payload, &test_limits(), &mut |_| {}).unwrap_err();

      assert!(error.contains(&paths::display(&path)), "{error}");
      assert!(error.contains("jev.url"), "{error}");
    });
  }

  #[test]
  fn router_yaml_that_is_not_yaml_exits_non_zero_naming_the_file() {
    with_timeout(|| {
      let home = TestHome::new();
      write_credentials(&home, "jev");
      let path = home.home.join(ROUTER_FILENAME);
      std::fs::write(&path, "jev: [unclosed\n").unwrap();

      let payload = request("implement", "Add a test for the parser.", &STRONG);
      let error = route_in_home(&home.home, &payload, &test_limits(), &mut |_| {}).unwrap_err();

      assert!(error.contains(&paths::display(&path)), "{error}");
    });
  }

  #[test]
  fn a_missing_router_yaml_exits_non_zero_naming_the_file() {
    with_timeout(|| {
      let home = TestHome::new();
      let path = home.home.join(ROUTER_FILENAME);

      let payload = request("implement", "Add a test for the parser.", &STRONG);
      let error = route_in_home(&home.home, &payload, &test_limits(), &mut |_| {}).unwrap_err();

      assert!(error.contains(&paths::display(&path)), "{error}");
      assert!(error.contains("missing"), "{error}");
    });
  }

  #[test]
  fn a_missing_jev_key_exits_non_zero_naming_the_credentials_file() {
    with_timeout(|| {
      let home = TestHome::new();
      write_credentials(&home, "another-section");
      let jev = FakeJev::start(Vec::new());
      write_router(&home, &jev.url, &STRONG);

      let payload = request("implement", "Add a test for the parser.", &STRONG);
      let error = route_in_home(&home.home, &payload, &test_limits(), &mut |_| {}).unwrap_err();

      assert!(error.contains("credentials.yaml"), "{error}");
      assert!(error.contains("jev.api_key"), "{error}");
      assert!(jev.requests().is_empty(), "no request reaches Jev without the key");
    });
  }

  #[test]
  fn the_worst_case_stays_inside_the_callers_twenty_five_seconds() {
    let worst_case = 2 * timeouts::ROUTER_JEV_REQUEST_TIMEOUT + timeouts::ROUTER_JEV_RATE_LIMIT_WAIT;
    assert!(worst_case < timeouts::ROUTER_TIMEOUT_SECONDS);
    assert_eq!(timeouts::ROUTER_JEV_REQUEST_TIMEOUT, 5);
  }
}
