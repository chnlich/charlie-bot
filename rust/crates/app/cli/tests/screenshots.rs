//! The `charliebot screenshots` process tests. Each runs the built `charliebot` on a synthetic evidence directory and
//! feeds the changed paths on stdin.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use charliebot_tasks::web_screenshots::WINDOW_SIZES;
use tempfile::TempDir;

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

fn check(dir: &Path, changed: &str) -> Output {
  let mut child = Command::new(env!("CARGO_BIN_EXE_charliebot"))
    .args(["screenshots", "check", "--dir"])
    .arg(dir)
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
  let output = check(&missing, "rust/crates/x/src/lib.rs\nprompts/master.md\nweb/tests/a.test.js\nweb/package.json\n");

  assert_eq!(output.status.code(), Some(0), "{}", text(&output));
  assert!(String::from_utf8_lossy(&output.stdout).is_empty());
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_with_two_screenshots_of_each_size_passes_and_names_the_change() {
  let dir = evidence_dir(2);
  let output = check(dir.path(), "web/static/app.js\n");

  assert_eq!(output.status.code(), Some(0), "{}", text(&output));
  assert!(String::from_utf8_lossy(&output.stdout).contains("web page change: 1 path(s)"), "{}", text(&output));
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_with_one_screenshot_of_each_size_exits_1() {
  let dir = evidence_dir(1);
  let output = check(dir.path(), "web/static/app.js\n");

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
  let output = check(dir.path(), "prompts/plan_template.html\n");

  assert_eq!(output.status.code(), Some(1), "{}", text(&output));
  let printed = text(&output);
  assert!(printed.contains(&format!("{width}x{}", height * 2)), "{printed}");
  assert!(printed.contains(&format!("{width}x{height}")), "{printed}");
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
