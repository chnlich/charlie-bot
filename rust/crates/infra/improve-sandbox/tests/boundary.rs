//! The boundary's permission tests: a write inside the round's allow-list succeeds, and a write outside it fails with a
//! permission error. A signal to a process outside the round fails with EPERM and leaves the process alive, while a
//! signal to the round's own child succeeds. The program runs with a scratch HOME, so every allow-list path resolves
//! inside it. The tests need a kernel that enforces Landlock ABI 6 and fail on one that does not, so a host without the
//! boundary is never reported as passing. The refusal of a direction without a clone needs no boundary: the program
//! refuses before it starts one, so that test runs on every kernel.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The kernel release and the active security modules, so a failure names the kernel it ran on.
fn kernel_summary() -> String {
  let release = fs::read_to_string("/proc/sys/kernel/osrelease").expect("the kernel release");
  let lsm = fs::read_to_string("/sys/kernel/security/lsm").expect("the active security modules");
  format!("kernel {}; LSM {}", release.trim(), lsm.trim())
}

/// Removes the two `/tmp` directories that the program creates for `direction`, when they exist.
fn remove_tmp_directories(direction: &str) {
  for dir in charliebot_improve_sandbox::tmp_directories(direction) {
    if dir.exists() {
      fs::remove_dir_all(&dir).expect("the test's own /tmp directory");
    }
  }
}

/// The scratch HOME of one direction's round. The round's protocol makes the clone before the round starts, so the
/// scratch HOME holds one too: the program refuses a direction without its clone.
fn scratch_home_with_clone(direction: &str) -> tempfile::TempDir {
  let home = tempfile::tempdir_in("/tmp").expect("the scratch HOME");
  fs::create_dir_all(home.path().join("improve").join(direction).join("charlie-bot")).expect("the direction clone");
  home
}

#[ignore = "integration"]
#[test]
fn a_write_inside_the_allow_list_succeeds_and_one_outside_fails_with_a_permission_error() {
  // The canary must sit outside the allow-list. The allow-list opens all of /dev/shm, where a build host's TMPDIR can
  // live, and leaves /tmp closed outside the direction's own two directories.
  // A direction of this test only, so the `/tmp` directories that the program creates are this test's own: the tests
  // run in parallel, and two of them that shared a direction would remove each other's directories.
  let direction = format!("allowlist{}", std::process::id());
  let home = scratch_home_with_clone(&direction);
  fs::create_dir_all(home.path().join("canary-outside")).expect("the canary directory");
  let script = format!(
    "printf inside >\"$HOME/improve/{direction}/evidence/inside.txt\"; \
     printf clone >\"$HOME/improve/{direction}/charlie-bot/inside.txt\"; \
     printf outside >\"$HOME/canary-outside/outside.txt\""
  );

  let output = Command::new(env!("CARGO_BIN_EXE_improve-sandbox"))
    .env_clear()
    .env("HOME", home.path())
    .env("PATH", "/usr/bin:/bin")
    .args([direction.as_str(), "--", "sh", "-c", script.as_str()])
    .output()
    .expect("the sandbox starts");
  remove_tmp_directories(&direction);

  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  let context = format!("{}; stderr: {stderr}", kernel_summary());
  let inside = home.path().join("improve").join(&direction).join("evidence/inside.txt");
  let clone_inside = home.path().join("improve").join(&direction).join("charlie-bot/inside.txt");
  assert_eq!(
    fs::read_to_string(&inside).ok().as_deref(),
    Some("inside"),
    "the write inside the allow-list failed: {context}"
  );
  assert_eq!(
    fs::read_to_string(&clone_inside).ok().as_deref(),
    Some("clone"),
    "the write inside the direction clone failed: {context}"
  );
  assert!(
    !home.path().join("canary-outside/outside.txt").exists(),
    "the write outside the allow-list succeeded: {context}"
  );
  assert!(stderr.contains("Permission denied"), "the denied write named no permission error: {context}");
  assert!(!output.status.success(), "the command succeeded although its outside write failed: {context}");
}

