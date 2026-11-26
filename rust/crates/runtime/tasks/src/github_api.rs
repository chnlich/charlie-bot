//! The GitHub REST reads of the landing path, run through the host's `gh`.
//!
//! Each read runs `gh api -i`, so the status line and the response headers come back with the body. `gh` exits 1
//! on a 304 and on every 4xx and 5xx, so the status line decides the outcome, never the exit code. A conditional GET
//! answered 304 is `NotModified`; GitHub does not count it against the primary rate limit. A 403 whose
//! `X-RateLimit-Remaining` is 0 is `RateLimited`, and its `X-RateLimit-Reset` names the epoch second the quota
//! returns. Every other failure is an error. The caller decides what a rate limit means: a poller waits until the
//! reset, a one-shot read ends its turn as waiting.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, bail};
use serde_json::Value;

/// The longest one `gh` process may run before it is killed.
const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// One finished `gh` process: whether it exited 0, and its stdout and stderr.
pub struct GhOutput {
  pub success: bool,
  pub stdout: Vec<u8>,
  pub stderr: String,
}

/// What one REST read came back as.
#[derive(Debug, PartialEq, Eq)]
pub enum RestOutcome {
  /// The body parsed as JSON, with the response's ETag when GitHub sent one.
  Body { value: Value, etag: Option<String> },
  /// A conditional GET whose body is unchanged since the ETag it sent.
  NotModified,
  /// The primary rate limit is spent; the quota returns at `reset`, an epoch second.
  RateLimited { reset: i64 },
}

/// Runs `gh` with `args` in `workdir`. A process that outlives `GH_TIMEOUT` is killed and reported as an error.
pub async fn run_gh(workdir: &Path, args: &[String]) -> anyhow::Result<GhOutput> {
  let mut command = tokio::process::Command::new("gh");
  command
    .args(args)
    .current_dir(workdir)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
  let output = match tokio::time::timeout(GH_TIMEOUT, command.output()).await {
    Ok(output) => output.with_context(|| format!("gh {} failed to start", args.join(" ")))?,
    Err(_) => bail!("gh {} timed out after {}s", args.join(" "), GH_TIMEOUT.as_secs()),
  };
  Ok(GhOutput {
    success: output.status.success(),
    stdout: output.stdout,
    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
  })
}

/// One GET of `endpoint` through `run`, which runs `gh` with the arguments it is given.
///
/// `if_none_match` sends the ETag of an earlier answer, so an unchanged body comes back as `NotModified`.
pub async fn rest_get<Run, Fut>(run: &Run, endpoint: &str, if_none_match: Option<&str>) -> anyhow::Result<RestOutcome>
where
  Run: Fn(Vec<String>) -> Fut,
  Fut: Future<Output = anyhow::Result<GhOutput>>,
{
  let mut args = vec!["api".to_string(), "-i".to_string()];
  if let Some(etag) = if_none_match {
    args.push("-H".to_string());
    args.push(format!("If-None-Match: {etag}"));
  }
  args.push(endpoint.to_string());
  let output = run(args).await?;
  outcome(endpoint, output)
}

/// The outcome of one finished `gh api -i` call. A response with no HTTP status line is an error that carries
/// gh's stderr.
fn outcome(endpoint: &str, output: GhOutput) -> anyhow::Result<RestOutcome> {
  let (head, body) = split_response(&output.stdout)
    .with_context(|| format!("gh api {endpoint} printed no HTTP response: {}", output.stderr.trim()))?;
  let status = status_code(head).with_context(|| format!("gh api {endpoint} printed no HTTP status line"))?;
  match status {
    200..=299 => {
      let value =
        serde_json::from_slice(body).with_context(|| format!("gh api {endpoint} printed a body that is not JSON"))?;
      Ok(RestOutcome::Body { value, etag: header(head, "etag") })
    }
    304 => Ok(RestOutcome::NotModified),
    403 if header(head, "x-ratelimit-remaining").as_deref() == Some("0") => {
      let reset = header(head, "x-ratelimit-reset")
        .with_context(|| format!("gh api {endpoint} answered a rate-limit 403 with no X-RateLimit-Reset"))?
        .parse::<i64>()
        .with_context(|| format!("gh api {endpoint} answered an X-RateLimit-Reset that is not an epoch second"))?;
      Ok(RestOutcome::RateLimited { reset })
    }
    _ => bail!("gh api {endpoint} answered HTTP {status}: {}", output.stderr.trim()),
  }
}

/// The header block and the body of a `gh api -i` stdout, split at the first blank line.
fn split_response(raw: &[u8]) -> Option<(&str, &[u8])> {
  let (end, skip) = match find(raw, b"\r\n\r\n") {
    Some(index) => (index, 4),
    None => (find(raw, b"\n\n")?, 2),
  };
  let head = std::str::from_utf8(&raw[..end]).ok()?;
  Some((head, &raw[end + skip..]))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
  haystack.windows(needle.len()).position(|window| window == needle)
}

/// The three-digit code of a status line such as `HTTP/2.0 403 Forbidden`.
fn status_code(head: &str) -> Option<u16> {
  head.lines().next()?.split_whitespace().nth(1)?.parse().ok()
}

