//! The `charliebot screenshots` process tests. Each runs the built `charliebot` on a synthetic evidence directory and
//! feeds the changed paths on stdin.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use charliebot_tasks::web_screenshots::WINDOW_SIZES;
use serde_json::json;
use tempfile::TempDir;

#[path = "common/stand_in.rs"]
mod stand_in;

/// Writes a PNG whose IHDR chunk carries `width` and `height`, which is all the check reads.
fn write_png(dir: &Path, name: &str, width: u32, height: u32) {
  let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
  bytes.extend_from_slice(&13u32.to_be_bytes());
  bytes.extend_from_slice(b"IHDR");
  bytes.extend_from_slice(&width.to_be_bytes());
  bytes.extend_from_slice(&height.to_be_bytes());
  fs::write(dir.join(name), bytes).expect("the screenshot is written");
}

/// One directory holding `count` PNGs at each window size.
fn evidence_dir(count: usize) -> TempDir {
  let dir = tempfile::tempdir().expect("a temporary directory");
  for (index, (width, height)) in WINDOW_SIZES.iter().enumerate() {
    for copy in 0..count {
      write_png(dir.path(), &format!("shot-{index}-{copy}.png"), *width, *height);
    }
  }
  dir
}

/// The names `evidence_dir(count)` writes, in its order.
fn shot_names(count: usize) -> Vec<String> {
  (0..WINDOW_SIZES.len()).flat_map(|index| (0..count).map(move |copy| format!("shot-{index}-{copy}.png"))).collect()
}

/// A review directory that holds a PR body naming each of `named` and a review record whose review read each of
/// `reads`. The record's reads are the `Read` calls that the check counts.
fn review_of(named: &[String], reads: &[String]) -> TempDir {
  let dir = tempfile::tempdir().expect("a temporary directory");
  let body: String = named.iter().map(|name| format!("- {name}\n")).collect();
  fs::write(dir.path().join("pr-body.md"), body).expect("the PR body is written");
  let mut lines = Vec::new();
  for (index, name) in reads.iter().enumerate() {
    let id = format!("toolu_{index}");
    let call = json!({"type": "assistant", "message": {"content": [
      {"type": "tool_use", "id": id.clone(), "name": "Read", "input": {"file_path": format!("/evidence/{name}")}}
    ]}});
    let result = json!({"type": "user", "message": {"content": [
      {"type": "tool_result", "tool_use_id": id, "is_error": false, "content": "ok"}
    ]}});
    lines.push(call.to_string());
    lines.push(result.to_string());
  }
  fs::write(dir.path().join("events.ndjson"), lines.join("\n") + "\n").expect("the review record is written");
  dir
}

/// Runs `screenshots check` on the changed paths; with `review`, it names that directory's PR body and review record.
fn check(dir: &Path, changed: &str, review: Option<&Path>) -> Output {
  let mut command = Command::new(env!("CARGO_BIN_EXE_charliebot"));
  command.args(["screenshots", "check", "--dir"]).arg(dir);
  if let Some(review) = review {
    command.arg("--pr-body").arg(review.join("pr-body.md")).arg("--review-record").arg(review.join("events.ndjson"));
  }
  let mut child = command
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("charliebot screenshots check runs");
  std::io::Write::write_all(child.stdin.as_mut().expect("piped stdin"), changed.as_bytes())
    .expect("the changed paths are written");
  child.wait_with_output().expect("the check finishes")
}

