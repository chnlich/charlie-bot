//! Reads a served session home back against its manifest.
//!
//! The session list must hold the manifest's sessions and the sessions that the default scheduled tasks bind (see
//! `session_count_mismatches`). That route reads only `metadata.json`, so the newest message is read from the events
//! route of the largest session and of two sessions that the manifest seed picks. Each scheduled task of the manifest
//! must be listed by the task route, unbroken and bound to the manifest's session.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use charliebot_cli_common::http::ensure_crypto_provider;
use charliebot_cli_common::route;
use charliebot_cron::TASKS_PATH;
use charliebot_session_api::api as session_routes;
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

/// Whether a served home matched its manifest.
pub enum CheckOutcome {
  /// Every check held.
  Matches,
  /// At least one check failed; each mismatch is already on stderr.
  Mismatches,
}

/// Checks the home at `base_url` against the manifest in `output`. Prints each mismatch to stderr and returns
/// `CheckOutcome::Mismatches`, or `CheckOutcome::Matches` when every check holds.
pub fn check(output: &Path, base_url: &str) -> Result<CheckOutcome> {
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
    return Ok(CheckOutcome::Mismatches);
  }
  println!(
    "check passed: {} sessions; newest messages of {} sessions match {MANIFEST_NAME}; {} scheduled tasks are bound",
    manifest.session_count,
    checked.len(),
    manifest.cron_tasks.len()
  );
  Ok(CheckOutcome::Matches)
}

