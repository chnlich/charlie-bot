//! The batch-mode ssh invocation that the remote probes and the CLI's remote launcher share.

use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::paths;

/// The ssh `ConnectTimeout`, in seconds.
pub const CONNECT_TIMEOUT_SECONDS: u32 = 10;

/// How long a ControlMaster stays up after its last use, in seconds.
pub const CONTROL_PERSIST_SECONDS: u32 = 1200;

/// `~/.ssh/controlmasters` of the process's home.
pub fn control_dir() -> PathBuf {
  PathBuf::from(paths::expanduser("~")).join(".ssh").join("controlmasters")
}

/// `dir` created with mode 0700 when it is absent. Its parents get the default mode,
/// and an existing directory at `dir` is kept as it is.
pub fn ensure_control_dir(dir: &Path) -> io::Result<()> {
  if let Some(parent) = dir.parent() {
    std::fs::create_dir_all(parent)?;
  }
  match DirBuilder::new().mode(0o700).create(dir) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
    Err(error) => Err(error),
  }
}

/// The argv that runs `remote_argv` on `host`, reusing the ControlMaster sockets under `control_dir`.
pub fn argv(host: &str, remote_argv: &[&str], control_dir: &Path) -> Vec<String> {
  let options = [
    "BatchMode=yes".to_string(),
    format!("ConnectTimeout={CONNECT_TIMEOUT_SECONDS}"),
    "ControlMaster=auto".to_string(),
    format!("ControlPath={}/%C", paths::display(control_dir)),
    format!("ControlPersist={CONTROL_PERSIST_SECONDS}"),
  ];
  let mut argv = vec!["ssh".to_string()];
  for option in options {
    argv.push("-o".to_string());
    argv.push(option);
  }
  argv.push(host.to_string());
  argv.extend(remote_argv.iter().map(|arg| arg.to_string()));
  argv
}

/// The argv for `host` under the process's control directory, which is created 0700 first: ssh fails a probe
/// outright when the ControlPath's parent is missing.
pub fn ssh_cmd(host: &str, remote_argv: &[&str]) -> io::Result<Vec<String>> {
  let dir = control_dir();
  ensure_control_dir(&dir)?;
  Ok(argv(host, remote_argv, &dir))
}

/// The argv that runs `remote_argv` on `host` over a new connection, with the options of the remote launcher's
/// preflight. It names no ControlMaster socket, so a reused connection cannot answer for a login that the preflight
/// would have to make again.
pub fn fresh_argv(host: &str, remote_argv: &[&str]) -> Vec<String> {
  let mut argv = vec![
    "ssh".to_string(),
    "-T".to_string(),
    "-o".to_string(),
    "BatchMode=yes".to_string(),
    "-o".to_string(),
    format!("ConnectTimeout={CONNECT_TIMEOUT_SECONDS}"),
    "-n".to_string(),
    host.to_string(),
  ];
  argv.extend(remote_argv.iter().map(|arg| arg.to_string()));
  argv
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::PermissionsExt;

  use serde_json::Value;

  use super::*;

  const FIXTURE: &str = include_str!("../../../../fixtures/golden/ssh_argv_golden.json");

  /// The home that the fixture's `{home}` stands for.
  const HOME: &str = "/home/user";

  #[test]
  fn argv_matches_the_builder() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("the fixture is JSON");
    let control_dir = Path::new(HOME).join(".ssh").join("controlmasters");
    let cases = fixture["cases"].as_array().expect("the fixture lists cases");
    assert!(!cases.is_empty(), "the fixture holds cases");
    for case in cases {
      let host = case["host"].as_str().expect("a case has a host");
      let remote: Vec<&str> = case["remote_argv"]
        .as_array()
        .expect("a case has a remote argv")
        .iter()
        .map(|arg| arg.as_str().expect("remote argv entries are strings"))
        .collect();
      let expected: Vec<String> = case["argv"]
        .as_array()
        .expect("a case has an argv")
        .iter()
        .map(|arg| arg.as_str().expect("argv entries are strings").replace("{home}", HOME))
        .collect();
      assert_eq!(argv(host, &remote, &control_dir), expected, "argv for {host}");
    }
  }

  #[test]
  fn fresh_argv_is_the_launch_preflight_without_a_control_socket() {
    assert_eq!(
      fresh_argv("gpu-box-1", &["true"]),
      ["ssh", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-n", "gpu-box-1", "true"]
    );
  }

  #[test]
  fn control_directory_is_0700_and_tolerates_an_existing_one() {
    let home = tempfile::tempdir().expect("a temporary home");
    let dir = home.path().join(".ssh").join("controlmasters");
    ensure_control_dir(&dir).expect("the directory is created");
    let mode = std::fs::metadata(&dir).expect("the directory exists").permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "the control directory is private");
    ensure_control_dir(&dir).expect("an existing directory is kept");
    let blocker = home.path().join("file");
    std::fs::write(&blocker, "").expect("a file to block the path");
    assert!(ensure_control_dir(&blocker).is_err(), "a file in the directory's place is an error");
  }
}
