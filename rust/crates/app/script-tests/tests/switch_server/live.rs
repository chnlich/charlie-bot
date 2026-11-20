//! The live capture step of the switch: the `View:` trailers it reads from the commits the switch moved across, the exit
//! the switch reports when a capture fails, and where the access key goes during one capture.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use charliebot_shell_harness::{self as harness, Sandbox};

use crate::drain_line;
use crate::world::World;

#[test]
#[ignore = "integration"]
fn test_view_trailers_names_each_view_of_each_commit_in_the_range() {
  let world = World::new();
  let before = world.git(&world.clone, &["rev-parse", "HEAD"]);
  world.git(&world.clone, &["commit", "-q", "--allow-empty", "-m", "Open the status panel\n\nView: /?session=abc"]);
  let first = world.git(&world.clone, &["rev-parse", "--short", "HEAD"]);
  world.git(
    &world.clone,
    &["commit", "-q", "--allow-empty", "-m", "Add the plan tray\n\nView: /?session=abc\nView: /plans"],
  );
  let second = world.git(&world.clone, &["rev-parse", "--short", "HEAD"]);

  let run = world.clone_command(&format!("view_trailers {before} HEAD"));

  assert_eq!(run.code, 0, "{}", run.describe());
  // `git rev-list` lists the newest commit first, and each commit's View lines follow its sha.
  assert_eq!(run.stdout.trim(), format!("{second} /?session=abc\n{second} /plans\n{first} /?session=abc"));
}

#[test]
#[ignore = "integration"]
fn test_a_commit_without_a_view_trailer_captures_nothing() {
  let world = World::new();
  let before = world.git(&world.clone, &["rev-parse", "HEAD"]);
  world.git(&world.clone, &["commit", "-q", "--allow-empty", "-m", "No page in this change"]);

  let run = world.clone_command(&format!("view_trailers {before} HEAD"));

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.trim().is_empty(), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn test_a_view_the_live_server_does_not_resolve_fails_its_capture_before_any_picture() {
  let world = World::new();
  let before = world.git(&world.clone, &["rev-parse", "HEAD"]);
  world.git(&world.clone, &["commit", "-q", "--allow-empty", "-m", "Open the session\n\nView: /?session=5f1c0d2e"]);
  let short = world.git(&world.clone, &["rev-parse", "--short", "HEAD"]);
  let head = world.git(&world.clone, &["rev-parse", "HEAD"]);

  let (run, captured) = world.live_capture(&before, &head, "/?session=known-session");

  assert_eq!(run.code, 1, "{}", world.report(&run));
  assert!(run.stdout.contains(&format!("live screenshot: commit {short}: ")), "{}", world.report(&run));
  assert!(run.stdout.contains("View line \"/?session=5f1c0d2e\""), "{}", world.report(&run));
  assert!(!captured, "a view that does not resolve is not captured: {}", world.report(&run));
}

#[test]
#[ignore = "integration"]
fn test_a_view_the_live_server_resolves_is_captured() {
  let world = World::new();
  let before = world.git(&world.clone, &["rev-parse", "HEAD"]);
  world.git(&world.clone, &["commit", "-q", "--allow-empty", "-m", "Open the session\n\nView: /?session=5f1c0d2e"]);
  let head = world.git(&world.clone, &["rev-parse", "HEAD"]);

  let (run, captured) = world.live_capture(&before, &head, "/?session=5f1c0d2e");

  assert_eq!(run.code, 0, "{}", world.report(&run));
  assert!(captured, "{}", world.report(&run));
  assert!(run.stdout.contains("view /?session=5f1c0d2e size 1536×900"), "{}", world.report(&run));
}

#[test]
#[ignore = "integration"]
fn test_a_failed_live_capture_exits_7_and_keeps_the_new_release_serving() {
  let mut world = World::new();
  world.seed_live_run("run-live", Some("claude-opus"));
  world.record_outcome("run-live", "success");
  let pid = world.live[0].id();

  let run = world.switch_forward_with(pid, &drain_line("run-live"), ":", "return 1");

  assert_eq!(run.code, 7, "{}", world.report(&run));
  assert!(!world.root().join("restored").exists(), "the switch did not switch back: {}", world.report(&run));
  assert!(run.stdout.contains("live screenshot capture failed"), "{}", world.report(&run));
  assert!(!run.stdout.contains("SWITCH COMPLETE:"), "{}", world.report(&run));
}

const LIVE_KEY: &str = "live-shot-test-key";
const VIEW: &str = "/?session=abc";

/// One request an origin received: its request target, its raw head, and the status it answered.
#[derive(Clone, Debug)]
struct Seen {
  target: String,
  head: String,
  status: u16,
}

/// The status, content type and body an origin answers one request with.
type Answer = (u16, &'static str, String);

/// An origin's answer to one request, given its request target and its raw head.
type Respond = Arc<dyn Fn(&str, &str) -> Answer + Send + Sync>;

/// Serves the requests of `listener` through `respond`, recording each one. Each connection gets its own thread, so an
/// idle connection that Chrome opens early does not hold up the others. An `OPTIONS` preflight is answered with 204.
fn stand_in_origin(listener: TcpListener, respond: Respond) -> Arc<Mutex<Vec<Seen>>> {
  let seen = Arc::new(Mutex::new(Vec::new()));
  let log = Arc::clone(&seen);
  thread::spawn(move || {
    for stream in listener.incoming() {
      let stream = stream.expect("a connection");
      let (log, respond) = (Arc::clone(&log), Arc::clone(&respond));
      thread::spawn(move || answer(stream, &log, respond.as_ref()));
    }
  });
  seen
}

fn answer(mut stream: TcpStream, seen: &Mutex<Vec<Seen>>, respond: &dyn Fn(&str, &str) -> Answer) {
  stream.set_read_timeout(Some(Duration::from_secs(10))).expect("a read timeout");
  let head = read_head(&mut stream);
  let mut words = head.split_whitespace();
  let method = words.next().unwrap_or_default();
  let target = words.next().unwrap_or_default();
  if method.is_empty() {
    return;
  }
  let (status, content_type, body) =
    if method == "OPTIONS" { (204, "text/plain", String::new()) } else { respond(target, &head) };
  seen.lock().expect("the request log").push(Seen { target: target.to_string(), head, status });
  let response = format!(
    "HTTP/1.1 {status} stand-in\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
     Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Authorization\r\nConnection: close\r\n\r\n{body}",
    body.len()
  );
  if let Err(error) = stream.write_all(response.as_bytes()) {
    eprintln!("stand-in origin: writing a response: {error}");
  }
}

/// The request head: the bytes up to the blank line that ends it.
fn read_head(stream: &mut TcpStream) -> String {
  let mut head = Vec::new();
  let mut byte = [0u8];
  while !head.ends_with(b"\r\n\r\n") {
    match stream.read(&mut byte) {
      Ok(0) => break,
      Ok(_) => head.push(byte[0]),
      Err(error) => {
        eprintln!("stand-in origin: reading a request: {error}");
        break;
      }
    }
  }
  String::from_utf8_lossy(&head).into_owned()
}

/// The value of each header that the head carries under `name`, in request order.
fn header_values<'a>(head: &'a str, name: &str) -> Vec<&'a str> {
  head
    .lines()
    .filter_map(|line| {
      let (field, value) = line.split_once(':')?;
      field.trim().eq_ignore_ascii_case(name).then_some(value.trim())
    })
    .collect()
}

