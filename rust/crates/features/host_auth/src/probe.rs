//! One probe of a host (`host_auth.py` `classify`, `build_detail` and `probe_host`): `ssh <alias> true` run through
//! charliebot-core's batch-mode argv, its output read in full, and the verdict read from that output.

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use charliebot_core::ssh;
use charliebot_core::str_methods;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// The overall wait on one ssh subprocess; the kill it ends is what the classifier reads past.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
pub const DETAIL_MAX_CHARS: usize = 200;

pub const STATUS_OK: &str = "ok";
pub const STATUS_NEEDS_OKTA: &str = "needs-okta";
pub const STATUS_NEEDS_INTERACTIVE_AUTH: &str = "needs-interactive-auth";
pub const STATUS_UNREACHABLE: &str = "unreachable";

/// Output markers that name a hold. They are matched before any return code: a held host's prompt waits until the
/// probe timeout kills it, so the return code of a held probe says nothing.
const OKTA_MARKERS: [&str; 2] = ["Okta verification required", "activate?user_code="];
const INTERACTIVE_MARKERS: [&str; 3] =
  ["requires an additional check", "To authenticate, visit:", "Host key verification failed"];
const URL_MARKER: &str = "http";

/// `classify`: the verdict of one probe, the output markers first and the return code second.
pub fn classify(returncode: i32, output: &str) -> &'static str {
  if OKTA_MARKERS.iter().any(|marker| output.contains(marker)) {
    return STATUS_NEEDS_OKTA;
  }
  if returncode == 0 {
    return STATUS_OK;
  }
  if INTERACTIVE_MARKERS.iter().any(|marker| output.contains(marker)) {
    return STATUS_NEEDS_INTERACTIVE_AUTH;
  }
  STATUS_UNREACHABLE
}

/// `build_detail`: the first output line, plus the first URL line that follows it. An empty output is named by its
/// return code. The result is capped at `DETAIL_MAX_CHARS` characters.
pub fn build_detail(returncode: i32, output: &str) -> String {
  let stripped = str_methods::strip(output);
  if stripped.is_empty() {
    return format!("exit {returncode}");
  }
  let lines = str_methods::splitlines(stripped);
  let (first, rest) = lines.split_first().expect("a non-empty stripped text has a line");
  let mut parts = vec![str_methods::strip(first)];
  if let Some(url_line) = rest.iter().find(|line| line.contains(URL_MARKER)) {
    parts.push(str_methods::strip(url_line));
  }
  parts.join("\n").chars().take(DETAIL_MAX_CHARS).collect()
}

/// `probe_host`: probe `alias` with the standing timeout. The result is `(returncode, output)`.
pub async fn probe_host(alias: String) -> io::Result<(i32, String)> {
  let argv = ssh::ssh_cmd(&alias, &["true"])?;
  run_probe(&argv, PROBE_TIMEOUT).await
}

/// Run `argv`, read its stdout and stderr in full, and kill it when it outlives `timeout`. The output read before
/// the kill is kept. The return code is the exit code, or the negated signal number of a killed process, as
/// Python's `returncode` reports it.
pub async fn run_probe(argv: &[String], timeout: Duration) -> io::Result<(i32, String)> {
  let (program, arguments) = argv.split_first().expect("an ssh argv names a program");
  let mut child = Command::new(program).args(arguments).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
  let stdout = child.stdout.take().expect("the stdout pipe was requested");
  let stderr = child.stderr.take().expect("the stderr pipe was requested");
  let stdout_read = tokio::spawn(read_to_end(stdout));
  let stderr_read = tokio::spawn(read_to_end(stderr));
  let status = match tokio::time::timeout(timeout, child.wait()).await {
    Ok(status) => status?,
    Err(_elapsed) => {
      child.start_kill()?;
      child.wait().await?
    }
  };
  let mut output = stdout_read.await.expect("the stdout reader does not panic")?;
  output.extend(stderr_read.await.expect("the stderr reader does not panic")?);
  Ok((returncode(status), String::from_utf8_lossy(&output).into_owned()))
}

async fn read_to_end<R: tokio::io::AsyncRead + Unpin>(mut pipe: R) -> io::Result<Vec<u8>> {
  let mut buffer = Vec::new();
  pipe.read_to_end(&mut buffer).await?;
  Ok(buffer)
}

fn returncode(status: ExitStatus) -> i32 {
  status.code().unwrap_or_else(|| -status.signal().expect("a process with no exit code ended by a signal"))
}

#[cfg(test)]
mod tests {
  use std::path::{Path, PathBuf};
  use std::sync::OnceLock;

  use charliebot_shell_harness::write_executable;

  use super::*;

