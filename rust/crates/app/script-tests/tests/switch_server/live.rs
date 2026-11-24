//! The live capture step of the switch: the `View:` trailers it reads from the commits the switch moved across, the exit
//! the switch reports when a capture fails, and where the access key goes during one capture.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use charliebot_shell_harness::{self as harness, Sandbox};

use crate::drain_line;
use crate::world::{World, install_release};

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

/// The switch's own capture step, run for real against a target whose View line the live server does not know: the
/// capture fails before any picture, the log names the commit and the line, and the switch exits 7 on the new release.
#[test]
#[ignore = "integration"]
fn test_a_view_the_live_server_does_not_know_exits_7_through_the_switch_and_stays_switched() {
  let mut world = World::new();
  world.seed_live_run("run-live", Some("claude-opus"));
  world.record_outcome("run-live", "success");
  let pid = world.live[0].id();
  world.git(&world.clone, &["commit", "-q", "--allow-empty", "-m", "Open the session\n\nView: /?session=5f1c0d2e"]);
  let target = world.git(&world.clone, &["rev-parse", "HEAD"]);
  let short = world.git(&world.clone, &["rev-parse", "--short", "HEAD"]);
  install_release(&world.releases, &target);

  let run = world.switch_forward_capturing(&target, pid, &drain_line("run-live"), "/?session=known-session");

  assert_eq!(run.code, 7, "{}", world.report(&run));
  assert!(!world.root().join("restored").exists(), "the switch did not switch back: {}", world.report(&run));
  let current = fs::read_link(world.releases.join("current")).expect("current names a release");
  assert_eq!(current, Path::new(&target), "the new release stays switched: {}", world.report(&run));
  assert!(run.stdout.contains(&format!("live screenshot: commit {short}: ")), "{}", world.report(&run));
  assert!(run.stdout.contains("View line `/?session=5f1c0d2e`"), "{}", world.report(&run));
  assert!(run.stdout.contains("live screenshot capture failed"), "{}", world.report(&run));
  assert!(!run.stdout.contains("SWITCH COMPLETE:"), "{}", world.report(&run));
}

fn commit_with_file_and_views(world: &World, message: &str, views: &[&str]) -> (String, String) {
  let file = world.clone.join("web_fixture.txt");
  fs::write(&file, format!("{message}\n")).expect("write fixture");
  world.git(&world.clone, &["add", "web_fixture.txt"]);
  let mut full_message = message.to_string();
  full_message.push_str("\n\n");
  for view in views {
    full_message.push_str(&format!("View: {view}\n"));
  }
  world.git(&world.clone, &["commit", "-q", "-m", &full_message]);
  let target = world.git(&world.clone, &["rev-parse", "HEAD"]);
  let short = world.git(&world.clone, &["rev-parse", "--short", "HEAD"]);
  install_release(&world.releases, &target);
  (target, short)
}

fn setup_judge_world() -> (World, u32) {
  let mut world = World::new();
  world.seed_live_run("run-live", Some("claude-opus"));
  world.record_outcome("run-live", "success");
  let pid = world.live[0].id();
  (world, pid)
}

#[test]
#[ignore = "integration"]
fn test_judge_not_visible_exits_7_logs_commit_view_reason_and_keeps_new_release() {
  let (world, pid) = setup_judge_world();
  let (target, short) = commit_with_file_and_views(&world, "Wrap diff code on phones", &["/diff"]);

  let run = world.switch_forward_live(
    &target,
    pid,
    &drain_line("run-live"),
    "/diff",
    &[("FAKE_JUDGE_STDOUT", "VERDICT /diff not-visible All three screenshots show an empty Diff Viewer form.\n")],
  );

  assert_eq!(run.code, 7, "{}", world.report(&run));
  assert!(!world.root().join("restored").exists(), "the switch did not switch back: {}", world.report(&run));
  let current = fs::read_link(world.releases.join("current")).expect("current names a release");
  assert_eq!(current, Path::new(&target), "the new release stays switched: {}", world.report(&run));
  assert!(
    run.stdout.contains(&format!(
      "live view check: commit {short} view /diff not-visible: All three screenshots show an empty Diff Viewer form."
    )),
    "{}",
    world.report(&run)
  );
  assert!(!run.stdout.contains("SWITCH COMPLETE:"), "{}", world.report(&run));
}

