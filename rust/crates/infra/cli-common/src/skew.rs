//! The version-skew hint: compare the running server's SHA with the repo's, and tell the caller to restart.

use charliebot_core::git;
use charliebot_core::repo_root;

use crate::timeouts;
use crate::wire::{Method, TransportError, WireRequest, WireResponse};

/// The version route `app/server` mounts. The skew probe sends its `GET` to the running server.
pub const VERSION_PATH: &str = "/api/internal/version";

/// `compose_version_skew_hint`: a hint when the server and the repo sit at different SHAs, `None`
/// when either SHA is missing (a best-effort fetch failed) or when they match.
pub fn compose_version_skew_hint(
  server_sha: Option<&str>,
  started_at: Option<&str>,
  local_sha: Option<&str>,
) -> Option<String> {
  let (server_sha, local_sha) =
    match (server_sha.filter(|sha| !sha.is_empty()), local_sha.filter(|sha| !sha.is_empty())) {
      (Some(server_sha), Some(local_sha)) => (server_sha, local_sha),
      _ => return None,
    };
  if server_sha == local_sha {
    return None;
  }
  let started_clause =
    started_at.filter(|started| !started.is_empty()).map(|started| format!(" (started {started})")).unwrap_or_default();
  Some(format!("server running {server_sha}{started_clause}, repo at {local_sha} — server restart may be required"))
}

/// `_maybe_version_skew_hint`: gather the server and local SHAs and compose the hint. Every
/// failure while gathering means no hint, so the CLI error path never raises from the hint.
pub fn version_skew_hint(
  base_url: &str,
  headers: &[(String, String)],
  transport: &mut dyn FnMut(&WireRequest) -> Result<WireResponse, TransportError>,
) -> Option<String> {
  let (server_sha, started_at) = best_effort_server_version(base_url, headers, transport);
  let local_sha = local_repo_head_sha();
  compose_version_skew_hint(server_sha.as_deref(), started_at.as_deref(), local_sha.as_deref())
}

/// `_best_effort_server_version`: `GET /api/internal/version` under the skew timeout; a connect
/// failure, a lost response, a non-2xx status or a non-JSON body all read as no version.
fn best_effort_server_version(
  base_url: &str,
  headers: &[(String, String)],
  transport: &mut dyn FnMut(&WireRequest) -> Result<WireResponse, TransportError>,
) -> (Option<String>, Option<String>) {
  let request = WireRequest {
    method: Method::Get,
    url: format!("{base_url}{VERSION_PATH}"),
    headers: headers.to_vec(),
    body: None,
    timeout: timeouts::HTTP_VERSION_SKEW_TIMEOUT,
  };
  let Ok(response) = transport(&request) else { return (None, None) };
  if response.status >= 400 {
    return (None, None);
  }
  match serde_json::from_slice::<serde_json::Value>(&response.body) {
    Ok(info) => (string_field(&info, "sha"), string_field(&info, "started_at")),
    Err(_) => (None, None),
  }
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
  value.get(key).and_then(serde_json::Value::as_str).map(str::to_string)
}

/// The local half of the hint: `git rev-parse --short HEAD` in the directory
/// `CHARLIEBOT_REPO_ROOT` names, under the sha timeout. A missing variable or a failed
/// read yields no local SHA and so no hint.
fn local_repo_head_sha() -> Option<String> {
  let root = repo_root::RepoRoot::new(std::env::var_os(repo_root::REPO_ROOT_ENV)).ok()?;
  git::head_sha(root.path(), timeouts::SUBPROCESS_GIT_SHA_TIMEOUT)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_sha_pair_composes_the_hint_with_the_start_time() {
    let hint = compose_version_skew_hint(Some("abc1234"), Some("2026-01-02T03:04:05+00:00"), Some("def5678"));
    assert_eq!(
      hint.as_deref(),
      Some(
        "server running abc1234 (started 2026-01-02T03:04:05+00:00), repo at def5678 — server restart may be required"
      )
    );
  }

  #[test]
  fn a_missing_start_time_leaves_the_clause_out() {
    let hint = compose_version_skew_hint(Some("abc1234"), None, Some("def5678"));
    assert_eq!(hint.as_deref(), Some("server running abc1234, repo at def5678 — server restart may be required"));
  }

  #[test]
  fn no_hint_without_both_shas_or_on_a_match() {
    assert_eq!(compose_version_skew_hint(None, None, Some("def5678")), None);
    assert_eq!(compose_version_skew_hint(Some("abc1234"), None, None), None);
    assert_eq!(compose_version_skew_hint(Some("abc1234"), None, Some("abc1234")), None);
    assert_eq!(compose_version_skew_hint(Some(""), None, Some("def5678")), None);
  }
}
