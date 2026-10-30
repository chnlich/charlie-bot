//! Reads a served session home back against its manifest.
//!
//! The session count comes from the session list. That route reads only `metadata.json`, so the newest message is read
//! from the events route of the largest session and of two sessions that the manifest seed picks.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use charliebot_cli_common::http::ensure_crypto_provider;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::manifest::{MANIFEST_NAME, Manifest, ManifestSession};
use crate::rng::Rng;

/// The `before` cursor of the newest page. The server clamps it to the session's message count.
const NEWEST_PAGE_BEFORE: u64 = (1 << 31) - 1;
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);
/// The variable that carries the access key as one `Name: value` header.
const AUTH_HEADER_VAR: &str = "CHARLIEBOT_SERVE_AUTH_HEADER";
/// The sessions that are read besides the largest one.
const PICKED_SESSIONS: usize = 2;

/// Checks the home at `base_url` against the manifest in `output`. Prints each mismatch to stderr and returns `Ok(false)`;
/// returns `Ok(true)` when every check holds.
pub fn check(output: &Path, base_url: &str) -> Result<bool> {
  let manifest = Manifest::read(output)?;
  let headers = auth_headers()?;
  let base = base_url.trim_end_matches('/');
  let checked = checked_sessions(&manifest);
  let runtime =
    tokio::runtime::Builder::new_current_thread().enable_all().build().context("cannot start the HTTP runtime")?;
  let failures = runtime.block_on(read_back(&manifest, &checked, base, &headers))?;
  if !failures.is_empty() {
    for failure in &failures {
      eprintln!("MISMATCH {failure}");
    }
    return Ok(false);
  }
  println!(
    "check passed: {} sessions; newest messages of {} sessions match {MANIFEST_NAME}",
    manifest.session_count,
    checked.len()
  );
  Ok(true)
}

/// The largest session, then `PICKED_SESSIONS` of the others, which the manifest seed picks.
fn checked_sessions(manifest: &Manifest) -> Vec<&ManifestSession> {
  let largest = manifest
    .sessions
    .iter()
    .max_by_key(|session| session.event_log_bytes)
    .expect("the manifest lists at least one session");
  let mut others: Vec<&ManifestSession> = manifest.sessions.iter().filter(|session| session.id != largest.id).collect();
  others.sort_by(|left, right| left.id.cmp(&right.id));
  Rng::new(manifest.seed).shuffle(&mut others);
  let mut checked = vec![largest];
  checked.extend(others.into_iter().take(PICKED_SESSIONS));
  checked
}

/// Compares the served session count and each checked session's newest message. Returns one line per mismatch.
async fn read_back(
  manifest: &Manifest,
  checked: &[&ManifestSession],
  base: &str,
  headers: &HeaderMap,
) -> Result<Vec<String>> {
  ensure_crypto_provider();
  // A client per call: the check reads a handful of routes, so no connection pool is worth keeping.
  let client =
    reqwest::Client::builder().no_proxy().timeout(HTTP_TIMEOUT).build().context("cannot build the HTTP client")?;
  let mut failures = Vec::new();

  let listing = get_json(&client, &format!("{base}/api/sessions/"), headers).await?;
  let listed = listing.as_array().context("the session list is not a JSON array")?.len();
  if listed != manifest.session_count {
    failures.push(format!("session count: manifest {}, server {listed}", manifest.session_count));
  }

  for session in checked {
    let url = format!("{base}/api/sessions/{}/events?before={NEWEST_PAGE_BEFORE}", session.id);
    let page = match get_json(&client, &url, headers).await {
      Ok(page) => page,
      Err(error) => {
        failures.push(format!("session {}: {error:#}", session.id));
        continue;
      }
    };
    let served =
      page["messages"].as_array().and_then(|messages| messages.last()).and_then(|message| message["content"].as_str());
    if served != Some(session.last_message.as_str()) {
      failures.push(format!(
        "session {}: manifest newest message {:?}, server newest message {served:?}",
        session.id, session.last_message
      ));
    }
  }
  Ok(failures)
}

async fn get_json(client: &reqwest::Client, url: &str, headers: &HeaderMap) -> Result<Value> {
  let response = client
    .get(url)
    .headers(headers.clone())
    .send()
    .await
    .and_then(reqwest::Response::error_for_status)
    .with_context(|| format!("GET {url} failed"))?;
  let body = response.bytes().await.with_context(|| format!("GET {url} failed"))?;
  serde_json::from_slice(&body).with_context(|| format!("GET {url} did not return JSON"))
}

/// The access-key header from `CHARLIEBOT_SERVE_AUTH_HEADER`. An unset or empty variable sends no header.
fn auth_headers() -> Result<HeaderMap> {
  let mut headers = HeaderMap::new();
  let value = match std::env::var(AUTH_HEADER_VAR) {
    Ok(value) => value,
    Err(std::env::VarError::NotPresent) => return Ok(headers),
    Err(error) => return Err(error).with_context(|| format!("{AUTH_HEADER_VAR} is not text")),
  };
  if value.is_empty() {
    return Ok(headers);
  }
  let Some((name, token)) = value.split_once(':') else {
    bail!("{AUTH_HEADER_VAR} must read `Name: value`");
  };
  headers.insert(HeaderName::from_bytes(name.trim().as_bytes())?, HeaderValue::from_str(token.trim())?);
  Ok(headers)
}