#[ignore = "integration"]
#[test]
fn a_signal_to_a_process_outside_the_round_fails_with_eperm_and_one_to_the_rounds_own_child_succeeds() {
  // The process outside the round is a sleep that this test starts, so only the signal scope stands between the
  // sandboxed shell and it. The round's own child is a sleep that the sandboxed shell starts.
  // A direction of this test only, so the `/tmp` directories that the program creates are this test's own.
  let direction = format!("signal{}", std::process::id());
  let home = scratch_home_with_clone(&direction);
  let mut outside = Command::new("sleep").arg("60").spawn().expect("the process outside the round");
  let outside_pid = outside.id();
  let script = format!(
    "if kill -TERM {outside_pid}; then echo outside-signalled; else echo outside-refused; fi; \
     if kill -0 {outside_pid}; then echo outside-probe-alive; else echo outside-probe-refused; fi; \
     sleep 60 & own=$!; \
     if kill -TERM \"$own\"; then echo own-signalled; fi; \
     wait \"$own\"; echo own-exit=$?"
  );

  let output = Command::new(env!("CARGO_BIN_EXE_improve-sandbox"))
    .env_clear()
    .env("HOME", home.path())
    .env("PATH", "/usr/bin:/bin")
    .args([direction.as_str(), "--", "sh", "-c", script.as_str()])
    .output()
    .expect("the sandbox starts");
  remove_tmp_directories(&direction);
  let outside_alive = outside.try_wait().expect("the outside process's state").is_none();
  outside.kill().expect("stop the outside process");
  outside.wait().expect("reap the outside process");

  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  let context = format!("{}; stdout: {stdout}; stderr: {stderr}", kernel_summary());
  let has_line = |line: &str| stdout.lines().any(|found| found == line);
  assert!(output.status.success(), "the sandboxed shell failed: {context}");
  assert!(outside_alive, "a signal from the round reached the process outside it: {context}");
  assert!(has_line("outside-refused"), "the signal to the outside process was not refused: {context}");
  assert!(has_line("outside-probe-refused"), "the liveness probe of the outside process was not refused: {context}");
  assert!(stderr.contains("Operation not permitted"), "the refused signal named no EPERM: {context}");
  assert!(has_line("own-signalled"), "the signal to the round's own child failed: {context}");
  assert!(has_line("own-exit=143"), "the round's own child did not end by SIGTERM: {context}");
}

#[ignore = "integration"]
#[test]
fn a_write_under_the_golden_scratch_directory_succeeds_and_a_neighbouring_path_fails() {
  // `golden_cases` writes under the fixed `/tmp/charliebot-golden`. The neighbour is a sibling of that directory, not a
  // child, so the allow-list must refuse it. The neighbour carries this process's id, so a concurrent run keeps its own.
  // A direction of this test only, so the `/tmp` directories that the program creates are this test's own.
  let direction = format!("golden{}", std::process::id());
  let home = scratch_home_with_clone(&direction);
  let scratch = Path::new("/tmp/charliebot-golden").join(&direction);
  let neighbour = PathBuf::from(format!("/tmp/charliebot-golden-other-{}", std::process::id()));
  fs::create_dir_all(&neighbour).expect("the neighbouring directory");
  let script = format!(
    "mkdir -p \"{scratch}\" && printf inside >\"{scratch}/inside.txt\"; printf outside >\"{neighbour}/outside.txt\"",
    scratch = scratch.display(),
    neighbour = neighbour.display(),
  );

  let output = Command::new(env!("CARGO_BIN_EXE_improve-sandbox"))
    .env_clear()
    .env("HOME", home.path())
    .env("PATH", "/usr/bin:/bin")
    .args([direction.as_str(), "--", "sh", "-c", script.as_str()])
    .output()
    .expect("the sandbox starts");
  remove_tmp_directories(&direction);
  let inside_text = fs::read_to_string(scratch.join("inside.txt")).ok();
  let outside_written = neighbour.join("outside.txt").exists();
  for own in [&scratch, &neighbour] {
    if own.exists() {
      fs::remove_dir_all(own).expect("the test's own directory");
    }
  }

  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  let context = format!("{}; stderr: {stderr}", kernel_summary());
  assert_eq!(inside_text.as_deref(), Some("inside"), "the write under the golden scratch directory failed: {context}");
  assert!(!outside_written, "the write to the neighbouring path succeeded: {context}");
  assert!(stderr.contains("Permission denied"), "the refused write named no permission error: {context}");
  assert!(!output.status.success(), "the command succeeded although its neighbouring write failed: {context}");
}

/// A direction without a clone is refused before any command runs. The refusal names the missing clone, and the program
/// creates no clone path. The refusal starts no boundary, so this test runs on every kernel.
#[test]
fn a_direction_without_a_clone_is_refused_before_any_command_and_creates_no_clone() {
  let home = tempfile::tempdir_in("/tmp").expect("the scratch HOME");
  // A direction of this test only, so the `/tmp` directories that the program would create are this test's own.
  let direction = format!("noclone{}", std::process::id());
  let clone = home.path().join("improve").join(&direction).join("charlie-bot");
  let marker = home.path().join("command-ran");
  let script = format!("touch \"{}\"", marker.display());

  let output = Command::new(env!("CARGO_BIN_EXE_improve-sandbox"))
    .env_clear()
    .env("HOME", home.path())
    .env("PATH", "/usr/bin:/bin")
    .args([direction.as_str(), "--", "sh", "-c", script.as_str()])
    .output()
    .expect("the sandbox starts");
  remove_tmp_directories(&direction);

  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(!output.status.success(), "the sandbox accepted a direction without a clone: stderr: {stderr}");
  assert!(stderr.contains(&clone.display().to_string()), "the refusal named no clone path: stderr: {stderr}");
  assert!(!marker.exists(), "the command ran although the clone is missing: stderr: {stderr}");
  assert!(!clone.exists(), "the sandbox created the clone path: stderr: {stderr}");
}