fn text(output: &Output) -> String {
  format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

#[test]
#[ignore = "integration"]
fn a_change_that_touches_no_web_page_passes_without_a_directory() {
  let missing = PathBuf::from("/nonexistent/evidence");
  let output =
    check(&missing, "rust/crates/x/src/lib.rs\nprompts/master.md\nweb/tests/a.test.js\nweb/package.json\n", None);

  assert_eq!(output.status.code(), Some(0), "{}", text(&output));
  assert!(String::from_utf8_lossy(&output.stdout).is_empty());
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_whose_review_read_every_named_screenshot_passes_and_names_the_change() {
  let dir = evidence_dir(2);
  let names = shot_names(2);
  let review = review_of(&names, &names);
  let output = check(dir.path(), "web/static/app.js\n", Some(review.path()));

  assert_eq!(output.status.code(), Some(0), "{}", text(&output));
  assert!(String::from_utf8_lossy(&output.stdout).contains("web page change: 1 path(s)"), "{}", text(&output));
}

#[test]
#[ignore = "integration"]
fn a_rust_page_source_is_a_web_page_change_that_names_itself() {
  let dir = evidence_dir(2);
  let names = shot_names(2);
  let review = review_of(&names, &names);
  let output = check(dir.path(), "rust/crates/features/files/src/error_page.rs\n", Some(review.path()));

  assert_eq!(output.status.code(), Some(0), "{}", text(&output));
  assert!(String::from_utf8_lossy(&output.stdout).contains("web page change: 1 path(s)"), "{}", text(&output));
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_whose_review_missed_a_named_screenshot_exits_1() {
  let dir = evidence_dir(2);
  let names = shot_names(2);
  let review = review_of(&names, &names[1..]);
  let output = check(dir.path(), "web/static/app.js\n", Some(review.path()));

  assert_eq!(output.status.code(), Some(1), "{}", text(&output));
  assert!(text(&output).contains("shows no read of shot-0-0.png"), "{}", text(&output));
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_with_one_screenshot_of_each_size_exits_1() {
  let dir = evidence_dir(1);
  let output = check(dir.path(), "web/static/app.js\n", None);

  assert_eq!(output.status.code(), Some(1), "{}", text(&output));
  assert!(text(&output).contains("1536x900"), "{}", text(&output));
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_with_a_screenshot_of_the_wrong_height_exits_1() {
  let dir = evidence_dir(2);
  let (width, height) = WINDOW_SIZES[2];
  write_png(dir.path(), "wrong.png", width, height * 2);
  fs::remove_file(dir.path().join("shot-2-0.png")).expect("one real phone screenshot is removed");
  let output = check(dir.path(), "prompts/plan_template.html\n", None);

  assert_eq!(output.status.code(), Some(1), "{}", text(&output));
  let printed = text(&output);
  assert!(printed.contains(&format!("{width}x{}", height * 2)), "{printed}");
  assert!(printed.contains(&format!("{width}x{height}")), "{printed}");
}

#[test]
#[ignore = "integration"]
fn resolve_asks_the_live_server_with_its_credential_for_each_session_view() {
  let home = tempfile::tempdir().expect("a temporary profile home");
  // The stand-in knows session `known` only, and returns each request's header lines, which carry the credential.
  let (port, server) = stand_in::serve(2, |path| if path == "/api/sessions/known" { 200 } else { 404 });
  fs::write(home.path().join("config.yaml"), format!("server:\n  port: {port}\n")).expect("the config is written");
  fs::write(home.path().join("credentials.yaml"), "charliebot:\n  access_key: test-key\n")
    .expect("the credentials are written");
  let resolve = |view: &str| {
    Command::new(env!("CARGO_BIN_EXE_charliebot"))
      .env("CHARLIEBOT_HOME", home.path())
      .env_remove("CHARLIEBOT_RUN_TOKEN")
      .env_remove("IMPROVE_LAUNCH_DIR")
      .args(["screenshots", "resolve", view])
      .output()
      .expect("charliebot screenshots resolve runs")
  };

  let known = resolve("/?session=known");
  let unknown = resolve("/?session=unknown");

  assert_eq!(known.status.code(), Some(0), "{}", text(&known));
  assert_eq!(unknown.status.code(), Some(1), "{}", text(&unknown));
  assert!(text(&unknown).contains("the live server does not know session unknown"), "{}", text(&unknown));
  for request in server.join().expect("the stand-in server finishes") {
    assert!(request.iter().any(|line| line.eq_ignore_ascii_case("authorization: Bearer test-key")), "{request:?}");
  }
}

#[test]
#[ignore = "integration"]
fn sizes_prints_every_window_size() {
  let output = Command::new(env!("CARGO_BIN_EXE_charliebot"))
    .args(["screenshots", "sizes"])
    .output()
    .expect("charliebot screenshots sizes runs");

  assert_eq!(output.status.code(), Some(0));
  let expected: Vec<String> = WINDOW_SIZES.iter().map(|(width, height)| format!("{width}x{height}")).collect();
  assert_eq!(String::from_utf8_lossy(&output.stdout), format!("{}\n", expected.join("\n")));
}
