//! The `git` subprocesses of the diff view: output captured, a nonzero exit is not an error of the run, and each
//! call carries a timeout.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use axum::response::Response;
use charliebot_compat::responses;
use charliebot_core::git::object_exists_args;
use charliebot_core::str_methods::universal_newlines;
use http::StatusCode;
use serde_json::json;

/// The read timeout of `rev-parse` and `branch`.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The timeout of the two `diff` reads.
pub(crate) const DIFF_TIMEOUT: Duration = Duration::from_secs(30);

/// The exit status and the two streams of one finished `git` run. The stderr is decoded before the status is read;
/// the stdout stays bytes so a caller can check the content before decoding it.
struct RawOutput {
  ok: bool,
  stdout: Vec<u8>,
  stderr: String,
}

/// Runs `git <args>` in `repo` and collects its streams. Only a timeout, a spawn failure or undecodable stderr is
/// this function's error; a nonzero exit lands in `RawOutput.ok`.
async fn run_raw(repo: &Path, args: &[&str], timeout: Duration) -> Result<RawOutput, Response> {
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
  let stderr = decode(&output.stderr)?;
  Ok(RawOutput { ok: output.status.success(), stdout: output.stdout, stderr })
}

/// The 500 a nonzero git exit answers with: the stripped stderr, or `failure_detail` when stderr is empty.
fn failed(output: &RawOutput, failure_detail: &str) -> Response {
  let detail = output.stderr.trim();
  let detail = if detail.is_empty() { failure_detail } else { detail };
  responses::json(StatusCode::INTERNAL_SERVER_ERROR, &json!({"detail": detail}))
}

/// The stdout of `git <args>` in `repo`. A nonzero exit answers 500 with the stripped stderr, or
/// `failure_detail` when stderr is empty. A timeout, a spawn failure or undecodable output answers the plain 500 of an
/// unhandled exception.
pub(crate) async fn run(
  repo: &Path,
  args: &[&str],
  timeout: Duration,
  failure_detail: &str,
) -> Result<String, Response> {
  let output = run_raw(repo, args, timeout).await?;
  if !output.ok {
    return Err(failed(&output, failure_detail));
  }
  decode(&output.stdout)
}

/// The bytes of `<revision>:<path>` when the path exists at that revision, `None` when it does not. The existence
/// probe is `git cat-file -e`'s exit status.
pub(crate) async fn show_bytes(repo: &Path, revision: &str, path: &str) -> Result<Option<Vec<u8>>, Response> {
  let spec = format!("{revision}:{path}");
  let probe = run_raw(repo, &object_exists_args(&spec), READ_TIMEOUT).await?;
  if !probe.ok {
    return Ok(None);
  }
  let output = run_raw(repo, &["show", &spec], READ_TIMEOUT).await?;
  if !output.ok {
    return Err(failed(&output, "git show failed"));
  }
  Ok(Some(output.stdout))
}

/// The number of lines the content holds: one per `\n` or `\r`, where `\r\n` is one break. A final segment without a
/// break is a line. This is the line count of `decode`'s text, so the two ends of a read agree on one file's length.
pub(crate) fn count_lines(bytes: &[u8]) -> usize {
  let mut count = 0;
  let mut index = 0;
  while index < bytes.len() {
    match bytes[index] {
      b'\n' => {
        count += 1;
        index += 1;
      }
      b'\r' => {
        count += 1;
        index += if bytes.get(index + 1) == Some(&b'\n') { 2 } else { 1 };
      }
      _ => index += 1,
    }
  }
  match bytes.last() {
    Some(last) if *last == b'\n' || *last == b'\r' => count,
    Some(_) => count + 1,
    None => 0,
  }
}

/// Text output: strict UTF-8, with `\r\n` and lone `\r` read as `\n`.
pub(crate) fn decode(bytes: &[u8]) -> Result<String, Response> {
  let text = std::str::from_utf8(bytes)
    .map_err(|error| responses::internal_error("UnicodeDecodeError", format!("git output is not UTF-8: {error}")))?;
  Ok(universal_newlines(text))
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

/// The merge base of two commits, the revision a three-dot range's base side diffs against.
pub(crate) async fn merge_base(repo: &Path, base: &str, head: &str) -> Result<String, Response> {
  let stdout = run(repo, &["merge-base", base, head], READ_TIMEOUT, "git merge-base failed").await?;
  Ok(stdout.trim().to_string())
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
