//! The two git reads the server makes in the repository root: the short HEAD sha and its commit date.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `git rev-parse --short HEAD` timeout at startup (`timeouts.SUBPROCESS_GIT_SHA_TIMEOUT`).
pub const SHA_TIMEOUT: Duration = Duration::from_secs(2);
/// The page version's git timeout (`timeouts.SUBPROCESS_GIT_VERSION_TIMEOUT`).
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// The stripped stdout of `git <args>` run in `repo_root`, or `None` when git is missing, exits
/// non-zero, prints nothing, or outlives `timeout`. Each caller picks its own sentinel for `None`.
pub fn run(repo_root: &Path, args: &[&str], timeout: Duration) -> Option<String> {
  let mut child = Command::new("git")
    .args(args)
    .current_dir(repo_root)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .ok()?;
  let deadline = Instant::now() + timeout;
  let status = loop {
    match child.try_wait() {
      Ok(Some(status)) => break status,
      Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
      Ok(None) | Err(_) => {
        // The git process is abandoned either way; a failed kill leaves nothing to report beyond None.
        let _ = child.kill();
        let _ = child.wait();
        return None;
      }
    }
  };
  if !status.success() {
    return None;
  }
  let mut stdout = String::new();
  std::io::Read::read_to_string(&mut child.stdout.take()?, &mut stdout).ok()?;
  let trimmed = stdout.trim();
  if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

/// `buildinfo.read_repo_head_sha`: the short HEAD sha.
pub fn head_sha(repo_root: &Path, timeout: Duration) -> Option<String> {
  run(repo_root, &["rev-parse", "--short", "HEAD"], timeout)
}
