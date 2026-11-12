//! End-to-end `charliebot router`: the built binary runs with a temp `CHARLIEBOT_HOME`, real stdin and a fake Jev on a
//! free loopback port. Its stdout, stderr and exit code are the contract.
//!
//! The per-failure details are unit-tested in `src/commands/router.rs`. This target proves the dispatcher wiring: one
//! success prints only the one-line JSON answer, and `--config` reads the file it names. One failure prints
//! `router: <message>` on stderr, nothing on stdout, and exits 1.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const API_KEY: &str = "test-key";
const STRONG: [&str; 2] = ["strong-alpha", "strong-beta"];
const POOL: &str = "pool-gamma";

/// The whole test runs behind this ceiling so a hang fails the test instead of the suite.
fn with_timeout<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
  let deadline = Instant::now() + Duration::from_secs(15);
  let handle = std::thread::spawn(body);
  loop {
    if handle.is_finished() {
      return handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    }
    if Instant::now() >= deadline {
      panic!("the test exceeded its 15 s timeout");
    }
    std::thread::sleep(Duration::from_millis(20));
  }
}

/// A minimal Jev endpoint: records each request body and answers from a scripted reply list.
struct FakeJev {
  url: String,
  port: u16,
  requests: Arc<Mutex<Vec<Value>>>,
  authorizations: Arc<Mutex<Vec<String>>>,
  stop: Arc<AtomicBool>,
}

impl FakeJev {
  fn start(replies: Vec<(u16, Value)>) -> FakeJev {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let authorizations: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let accept = listener.try_clone().unwrap();
    let thread_replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let thread_requests = requests.clone();
    let thread_authorizations = authorizations.clone();
    let thread_stop = stop.clone();
    std::thread::spawn(move || {
      for stream in accept.incoming() {
        if thread_stop.load(Ordering::Relaxed) {
          break;
        }
        let Ok(stream) = stream else { break };
        serve(stream, &thread_requests, &thread_authorizations, &thread_replies);
      }
    });
    FakeJev { url: format!("http://127.0.0.1:{port}/v1/systemone"), port, requests, authorizations, stop }
  }

  fn requests(&self) -> Vec<Value> {
    self.requests.lock().unwrap().clone()
  }
}

impl Drop for FakeJev {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Relaxed);
    let _ = TcpStream::connect(("127.0.0.1", self.port));
  }
}

/// One request = one connection: the stub answers with `Connection: close`.
fn serve(
  mut stream: TcpStream,
  requests: &Mutex<Vec<Value>>,
  authorizations: &Mutex<Vec<String>>,
  replies: &Mutex<VecDeque<(u16, Value)>>,
) {
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
  requests.lock().unwrap().push(serde_json::from_slice(&body).expect("the router sends one JSON object"));
  authorizations.lock().unwrap().push(
    head
      .lines()
      .find_map(|line| {
        if line.to_ascii_lowercase().starts_with("authorization:") {
          Some(line["authorization:".len()..].trim().to_string())
        } else {
          None
        }
      })
      .unwrap_or_default(),
  );
  let (status, reply) = replies.lock().unwrap().pop_front().expect("the test scripted one reply per request");
  let reply_body = serde_json::to_vec(&reply).unwrap();
  let response = format!(
    "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    reply_body.len()
  );
  let _ = stream.write_all(response.as_bytes());
  let _ = stream.write_all(&reply_body);
  let _ = stream.flush();
}

/// The rules file of the example's shape: routine runs on the pool, context on the session.
fn rules_text(url: &str) -> String {
  format!(
    "jev:\n  url: {url}\n  model: jev-test\n  credential: jev\n\
     instructions: \"Which class of work is this?\"\n\
     classes:\n\
     \x20 routine: {{when: \"Routine work: fixes, tests and short actions.\", run_on: {POOL}}}\n\
     \x20 context: {{when: \"Context class: a first read or a design.\", run_on: session}}\n\
     roles:\n\
     \x20 implement: [routine, context]\n"
  )
}

/// The live `config.yaml`: the backend options the rules may name, and the rotate pool `POOL` over them.
fn write_config(home: &Path) {
  let options: String =
    STRONG.iter().map(|id| format!("    - {{id: {id}, label: {id}, type: codex, model: synthetic-model}}\n")).collect();
  let members = STRONG.join(", ");
  let pools = format!("  pools:\n    - {{id: {POOL}, label: {POOL}, rule: rotate, members: [{members}]}}\n");
  std::fs::write(home.join("config.yaml"), format!("backends:\n  options:\n{options}{pools}")).unwrap();
}

/// Run the built binary once with `router` and `arguments` as argv; returns `(exit code, stdout, stderr)`.
fn run_binary(home: &Path, arguments: &[&str], stdin_text: &str) -> (i32, String, String) {
  let mut child = Command::new(env!("CARGO_BIN_EXE_charliebot"))
    .arg("router")
    .args(arguments)
    .env("CHARLIEBOT_HOME", home)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("the built charliebot binary starts");
  child.stdin.take().expect("the router reads stdin").write_all(stdin_text.as_bytes()).unwrap();
  let output = child.wait_with_output().unwrap();
  (
    output.status.code().expect("the router exits by code"),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

#[test]
#[ignore = "integration"]
fn router_end_to_end_prints_one_answer_and_reports_failures_through_the_built_binary() {
  with_timeout(|| {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    std::fs::write(home.join("credentials.yaml"), format!("jev:\n  api_key: {API_KEY}\n")).unwrap();
    write_config(home);
    let answer = (
      200,
      json!({ "answers": { "kind": { "choice": "context", "probabilities": { "routine": 0.14, "context": 0.86 } } } }),
    );
    let jev = FakeJev::start(vec![answer.clone(), answer]);
    std::fs::write(home.join("router.yaml"), rules_text(&jev.url)).unwrap();
    let stdin = json!({
      "role": "implement",
      "text": "Understand the caching layer.",
      "candidates": [{ "id": STRONG[0], "cooling_until": null }, { "id": POOL, "cooling_until": null }],
      "session_backend": STRONG[0],
    })
    .to_string();

    let (code, stdout, stderr) = run_binary(home, &[], &stdin);

    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
      stdout,
      format!("{{\"routed\": true, \"backends\": [\"{}\"], \"reason\": \"context p=0.86\"}}\n", STRONG[0])
    );
    assert_eq!(stderr, "");
    let requests = jev.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["model"], "jev-test");
    assert_eq!(requests[0]["state"], "Task type: implement\n\nUnderstand the caching layer.");
    assert_eq!(jev.authorizations.lock().unwrap().as_slice(), &[format!("Bearer {API_KEY}")]);

    // `--config` reads the file it names, even while the home's router.yaml is broken.
    std::fs::write(home.join("router.yaml"), "jev: [unclosed\n").unwrap();
    let calibration = temp.path().join("calibration.yaml");
    std::fs::write(&calibration, rules_text(&jev.url)).unwrap();
    let calibration_text = calibration.display().to_string();
    let (code, stdout, stderr) = run_binary(home, &["--config", &calibration_text], &stdin);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("\"routed\": true"), "{stdout}");

    // One failure through the same binary: stdout stays empty and stderr carries the message.
    let (code, stdout, stderr) = run_binary(home, &[], &stdin);

    assert_eq!(code, 1);
    assert_eq!(stdout, "");
    assert!(stderr.starts_with("router: "), "{stderr}");
    assert!(stderr.contains("router.yaml"), "{stderr}");
  });
}