/// Whether a request head carries the key as its bearer token or as the access-key cookie.
fn carries_key(head: &str) -> bool {
  let bearer = format!("Bearer {LIVE_KEY}");
  let cookie = format!("charliebot_access_key={LIVE_KEY}");
  header_values(head, "Authorization").contains(&bearer.as_str())
    || header_values(head, "Cookie").iter().any(|value| value.split(';').any(|pair| pair.trim() == cookie))
}

/// CharlieBot: the public root, and a view answered with 200 only to a request that carries the key. The view's page
/// loads a script from the second origin, and its own fetch to that origin sends the key from localStorage as an
/// Authorization header, the way the web client's fetch wrapper does for every fetch.
fn charliebot(second_port: u16) -> Respond {
  Arc::new(move |target, head| {
    if target == "/" {
      return (200, "text/html", "<!doctype html><title>CharlieBot</title>".to_string());
    }
    if !carries_key(head) {
      return (401, "text/html", "<!doctype html><title>CharlieBot login</title>".to_string());
    }
    let page = format!(
      "<!doctype html><title>CharlieBot</title>\
       <script src=\"http://127.0.0.1:{second_port}/asset.js\"></script>\
       <script>fetch('http://127.0.0.1:{second_port}/api', {{ headers: {{ \
       Authorization: 'Bearer ' + localStorage.getItem('charliebot_access_key') }} }});</script>"
    );
    (200, "text/html", page)
  })
}

/// The second origin: a script and an API route that the view's page reaches.
fn second_origin() -> Respond {
  Arc::new(|target, _head| match target {
    "/asset.js" => (200, "text/javascript", "window.stand_in_asset = true;".to_string()),
    "/api" => (200, "text/plain", "ok".to_string()),
    _ => (404, "text/plain", String::new()),
  })
}

#[test]
#[ignore = "integration"]
fn test_the_capture_sends_the_key_to_the_charliebot_origin_only() {
  let sandbox = Sandbox::new();
  let second_listener = TcpListener::bind("127.0.0.1:0").expect("a port");
  let second_port = second_listener.local_addr().expect("the port").port();
  let second_seen = stand_in_origin(second_listener, second_origin());
  let first_listener = TcpListener::bind("127.0.0.1:0").expect("a port");
  let first_port = first_listener.local_addr().expect("the port").port();
  let first_seen = stand_in_origin(first_listener, charliebot(second_port));
  let out = sandbox.dir("shots").join("view.png");

  let mut env = sandbox.env();
  env.insert("CHARLIEBOT_ACCESS_KEY".to_string(), LIVE_KEY.to_string());
  let mut command = harness::command("node", &env);
  command
    .arg(harness::repo_root().join("scripts/live-shot.mjs"))
    .arg(format!("http://127.0.0.1:{first_port}"))
    .arg(VIEW)
    .args(["800", "600"])
    .arg(&out);
  let run = harness::run(&mut command);

  assert_eq!(run.code, 0, "{}", run.describe());
  let first = first_seen.lock().expect("the first origin's log").clone();
  let second = second_seen.lock().expect("the second origin's log").clone();
  // The view answers 200 only to a request that carries the key, so 200 is the logged-in render.
  assert!(
    first.iter().any(|seen| seen.target == VIEW && seen.status == 200),
    "the view did not render logged in: {first:?}"
  );
  // The second origin served the script and the fetch, so the checks below cover requests the view made.
  assert!(second.iter().any(|seen| seen.target == "/asset.js"), "the view loaded no script: {second:?}");
  assert!(second.iter().any(|seen| seen.target == "/api"), "the view's fetch never arrived: {second:?}");
  for seen in &second {
    assert!(header_values(&seen.head, "Authorization").is_empty(), "the second origin got an Authorization: {seen:?}");
    assert!(!seen.head.contains(LIVE_KEY), "the second origin got the key: {seen:?}");
  }
}
