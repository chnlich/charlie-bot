//! The `git` subprocesses of the diff view: output captured, a nonzero exit is not an error of the run, and each
//! call carries a timeout.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use axum::response::Response;
use charliebot_compat::responses;
use http::StatusCode;
use serde_json::json;

/// The read timeout of `rev-parse` and `branch`.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The timeout of the two `diff` reads.
pub(crate) const DIFF_TIMEOUT: Duration = Duration::from_secs(30);

/// The stdout of `git <args>` in `repo`. A nonzero exit answers 500 with the stripped stderr, or
/// `failure_detail` when stderr is empty. A timeout, a spawn failure or undecodable output answers the plain 500 of an
/// unhandled exception.
pub(crate) async fn run(
  repo: &Path,
  args: &[&str],
  timeout: Duration,
  failure_detail: &str,
) -> Result<String, Response> {
  let mut command = tokio::process::Command::new("git");
  command
    .args(args)
    .current_dir(repo)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
  let child = command.spawn().map_err(|error| responses::internal_error("OSError", error.to_string()))?;
  let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
    Ok(output) => output.map_err(|error| responses::internal_error("OSError", error.to_string()))?,
    Err(_) => {
      let detail = format!("Command '[\"git\", {}]' timed out after {} seconds", quoted(args), timeout.as_secs());
      return Err(responses::internal_error("TimeoutExpired", detail));
    }
  };
  // Both streams decode before the exit status is read.
  let stdout = decode(&output.stdout)?;
  let stderr = decode(&output.stderr)?;
  if !output.status.success() {
    let detail = stderr.trim();
    let detail = if detail.is_empty() { failure_detail } else { detail };
    return Err(responses::json(StatusCode::INTERNAL_SERVER_ERROR, &json!({"detail": detail})));
  }
  Ok(stdout)
}

/// Text output: strict UTF-8, with `\r\n` and lone `\r` read as `\n`.
fn decode(bytes: &[u8]) -> Result<String, Response> {
  let text = std::str::from_utf8(bytes)
    .map_err(|error| responses::internal_error("UnicodeDecodeError", format!("git output is not UTF-8: {error}")))?;
  Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// The argument list as the timeout-failure message carries it.
fn quoted(args: &[&str]) -> String {
  args.iter().map(|arg| format!("{arg:?}")).collect::<Vec<_>>().join(", ")
}

/// Each ref's full commit SHA, in order, from one `rev-parse` call.
pub(crate) async fn resolve_commits(repo: &Path, refs: &[&str]) -> Result<Vec<String>, Response> {
  let specs: Vec<String> = refs.iter().map(|reference| format!("{reference}^{{commit}}")).collect();
  let mut args = vec!["rev-parse"];
  args.extend(specs.iter().map(String::as_str));
  let stdout = run(repo, &args, READ_TIMEOUT, "git rev-parse failed").await?;
  Ok(stdout.split_whitespace().map(str::to_string).collect())
}

/// The output lines of `git branch -a`, most recent commit first.
pub(crate) async fn branch_lines(repo: &Path) -> Result<Vec<String>, Response> {
  let stdout = run(
    repo,
    &["branch", "-a", "--sort=-committerdate", "--format=%(refname:short)"],
    READ_TIMEOUT,
    "git branch failed",
  )
  .await?;
  Ok(stdout.lines().map(str::to_string).collect())
}