/// The mismatches between a served session list and the manifest. The scheduler binds each task that has no session on
/// its first tick, enabled or not, so every default task adds the session it binds to the list. A listed session outside
/// the manifest is accepted only when a task outside the manifest binds it. Each manifest session must be listed, and the
/// list must hold the manifest's count plus those bound sessions.
fn session_count_mismatches(manifest: &Manifest, listed_ids: &[String], tasks: &[Value]) -> Vec<String> {
  let manifest_ids: HashSet<&str> = manifest.sessions.iter().map(|session| session.id.as_str()).collect();
  let listed: HashSet<&str> = listed_ids.iter().map(String::as_str).collect();
  let fixture_names: HashSet<&str> = manifest.cron_tasks.iter().map(|task| task.name.as_str()).collect();
  let default_bound: HashSet<&str> = tasks
    .iter()
    .filter(|row| row["name"].as_str().is_some_and(|name| !fixture_names.contains(name)))
    .filter_map(|row| row["session_id"].as_str())
    .collect();
  let mut failures = Vec::new();
  for session in &manifest.sessions {
    if !listed.contains(session.id.as_str()) {
      failures.push(format!("session {}: in the manifest, but the server lists no such session", session.id));
    }
  }
  let mut auto_bound = 0;
  for id in listed_ids {
    if manifest_ids.contains(id.as_str()) {
      continue;
    }
    if default_bound.contains(id.as_str()) {
      auto_bound += 1;
    } else {
      failures
        .push(format!("session {id}: listed by the server, but neither in the manifest nor bound by a default task"));
    }
  }
  if listed_ids.len() != manifest.session_count + auto_bound {
    failures.push(format!(
      "session count: manifest {}, server {}, of which {auto_bound} bound by default tasks",
      manifest.session_count,
      listed_ids.len()
    ));
  }
  failures
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

  let listing = get_json(&client, &format!("{base}{}", session_routes::SESSIONS_PATH), headers).await?;
  let listed_ids = listing
    .as_array()
    .context("the session list is not a JSON array")?
    .iter()
    .map(|row| row["id"].as_str().map(str::to_string).context("a session row names no id"))
    .collect::<Result<Vec<String>>>()?;
  let tasks = get_json(&client, &format!("{base}{TASKS_PATH}"), headers).await?;
  let listed_tasks = tasks.as_array().context("the scheduled task list is not a JSON array")?;
  failures.extend(session_count_mismatches(manifest, &listed_ids, listed_tasks));

  for session in checked {
    let events_path = route::fill(session_routes::SESSION_EVENTS_PATH, &[session.id.as_str()]);
    let url = format!("{base}{events_path}?before={NEWEST_PAGE_BEFORE}");
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

  for task in &manifest.cron_tasks {
    let row = listed_tasks.iter().find(|row| row["name"].as_str() == Some(task.name.as_str()));
    match row {
      None => failures.push(format!("scheduled task {}: the server lists no such task", task.name)),
      Some(row) if row["broken"] == true => {
        failures.push(format!("scheduled task {}: the server reads it as broken: {}", task.name, row["error"]));
      }
      Some(row) if row["session_id"].as_str() != Some(task.session_id.as_str()) => failures.push(format!(
        "scheduled task {}: manifest session {}, server session {}",
        task.name, task.session_id, row["session_id"]
      )),
      Some(_) => {}
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

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;
  use crate::manifest::{ManifestCronTask, ManifestSession};

  /// A manifest of two sessions, `a` and `b`, that binds the fixture task `fixture` to `a`.
  fn manifest() -> Manifest {
    let session = |id: &str| ManifestSession { id: id.to_string(), event_log_bytes: 1, last_message: String::new() };
    Manifest {
      seed: 1,
      session_count: 2,
      event_log_bytes: 2,
      sessions: vec![session("a"), session("b")],
      shape: Default::default(),
      language: Default::default(),
      cron_tasks: vec![ManifestCronTask { name: "fixture".to_string(), session_id: "a".to_string() }],
    }
  }

  fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
  }

  #[test]
  fn a_listing_of_the_manifest_sessions_matches() {
    let tasks = [json!({"name": "fixture", "session_id": "a"})];
    assert!(session_count_mismatches(&manifest(), &ids(&["a", "b"]), &tasks).is_empty());
  }

  #[test]
  fn a_session_a_default_task_binds_counts_beside_the_manifest_sessions() {
    // The seeded default task is disabled, and the scheduler still bound its session on its first tick.
    let tasks = [json!({"name": "fixture", "session_id": "a"}), json!({"name": "storage-cool", "session_id": "c"})];
    assert!(session_count_mismatches(&manifest(), &ids(&["a", "b", "c"]), &tasks).is_empty());
  }

  #[test]
  fn a_default_task_without_a_session_yet_adds_nothing() {
    let tasks = [json!({"name": "storage-cool", "session_id": null})];
    assert!(session_count_mismatches(&manifest(), &ids(&["a", "b"]), &tasks).is_empty());
  }

  #[test]
  fn an_extra_session_that_no_default_task_binds_is_named() {
    let tasks = [json!({"name": "fixture", "session_id": "a"})];
    let failures = session_count_mismatches(&manifest(), &ids(&["a", "b", "x"]), &tasks);
    assert_eq!(failures.len(), 2, "{failures:?}");
    assert!(failures[0].contains("session x"), "{failures:?}");
    assert!(failures[1].contains("session count: manifest 2, server 3"), "{failures:?}");
  }

  #[test]
  fn a_session_only_a_fixture_task_binds_is_not_an_auto_bound_session() {
    let tasks = [json!({"name": "fixture", "session_id": "a"}), json!({"name": "fixture", "session_id": "x"})];
    let failures = session_count_mismatches(&manifest(), &ids(&["a", "b", "x"]), &tasks);
    assert!(failures.iter().any(|failure| failure.contains("session x")), "{failures:?}");
  }

  #[test]
  fn a_manifest_session_the_server_does_not_list_is_named() {
    let tasks = [json!({"name": "fixture", "session_id": "a"})];
    let failures = session_count_mismatches(&manifest(), &ids(&["a"]), &tasks);
    assert_eq!(failures.len(), 2, "{failures:?}");
    assert!(failures[0].contains("session b"), "{failures:?}");
  }
}