#[test]
#[ignore = "integration"]
fn test_judge_all_views_shows_exits_0_and_logs_one_line_per_view() {
  let (world, pid) = setup_judge_world();
  let (target, short) = commit_with_file_and_views(&world, "Update status and plans", &["/?session=abc", "/plans"]);

  let run = world.switch_forward_live(
    &target,
    pid,
    &drain_line("run-live"),
    "/?session=abc\n/plans",
    &[(
      "FAKE_JUDGE_STDOUT",
      "VERDICT /?session=abc shows The status panel shows the updated badge.\n\
       VERDICT /plans shows The plan table renders the new column.\n",
    )],
  );

  assert_eq!(run.code, 0, "{}", world.report(&run));
  assert!(
    run.stdout.contains(&format!(
      "live view check: commit {short} view /?session=abc shows: The status panel shows the updated badge."
    )),
    "{}",
    world.report(&run)
  );
  assert!(
    run
      .stdout
      .contains(&format!("live view check: commit {short} view /plans shows: The plan table renders the new column.")),
    "{}",
    world.report(&run)
  );
  assert!(run.stdout.contains("SWITCH COMPLETE:"), "{}", world.report(&run));
}

/// The run token of a run that has finished. The stand-in server refuses it, and a switch that a trigger watch starts
/// inherits it from the agent turn that registered the watch.
const FINISHED_RUN_TOKEN: &str = "run-token-of-a-finished-run";

/// One switch to a commit whose `/diff` View the judge shows, with `extra` added to the switch's environment. The judge
/// answers only after its option resolves on the server, so a verdict in the log means the call got through.
fn switch_judge_shows(extra: &[(&str, &str)]) {
  let (world, pid) = setup_judge_world();
  let (target, short) = commit_with_file_and_views(&world, "Show the diff on phones", &["/diff"]);
  let mut judge_env = vec![("FAKE_JUDGE_STDOUT", "VERDICT /diff shows The diff viewer renders the commit's change.\n")];
  judge_env.extend_from_slice(extra);

  let run = world.switch_forward_live(&target, pid, &drain_line("run-live"), "/diff", &judge_env);

  assert_eq!(run.code, 0, "{}", world.report(&run));
  assert!(
    run.stdout.contains(&format!(
      "live view check: commit {short} view /diff shows: The diff viewer renders the commit's change."
    )),
    "{}",
    world.report(&run)
  );
  assert!(run.stdout.contains("SWITCH COMPLETE:"), "{}", world.report(&run));
}

/// The switch inherits a run token that the server refuses. The judge call must authenticate with the access key
/// instead, so it still yields its verdict.
#[test]
#[ignore = "integration"]
fn test_judge_runs_on_the_access_key_when_the_switch_inherits_a_refused_run_token() {
  switch_judge_shows(&[("CHARLIEBOT_RUN_TOKEN", FINISHED_RUN_TOKEN)]);
}

/// A switch from a plain shell carries no run token, and its judge call runs on the access key the same way.
#[test]
#[ignore = "integration"]
fn test_judge_runs_on_the_access_key_from_a_shell_without_a_run_token() {
  switch_judge_shows(&[]);
}

#[test]
#[ignore = "integration"]
fn test_judge_broken_verdict_exits_7_and_logs_reason() {
  let (world, pid) = setup_judge_world();
  let (target, short) = commit_with_file_and_views(&world, "Render plans page", &["/plans"]);

  let run = world.switch_forward_live(
    &target,
    pid,
    &drain_line("run-live"),
    "/plans",
    &[("FAKE_JUDGE_STDOUT", "VERDICT /plans broken The page rendered a blank 500 error screen.\n")],
  );

  assert_eq!(run.code, 7, "{}", world.report(&run));
  assert!(!world.root().join("restored").exists(), "{}", world.report(&run));
  assert!(
    run.stdout.contains(&format!(
      "live view check: commit {short} view /plans broken: The page rendered a blank 500 error screen."
    )),
    "{}",
    world.report(&run)
  );
}

