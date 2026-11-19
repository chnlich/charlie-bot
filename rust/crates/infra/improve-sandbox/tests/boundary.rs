//! The boundary's permission tests: a write inside the round's allow-list succeeds, and a write outside it fails with a
//! permission error. A signal to a process outside the round fails with EPERM and leaves the process alive, while a
//! signal to the round's own child succeeds. The program runs with a scratch HOME, so every allow-list path resolves
//! inside it. The tests need a kernel that enforces Landlock ABI 6 and fail on one that does not, so a host without the
//! boundary is never reported as passing.

use std::fs;
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

#[ignore = "integration"]
#[test]
fn a_write_inside_the_allow_list_succeeds_and_one_outside_fails_with_a_permission_error() {
  // The canary must sit outside the allow-list. The allow-list opens all of /dev/shm, where a build host's TMPDIR can
  // live, and leaves /tmp closed outside the direction's own two directories.
  let home = tempfile::tempdir_in("/tmp").expect("the scratch HOME");
  // A direction of this process only, so the `/tmp` directories that the program creates are this test's own.
  let direction = format!("probe{}", std::process::id());
  fs::create_dir_all(home.path().join("canary-outside")).expect("the canary directory");
  let script = format!(
    "printf inside >\"$HOME/improve/{direction}/evidence/inside.txt\"; \
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
  assert_eq!(
    fs::read_to_string(&inside).ok().as_deref(),
    Some("inside"),
    "the write inside the allow-list failed: {context}"
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
  let home = tempfile::tempdir_in("/tmp").expect("the scratch HOME");
  // A direction of this process only, so the `/tmp` directories that the program creates are this test's own.
  let direction = format!("probe{}", std::process::id());
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
