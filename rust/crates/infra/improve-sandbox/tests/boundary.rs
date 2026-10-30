//! The boundary's permission test: a write inside the round's allow-list succeeds, and a write outside it fails with a
//! permission error. The program runs with a scratch HOME, so every allow-list path resolves inside it. The test needs
//! a kernel that enforces Landlock and fails on one that does not, so a host without the boundary is never reported
//! as passing.

use std::fs;
use std::path::Path;
use std::process::Command;

/// The kernel release and the active security modules, so a failure names the kernel it ran on.
fn kernel_summary() -> String {
  let release = fs::read_to_string("/proc/sys/kernel/osrelease").expect("the kernel release");
  let lsm = fs::read_to_string("/sys/kernel/security/lsm").expect("the active security modules");
  format!("kernel {}; LSM {}", release.trim(), lsm.trim())
}

/// Removes the two `/tmp` directories that the program creates for `direction`, when they exist.
fn remove_tmp_directories(direction: &str) {
  for dir in [format!("/tmp/improve-{direction}"), format!("/tmp/improve-{direction}-tmp")] {
    if Path::new(&dir).exists() {
      fs::remove_dir_all(&dir).expect("the test's own /tmp directory");
    }
  }
}

#[test]
fn a_write_inside_the_allow_list_succeeds_and_one_outside_fails_with_a_permission_error() {
  let home = tempfile::tempdir().expect("the scratch HOME");
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