#[test]
#[ignore = "integration"]
fn test_judge_missing_verdict_exits_7_and_logs_missing() {
  let (world, pid) = setup_judge_world();
  let (target, short) = commit_with_file_and_views(&world, "Update two views", &["/?session=abc", "/plans"]);

  let run = world.switch_forward_live(
    &target,
    pid,
    &drain_line("run-live"),
    "/?session=abc\n/plans",
    &[("FAKE_JUDGE_STDOUT", "VERDICT /?session=abc shows The panel shows the change.\n")],
  );

  assert_eq!(run.code, 7, "{}", world.report(&run));
  assert!(!world.root().join("restored").exists(), "{}", world.report(&run));
  assert!(
    run.stdout.contains(&format!("live view check: commit {short} view /plans missing verdict")),
    "{}",
    world.report(&run)
  );
}

#[test]
#[ignore = "integration"]
fn test_judge_unrecognized_verdict_exits_7_and_logs_unrecognized() {
  let (world, pid) = setup_judge_world();
  let (target, short) = commit_with_file_and_views(&world, "Update diff header", &["/diff"]);

  let run = world.switch_forward_live(
    &target,
    pid,
    &drain_line("run-live"),
    "/diff",
    &[("FAKE_JUDGE_STDOUT", "VERDICT /diff maybe Could not tell from screenshot.\n")],
  );

  assert_eq!(run.code, 7, "{}", world.report(&run));
  assert!(!world.root().join("restored").exists(), "{}", world.report(&run));
  assert!(
    run.stdout.contains(&format!("live view check: commit {short} view /diff unrecognized verdict 'maybe'")),
    "{}",
    world.report(&run)
  );
}

#[test]
#[ignore = "integration"]
fn test_judge_backend_run_failure_exits_7_and_logs_error() {
  let (world, pid) = setup_judge_world();
  let (target, short) = commit_with_file_and_views(&world, "Update diff header", &["/diff"]);

  let run = world.switch_forward_live(
    &target,
    pid,
    &drain_line("run-live"),
    "/diff",
    &[("FAKE_JUDGE_EXIT_CODE", "1"), ("FAKE_JUDGE_STDERR", "gateway returned 503 overloaded")],
  );

  assert_eq!(run.code, 7, "{}", world.report(&run));
  assert!(!world.root().join("restored").exists(), "{}", world.report(&run));
  assert!(run.stdout.contains(&format!("live view check: commit {short}: judge failed:")), "{}", world.report(&run));
  assert!(run.stdout.contains("gateway returned 503 overloaded"), "{}", world.report(&run));
}

#[test]
#[ignore = "integration"]
fn test_judge_unset_calls_config_exits_7_and_logs_missing_key() {
  let (world, pid) = setup_judge_world();
  let (target, short) = commit_with_file_and_views(&world, "Update diff header", &["/diff"]);

  let run =
    world.switch_forward_live(&target, pid, &drain_line("run-live"), "/diff", &[("FAKE_JUDGE_CHECK_CONFIG", "1")]);

  assert_eq!(run.code, 7, "{}", world.report(&run));
  assert!(!world.root().join("restored").exists(), "{}", world.report(&run));
  assert!(run.stdout.contains(&format!("live view check: commit {short}: judge failed:")), "{}", world.report(&run));
  assert!(run.stdout.contains("backends.calls.judge"), "{}", world.report(&run));
}

#[test]
#[ignore = "integration"]
fn test_judge_verdict_with_leading_text_on_same_line_is_accepted() {
  let (world, pid) = setup_judge_world();
  let (target, short) = commit_with_file_and_views(&world, "Adjust badge color", &["/?session=abc"]);

  let run = world.switch_forward_live(
    &target,
    pid,
    &drain_line("run-live"),
    "/?session=abc",
    &[(
      "FAKE_JUDGE_STDOUT",
      "I need to read the screenshots.VERDICT /?session=abc shows The badge uses the new color.\n",
    )],
  );

  assert_eq!(run.code, 0, "{}", world.report(&run));
  assert!(
    run
      .stdout
      .contains(&format!("live view check: commit {short} view /?session=abc shows: The badge uses the new color.")),
    "{}",
    world.report(&run)
  );
}