/// The value of the first header named `name` (case-insensitive), trimmed.
fn header(head: &str, name: &str) -> Option<String> {
  head.lines().skip(1).find_map(|line| {
    let (key, value) = line.split_once(':')?;
    key.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_string())
  })
}

#[cfg(test)]
mod tests {
  use std::cell::RefCell;
  use std::collections::VecDeque;

  use super::*;
  use serde_json::json;

  fn reply(status_line: &str, headers: &[&str], body: &str, stderr: &str, success: bool) -> GhOutput {
    let mut stdout = format!("{status_line}\r\n");
    for header in headers {
      stdout.push_str(header);
      stdout.push_str("\r\n");
    }
    stdout.push_str("\r\n");
    stdout.push_str(body);
    GhOutput { success, stdout: stdout.into_bytes(), stderr: stderr.to_string() }
  }

  /// Runs `rest_get` against a script of replies, one per call, and returns the outcome with each call's arguments.
  async fn scripted_get(
    replies: Vec<GhOutput>,
    if_none_match: Option<&str>,
  ) -> (anyhow::Result<RestOutcome>, Vec<Vec<String>>) {
    let script = RefCell::new(VecDeque::from(replies));
    let calls = RefCell::new(Vec::new());
    let run = |args: Vec<String>| {
      calls.borrow_mut().push(args);
      let next = script.borrow_mut().pop_front().expect("the script has a reply for each call");
      std::future::ready(Ok(next))
    };
    let outcome = rest_get(&run, "repos/acme/widgets/commits/abc/status", if_none_match).await;
    (outcome, calls.into_inner())
  }

  #[tokio::test]
  async fn a_200_reply_is_its_json_body_with_its_etag() {
    let body = json!({"total_count": 0}).to_string();
    let (outcome, _) = scripted_get(vec![reply("HTTP/2.0 200 OK", &["Etag: \"abc\""], &body, "", true)], None).await;
    assert_eq!(
      outcome.unwrap(),
      RestOutcome::Body { value: json!({"total_count": 0}), etag: Some("\"abc\"".to_string()) }
    );
  }

  #[tokio::test]
  async fn a_304_is_not_modified_even_though_gh_exits_one() {
    let (outcome, calls) =
      scripted_get(vec![reply("HTTP/2.0 304 Not Modified", &[], "", "gh: HTTP 304", false)], Some("\"abc\"")).await;
    assert_eq!(outcome.unwrap(), RestOutcome::NotModified);
    assert_eq!(
      calls[0],
      ["api", "-i", "-H", "If-None-Match: \"abc\"", "repos/acme/widgets/commits/abc/status"],
      "the conditional GET sends the ETag it was given"
    );
  }

  #[tokio::test]
  async fn a_rate_limit_403_names_its_reset_time() {
    let (outcome, _) = scripted_get(
      vec![reply(
        "HTTP/2.0 403 Forbidden",
        &["X-RateLimit-Remaining: 0", "X-RateLimit-Reset: 1795719077"],
        "{\"message\":\"API rate limit exceeded\"}",
        "gh: API rate limit exceeded (HTTP 403)",
        false,
      )],
      None,
    )
    .await;
    assert_eq!(outcome.unwrap(), RestOutcome::RateLimited { reset: 1_795_719_077 });
  }

  #[tokio::test]
  async fn a_403_without_an_exhausted_quota_is_an_error_not_a_rate_limit() {
    let (outcome, _) = scripted_get(
      vec![reply(
        "HTTP/2.0 403 Forbidden",
        &["X-RateLimit-Remaining: 4999", "X-RateLimit-Reset: 1795719077"],
        "{\"message\":\"Resource not accessible by integration\"}",
        "gh: Resource not accessible by integration (HTTP 403)",
        false,
      )],
      None,
    )
    .await;
    let error = outcome.unwrap_err().to_string();
    assert!(error.contains("answered HTTP 403"), "{error}");
    assert!(error.contains("Resource not accessible"), "{error}");
  }

  #[tokio::test]
  async fn a_server_error_is_an_error() {
    let bad_gateway = reply("HTTP/2.0 502 Bad Gateway", &[], "", "gh: Bad Gateway", false);
    let (outcome, _) = scripted_get(vec![bad_gateway], None).await;
    assert!(outcome.unwrap_err().to_string().contains("answered HTTP 502"));
  }

  #[tokio::test]
  async fn a_rate_limit_403_without_a_reset_header_is_an_error() {
    let (outcome, _) = scripted_get(
      vec![reply("HTTP/2.0 403 Forbidden", &["X-RateLimit-Remaining: 0"], "{}", "gh: forbidden (HTTP 403)", false)],
      None,
    )
    .await;
    assert!(outcome.unwrap_err().to_string().contains("no X-RateLimit-Reset"));
  }

  #[tokio::test]
  async fn a_failure_with_no_http_response_carries_gh_stderr() {
    let run = |_args: Vec<String>| {
      std::future::ready(Ok(GhOutput {
        success: false,
        stdout: Vec::new(),
        stderr: "gh: could not resolve host".to_string(),
      }))
    };
    let error = rest_get(&run, "repos/o/r", None).await.unwrap_err().to_string();
    assert!(error.contains("could not resolve host"), "{error}");
  }
}
