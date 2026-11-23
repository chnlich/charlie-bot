//! The agent ssh guard script, run for real against a stub ssh and the real ssh's configuration readback.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The guard options the script prepends, in the order it passes them.
const GUARD_OPTIONS: [&str; 6] = ["-o", "BatchMode=yes", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"];

/// A ControlMaster argv. The guard prepends BatchMode=yes and the two keepalive options (ssh_guard/ssh), so the
/// BatchMode=no and ServerAliveInterval=0 here lose to them. The guard leaves ControlPersist=8h as the caller set it.
const UNGUARDED_MASTER_ARGV: [&str; 9] = [
  "-M",
  "-S",
  "/tmp/ssh-guard-test-control-socket",
  "-o",
  "ControlPersist=8h",
  "-o",
  "ServerAliveInterval=0",
  "-o",
  "BatchMode=no",
];

fn checkout() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("the crate lives under the checkout").to_path_buf()
}

fn guard_dir() -> PathBuf {
  checkout().join("rust/crates/runtime/agents/ssh_guard")
}

/// A PATH with the guard directory first and the test process's own PATH after it.
fn guarded_path(guard: &Path, stub: Option<&Path>) -> String {
  let inherited = env::var("PATH").expect("the test process has a PATH");
  match stub {
    Some(stub) => format!("{}:{}:{inherited}", guard.display(), stub.display()),
    None => format!("{}:{inherited}", guard.display()),
  }
}

/// A stub `ssh` that writes its arguments, one per line, to `record`.
fn write_stub(directory: &Path, record: &Path) {
  fs::create_dir_all(directory).unwrap();
  let script = directory.join("ssh");
  fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", record.display())).unwrap();
  fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
#[ignore = "integration"]
fn guard_prepends_the_three_options_before_the_callers_own() {
  let root = tempfile::tempdir().unwrap();
  let stub_dir = root.path().join("stub");
  let record = root.path().join("argv");
  write_stub(&stub_dir, &record);

  let status = Command::new(guard_dir().join("ssh"))
    .env("PATH", guarded_path(&guard_dir(), Some(&stub_dir)))
    .args(UNGUARDED_MASTER_ARGV)
    .arg("host1")
    .status()
    .unwrap();

  assert!(status.success(), "the guard runs the stub: {status}");
  let received: Vec<String> = fs::read_to_string(&record).unwrap().lines().map(str::to_string).collect();
  let expected: Vec<&str> =
    GUARD_OPTIONS.iter().chain(UNGUARDED_MASTER_ARGV.iter()).chain(["host1"].iter()).copied().collect();
  assert_eq!(received, expected);
}

/// The guard's string removal misses a PATH entry that spells the guard directory through a symlink, so the lookup
/// skips its own directory by physical path. Without that, the guard execs itself forever.
#[test]
#[ignore = "integration"]
fn symlinked_guard_path_entry_still_runs_the_real_ssh() {
  let root = tempfile::tempdir().unwrap();
  let stub_dir = root.path().join("stub");
  let record = root.path().join("argv");
  write_stub(&stub_dir, &record);
  let linked_guard = root.path().join("guard-link");
  std::os::unix::fs::symlink(guard_dir(), &linked_guard).unwrap();

  let mut child = Command::new(linked_guard.join("ssh"))
    .env("PATH", guarded_path(&linked_guard, Some(&stub_dir)))
    .args(UNGUARDED_MASTER_ARGV)
    .arg("host1")
    .spawn()
    .unwrap();
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
  let status = loop {
    if let Some(status) = child.try_wait().unwrap() {
      break status;
    }
    if std::time::Instant::now() >= deadline {
      child.kill().unwrap();
      child.wait().unwrap();
      panic!("the guard never reached the stub through a symlinked PATH entry; it execs itself");
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
  };

  assert!(status.success(), "the guard runs the stub: {status}");
  let received: Vec<String> = fs::read_to_string(&record).unwrap().lines().map(str::to_string).collect();
  let expected: Vec<&str> =
    GUARD_OPTIONS.iter().chain(UNGUARDED_MASTER_ARGV.iter()).chain(["host1"].iter()).copied().collect();
  assert_eq!(received, expected);
}

#[test]
#[ignore = "integration"]
fn ssh_config_readback_through_the_guard_shows_the_guard_values_first() {
  let output = Command::new(guard_dir().join("ssh"))
    .env("PATH", guarded_path(&guard_dir(), None))
    .args(["-G", "-F", "/dev/null"])
    .args(UNGUARDED_MASTER_ARGV)
    .arg("host1")
    .output()
    .unwrap();

  assert!(output.status.success(), "ssh -G reads the configuration: {}", String::from_utf8_lossy(&output.stderr));
  let readback = String::from_utf8(output.stdout).unwrap();
  let value = |key: &str| {
    readback
      .lines()
      .find_map(|line| line.strip_prefix(key).map(|rest| rest.trim().to_string()))
      .unwrap_or_else(|| panic!("ssh -G prints {key}: {readback}"))
  };
  assert_eq!(value("batchmode "), "yes");
  assert_eq!(value("serveraliveinterval "), "15");
  assert_eq!(value("serveralivecountmax "), "3");
}