  #[test]
  fn the_output_marker_beats_the_return_code_and_the_code_classifies_the_rest() {
    assert_eq!(classify(124, "Okta verification required"), STATUS_NEEDS_OKTA);
    assert_eq!(classify(-9, "Enroll at https://login.example.internal/activate?user_code=7f3c9d21"), STATUS_NEEDS_OKTA);
    assert_eq!(classify(0, ""), STATUS_OK);
    assert_eq!(classify(0, "some unrelated banner"), STATUS_OK);
    assert_eq!(
      classify(1, "# Tailscale SSH requires an additional check.\nTo authenticate, visit: https://login.example.net/a"),
      STATUS_NEEDS_INTERACTIVE_AUTH
    );
    assert_eq!(classify(255, "Host key verification failed."), STATUS_NEEDS_INTERACTIVE_AUTH);
    assert_eq!(classify(255, "ssh: connect to host port 22: Connection timed out"), STATUS_UNREACHABLE);
    assert_eq!(classify(124, "ssh: connect to host port 22: Connection timed out"), STATUS_UNREACHABLE);
  }

  #[test]
  fn the_detail_names_the_first_line_and_the_first_url_line_under_the_cap() {
    assert_eq!(build_detail(0, "  \n"), "exit 0");
    assert_eq!(build_detail(-9, ""), "exit -9");
    assert_eq!(
      build_detail(1, "first line\nchatter\nTo authenticate, visit: https://x.example.net/a\nlater"),
      "first line\nTo authenticate, visit: https://x.example.net/a"
    );
    let long = "x".repeat(DETAIL_MAX_CHARS + 50);
    assert_eq!(build_detail(255, &long).chars().count(), DETAIL_MAX_CHARS);
  }

  /// The stand-in ssh. The probe passes the alias just before `true`, so the script reads the alias from there and
  /// each alias shows one kind of host. The held host's `exec` makes its sleep the process the timeout kills.
  const STAND_IN: &str = r#"#!/bin/sh
for arg in "$@"; do
  alias=$previous
  previous=$arg
done
case "$alias" in
  held)
    printf 'Okta verification required\n'
    printf 'Enroll at https://login.example.internal/activate?user_code=7f3c9d21\n'
    exec sleep 30
    ;;
  refused)
    printf 'ssh: connect to host refused.example.internal port 22: Connection refused\n' >&2
    exit 255
    ;;
  silent)
    exit 255
    ;;
  invalid-utf8)
    printf 'Okta verification required \377\376 tail\n'
    ;;
esac
"#;

  /// The stand-in's path. It is written once, by `write_executable`, which never leaves the file open for writing in
  /// this process, so a probe can execute it while other tests fork.
  fn stand_in() -> PathBuf {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
      let dir = tempfile::tempdir().expect("a temporary directory");
      write_executable(&dir.path().join("ssh"), STAND_IN);
      dir
    });
    dir.path().join("ssh")
  }

  /// The argv the probe builds for `alias`, with its program replaced by the stand-in.
  fn probe_argv(alias: &str) -> Vec<String> {
    let mut argv = ssh::argv(alias, &["true"], Path::new("/nonexistent/controlmasters"));
    argv[0] = stand_in().display().to_string();
    argv
  }

  #[tokio::test]
  async fn a_held_host_is_killed_after_the_timeout_and_the_output_it_printed_is_kept() {
    let (returncode, output) =
      run_probe(&probe_argv("held"), Duration::from_millis(300)).await.expect("the probe runs");
    assert_eq!(returncode, -9);
    assert_eq!(classify(returncode, &output), STATUS_NEEDS_OKTA);
    assert_eq!(
      build_detail(returncode, &output),
      "Okta verification required\nEnroll at https://login.example.internal/activate?user_code=7f3c9d21"
    );
  }

  #[tokio::test]
  async fn a_host_that_exits_255_with_a_connect_error_is_unreachable_with_that_error() {
    let (returncode, output) = run_probe(&probe_argv("refused"), PROBE_TIMEOUT).await.expect("the probe runs");
    assert_eq!(returncode, 255);
    assert_eq!(classify(returncode, &output), STATUS_UNREACHABLE);
    assert_eq!(
      build_detail(returncode, &output),
      "ssh: connect to host refused.example.internal port 22: Connection refused"
    );
  }

  #[tokio::test]
  async fn a_host_with_no_output_is_named_by_its_return_code() {
    let (returncode, output) = run_probe(&probe_argv("silent"), PROBE_TIMEOUT).await.expect("the probe runs");
    assert!(output.is_empty());
    assert_eq!(classify(returncode, &output), STATUS_UNREACHABLE);
    assert_eq!(build_detail(returncode, &output), "exit 255");
  }

  #[tokio::test]
  async fn output_with_invalid_utf8_decodes_with_replacement_characters() {
    let (returncode, output) = run_probe(&probe_argv("invalid-utf8"), PROBE_TIMEOUT).await.expect("the probe runs");
    assert_eq!(classify(returncode, &output), STATUS_NEEDS_OKTA);
    assert_eq!(build_detail(returncode, &output), "Okta verification required \u{fffd}\u{fffd} tail");
  }
}