#[test]
#[ignore = "integration"]
fn test_judge_task_file_and_working_dir_include_commit_diff_views_and_pngs() {
  let (world, pid) = setup_judge_world();
  let (target, short) = commit_with_file_and_views(&world, "Style the status header", &["/?session=abc", "/plans"]);
  let record_dir = world.root().join("judge-record");
  let record_dir_str = record_dir.display().to_string();

  let run = world.switch_forward_live(
    &target,
    pid,
    &drain_line("run-live"),
    "/?session=abc\n/plans",
    &[
      ("FAKE_SCREENSHOT_SIZES", "1536x900 390x844"),
      ("FAKE_JUDGE_RECORD_DIR", &record_dir_str),
      (
        "FAKE_JUDGE_STDOUT",
        "VERDICT /?session=abc shows Status header matches.\nVERDICT /plans shows Plans view unaffected.\n",
      ),
    ],
  );

  assert_eq!(run.code, 0, "{}", world.report(&run));
  let shot_dir = world.logs.join("live-screenshots/switch_test").join(&short);
  let recorded_cwd = fs::read_to_string(record_dir.join("cwd.txt")).expect("cwd.txt");
  assert_eq!(Path::new(recorded_cwd.trim()), shot_dir.as_path());

  let args = fs::read_to_string(record_dir.join("args.txt")).expect("args.txt");
  let arg_lines: Vec<&str> = args.lines().collect();
  assert!(arg_lines.windows(2).any(|w| w == ["--call", "judge"]), "{args}");
  assert!(arg_lines.contains(&"--task-file"), "{args}");
  assert!(!arg_lines.contains(&"--one-shot"), "never pass --one-shot: {args}");

  assert!(shot_dir.join("task.md").is_file());
  assert!(shot_dir.join("answer.txt").is_file());
  assert!(shot_dir.join("events.ndjson").is_file());

  let task_md = fs::read_to_string(record_dir.join("task.md")).expect("task.md");
  assert!(task_md.contains("# Live view check"), "{task_md}");
  assert!(task_md.contains("Style the status header"), "{task_md}");
  assert!(task_md.contains("web_fixture.txt | 1 +"), "{task_md}");
  assert!(task_md.contains("diff --git a/web_fixture.txt b/web_fixture.txt"), "{task_md}");
  assert!(task_md.contains("- View: /?session=abc"), "{task_md}");
  assert!(task_md.contains("- View: /plans"), "{task_md}");

  let cwd_entries = fs::read_to_string(record_dir.join("cwd_entries.txt")).expect("cwd_entries.txt");
  let pngs: Vec<&str> = cwd_entries.lines().filter(|name| name.ends_with(".png")).collect();
  assert_eq!(pngs.len(), 4, "expected 2 views x 2 sizes = 4 PNGs: {cwd_entries}");
  for png in pngs {
    assert!(task_md.contains(png), "task.md missing PNG {png}: {task_md}");
  }
}

#[test]
#[ignore = "integration"]
fn test_two_views_sharing_first_80_chars_produce_distinct_png_filenames() {
  let (world, pid) = setup_judge_world();
  let prefix = "/diff?repo=~/workspace/charlie-bot&base=0123456789abcdef0123456789abcdef01234567&";
  assert!(prefix.len() > 80);
  let view_a = format!("{prefix}head=aaaaaaa");
  let view_b = format!("{prefix}head=bbbbbbb");
  let (target, short) = commit_with_file_and_views(&world, "Two long diff views", &[&view_a, &view_b]);
  let known = format!("{view_a}\n{view_b}");
  let stdout = format!("VERDICT {view_a} shows View A rendered.\nVERDICT {view_b} shows View B rendered.\n");

  let run = world.switch_forward_live(&target, pid, &drain_line("run-live"), &known, &[("FAKE_JUDGE_STDOUT", &stdout)]);

  assert_eq!(run.code, 0, "{}", world.report(&run));
  let shot_dir = world.logs.join("live-screenshots/switch_test").join(&short);
  let pngs: Vec<String> = fs::read_dir(&shot_dir)
    .expect("shot dir")
    .filter_map(|entry| {
      let name = entry.expect("entry").file_name().to_string_lossy().into_owned();
      name.ends_with(".png").then_some(name)
    })
    .collect();
  assert_eq!(pngs.len(), 2, "two distinct PNGs expected for two long views: {pngs:?}");
  assert_ne!(pngs[0], pngs[1]);
}
