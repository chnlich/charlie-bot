//! The state file, the probe round, the one-transition baseline and the estimate.
//!
//! A round probes every host whose backoff has run out, publishes the result to `<home>/host_auth.json`, and holds
//! the `probe_running` claim while it probes. The remote side sees one read-only command per probe.

use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use charliebot_core::text_repr::{self, JsonStyle};
use charliebot_core::{fsutil, home, paths};
use charliebot_registry::registry::HostLogin;
use futures_util::future::join_all;
use serde_json::{Map, Value, json};
use time::{Duration, OffsetDateTime};
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::probe::{STATUS_NEEDS_OKTA, STATUS_OK, awaits_approval, build_detail, classify, probe_host_fresh};
use crate::ssh_config;
use crate::timestamps::{self, isoformat};

/// The standing probe period. A held host is probed at its own backoff period.
pub const PROBE_INTERVAL_SEC: i64 = 1620;
pub const BLOCKED_BACKOFF_SEC: i64 = 21600;
/// The trust-cache lifetime the estimate counts from a renewal.
pub const TRUST_CACHE_TTL_SEC: i64 = 7 * 86400;

pub const STATE_FILENAME: &str = "host_auth.json";

/// The state file's object, with its keys in file order.
pub type State = Map<String, Value>;

/// The state file's layout: JSON with two-space indent, non-ASCII text literal and a trailing newline, written atomically.
const STATE_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ",", key_separator: ": ", indent: Some(2) };

/// The state file of this profile and the operator's ssh config: the two paths a round reads.
pub fn default_paths() -> anyhow::Result<(PathBuf, PathBuf)> {
  Ok((default_state_path()?, ssh_config_path()))
}

pub fn default_state_path() -> anyhow::Result<PathBuf> {
  Ok(home::charliebot_home_dir()?.join(STATE_FILENAME))
}

/// The operator's ssh config, whose `Host` blocks name the probed aliases.
pub fn ssh_config_path() -> PathBuf {
  PathBuf::from(paths::expanduser("~")).join(".ssh").join("config")
}

/// `empty_state`: the state a fresh install serves.
pub fn empty_state() -> State {
  let Value::Object(state) = json!({
    "probed_at": null,
    "probe_running": false,
    "interval_sec": PROBE_INTERVAL_SEC,
    "ttl_sec": TRUST_CACHE_TTL_SEC,
    "hosts": [],
  }) else {
    unreachable!("an object literal builds an object");
  };
  state
}

/// `read_state`: the state file, or the empty state when it is absent. A file that is unreadable, not UTF-8 JSON, or
/// not a state object is an error. A reader that decides on the hosts' verdicts must not take such a file for no hosts.
pub fn read_state(path: &Path) -> anyhow::Result<State> {
  match fs::read(path) {
    Ok(bytes) => parse_state(&bytes).map_err(|error| anyhow::anyhow!("{}: {error}", path.display())),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(empty_state()),
    Err(error) => Err(anyhow::Error::from(error).context(format!("reading {}", path.display()))),
  }
}

/// `load_state`: the state the page and the poller serve. A state file that `read_state` refuses is logged and answered
/// with the empty state.
pub fn load_state(path: &Path) -> State {
  read_state(path).unwrap_or_else(|error| {
    tracing::warn!(path = %path.display(), error = %format!("{error:#}"), "host_auth_state_read_failed");
    empty_state()
  })
}

fn parse_state(bytes: &[u8]) -> Result<State, String> {
  let text = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
  match serde_json::from_str::<Value>(text).map_err(|error| error.to_string())? {
    Value::Object(state) if state.get("hosts").is_some_and(Value::is_array) => Ok(state),
    _ => Err("state file does not hold a host_auth state object".to_string()),
  }
}

/// `save_state`: publish the state through a temporary file and a rename. A failed write is logged and returned.
pub fn save_state(state: &State, path: &Path) -> anyhow::Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)?;
  }
  let text = format!("{}\n", text_repr::json_dumps(&Value::Object(state.clone()), &STATE_JSON));
  if let Err(error) = fsutil::atomic_write_text(path, &text) {
    tracing::error!(path = %path.display(), error = %error, "host_auth_state_write_failed");
    return Err(error.into());
  }
  Ok(())
}

/// `host_due`: whether the host's backoff has run out. A host with no record, or with no probe yet, is always due.
pub fn host_due(entry: Option<&Map<String, Value>>, now: OffsetDateTime) -> anyhow::Result<bool> {
  let Some(entry) = entry else {
    return Ok(true);
  };
  let Some(last) = opt_str(entry, "last_probe_at")? else {
    return Ok(true);
  };
  let held = entry.get("status").and_then(Value::as_str) == Some(STATUS_NEEDS_OKTA);
  let period = if held { BLOCKED_BACKOFF_SEC } else { PROBE_INTERVAL_SEC };
  let last = timestamps::parse(last)?;
  Ok((now - last).whole_microseconds() >= i128::from(period) * 1_000_000)
}

/// `apply_probe_result`: fold one probe into the entry under the one-transition baseline rule. The baseline moves
/// only on the probe after a held probe answers. A repeat of an answer leaves it, so the estimate stays an upper bound.
pub fn apply_probe_result(entry: &mut Map<String, Value>, status: &str, detail: &str, probed: &str) {
  let previous = entry.get("status").and_then(Value::as_str).map(str::to_owned);
  entry.insert("status".to_string(), json!(status));
  entry.insert("detail".to_string(), json!(detail));
  entry.insert("last_probe_at".to_string(), json!(probed));
  if status == STATUS_OK {
    entry.insert("last_ok_at".to_string(), json!(probed));
    entry.remove("dm_status");
  }
  if previous.as_deref() != Some(status) {
    entry.insert("last_change_at".to_string(), json!(probed));
  }
  if previous.as_deref() == Some(STATUS_NEEDS_OKTA) && status == STATUS_OK {
    record_renewal(entry, probed);
  }
}

/// `record_renewal`: the owner of the baseline read by [`derive_estimate`].
fn record_renewal(entry: &mut Map<String, Value>, renewed_at: &str) {
  entry.insert("enrolled_observed_at".to_string(), json!(renewed_at));
}

/// The estimate of one host, derived at read time and never stored.
pub struct Estimate {
  pub expires_at: Option<String>,
  pub remaining_sec: Option<f64>,
}

/// `derive_estimate`: the renewal baseline plus the trust-cache lifetime, and the seconds left until it.
pub fn derive_estimate(entry: &Map<String, Value>, now: OffsetDateTime) -> anyhow::Result<Estimate> {
  let Some(baseline) = opt_str(entry, "enrolled_observed_at")? else {
    return Ok(Estimate { expires_at: None, remaining_sec: None });
  };
  let expires = timestamps::parse(baseline)? + Duration::seconds(TRUST_CACHE_TTL_SEC);
  Ok(Estimate {
    expires_at: Some(isoformat(expires)),
    remaining_sec: Some((expires - now).whole_microseconds() as f64 / 1_000_000.0),
  })
}

/// The string at `key`, or `None` when the key is absent or null. Any other type is an error.
pub fn opt_str<'a>(entry: &'a Map<String, Value>, key: &str) -> anyhow::Result<Option<&'a str>> {
  match entry.get(key) {
    None | Some(Value::Null) => Ok(None),
    Some(Value::String(text)) => Ok(Some(text)),
    Some(other) => anyhow::bail!("host entry field {key} holds {other}, not a string"),
  }
}

/// The host entries of the state, in file order.
pub fn host_entries(state: &State) -> anyhow::Result<Vec<&Map<String, Value>>> {
  let hosts = state.get("hosts").and_then(Value::as_array).context("the state holds no host list")?;
  hosts.iter().map(|entry| entry.as_object().context("a host entry is not an object")).collect()
}

pub fn alias_of(entry: &Map<String, Value>) -> anyhow::Result<&str> {
  entry.get("alias").and_then(Value::as_str).context("a host entry has no string alias")
}

/// The login record of a host that a probe has answered, or `None` for a host no probe has answered yet.
fn host_login(entry: &Map<String, Value>) -> anyhow::Result<Option<HostLogin>> {
  let alias = alias_of(entry)?;
  let Some(status) = opt_str(entry, "status")? else {
    return Ok(None);
  };
  let detail = opt_str(entry, "detail")?.with_context(|| format!("host {alias} has a status but no detail"))?;
  let last_change_at =
    opt_str(entry, "last_change_at")?.with_context(|| format!("host {alias} has a status but no last_change_at"))?;
  Ok(Some(HostLogin {
    alias: alias.to_string(),
    status: status.to_string(),
    detail: detail.to_string(),
    last_change_at: last_change_at.to_string(),
    awaits_approval: awaits_approval(status),
  }))
}

/// The login records of the probed hosts, in state order.
pub fn host_logins(state: &State) -> anyhow::Result<Vec<HostLogin>> {
  let mut logins = Vec::new();
  for entry in host_entries(state)? {
    logins.extend(host_login(entry)?);
  }
  Ok(logins)
}

fn new_entry(alias: &str, hostname: &str) -> Map<String, Value> {
  let Value::Object(entry) = json!({
    "alias": alias,
    "hostname": hostname,
    "status": null,
    "detail": "",
    "last_probe_at": null,
    "last_change_at": null,
    "last_ok_at": null,
    "enrolled_observed_at": null,
  }) else {
    unreachable!("an object literal builds an object");
  };
  entry
}

/// Rebuild the host list in config order. The config alone decides the set. An existing entry keeps
/// its history and takes the config's `HostName`.
fn merge_hosts(state: &mut State, host_list: &[(String, String)]) -> anyhow::Result<()> {
  let mut previous: HashMap<String, Map<String, Value>> = HashMap::new();
  for entry in host_entries(state)? {
    previous.insert(alias_of(entry)?.to_string(), entry.clone());
  }
  let mut merged = Vec::with_capacity(host_list.len());
  for (alias, hostname) in host_list {
    let mut entry = previous.remove(alias.as_str()).unwrap_or_else(|| new_entry(alias, hostname));
    entry.insert("hostname".to_string(), json!(hostname));
    merged.push(Value::Object(entry));
  }
  state.insert("hosts".to_string(), Value::Array(merged));
  state.insert("interval_sec".to_string(), json!(PROBE_INTERVAL_SEC));
  state.insert("ttl_sec".to_string(), json!(TRUST_CACHE_TTL_SEC));
  Ok(())
}

fn host_entry_mut(state: &mut State, index: usize) -> anyhow::Result<&mut Map<String, Value>> {
  let hosts = state.get_mut("hosts").and_then(Value::as_array_mut).context("the state holds no host list")?;
  hosts.get_mut(index).and_then(Value::as_object_mut).context("a merged host entry is not an object")
}

/// The indices, in config order, of the hosts this round probes: all of them when `force`, else those due.
fn due_indices(
  state: &State,
  host_list: &[(String, String)],
  force: bool,
  now: OffsetDateTime,
) -> anyhow::Result<Vec<usize>> {
  let mut entries: HashMap<&str, &Map<String, Value>> = HashMap::new();
  for entry in host_entries(state)? {
    entries.insert(alias_of(entry)?, entry);
  }
  let mut due = Vec::new();
  for (index, (alias, _)) in host_list.iter().enumerate() {
    if force || host_due(entries.get(alias.as_str()).copied(), now)? {
      due.push(index);
    }
  }
  Ok(due)
}

/// The ssh config's collectable hosts. A missing file holds none. The file is read as UTF-8.
fn read_config_hosts(path: &Path) -> anyhow::Result<Vec<(String, String)>> {
  match fs::read_to_string(path) {
    Ok(text) => ssh_config::parse_hosts(&text),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
    Err(error) => Err(error.into()),
  }
}

/// `probe_running` held for the whole round. It is set before the probes and released on every path: `release`
/// saves the finished state, and a drop saves the released claim when the round ends early.
struct ProbeClaim<'a> {
  path: &'a Path,
  state: State,
  released: bool,
}

impl<'a> ProbeClaim<'a> {
  fn start(path: &'a Path, mut state: State) -> anyhow::Result<ProbeClaim<'a>> {
    state.insert("probe_running".to_string(), json!(true));
    save_state(&state, path)?;
    Ok(ProbeClaim { path, state, released: false })
  }

  fn release(mut self) -> anyhow::Result<State> {
    self.state.insert("probe_running".to_string(), json!(false));
    self.released = true;
    save_state(&self.state, self.path)?;
    Ok(std::mem::take(&mut self.state))
  }
}

impl Drop for ProbeClaim<'_> {
  fn drop(&mut self) {
    if self.released {
      return;
    }
    self.state.insert("probe_running".to_string(), json!(false));
    // `save_state` logs its own failure, and a drop has no way to return one.
    let _ = save_state(&self.state, self.path);
  }
}

/// `run_round`: probe the hosts that are due (every host when `force`), then publish the state.
pub async fn run_round(force: bool, state_path: &Path, config_path: &Path) -> anyhow::Result<State> {
  run_round_with(force, state_path, config_path, probe_host_fresh).await
}

/// [`run_round`] with the probe as a parameter, so the round can be driven without ssh.
pub async fn run_round_with<P, F>(force: bool, state_path: &Path, config_path: &Path, probe: P) -> anyhow::Result<State>
where
  P: Fn(String) -> F,
  F: Future<Output = io::Result<(i32, String)>>,
{
  run_round_with_now(force, state_path, config_path, timestamps::utc_now(), probe).await
}

/// [`run_round_with`] with an explicit clock, for paused-clock verification.
pub async fn run_round_with_now<P, F>(
  force: bool,
  state_path: &Path,
  config_path: &Path,
  now: OffsetDateTime,
  probe: P,
) -> anyhow::Result<State>
where
  P: Fn(String) -> F,
  F: Future<Output = io::Result<(i32, String)>>,
{
  let mut state = load_state(state_path);
  let host_list = read_config_hosts(config_path)?;
  let due = due_indices(&state, &host_list, force, now)?;
  if due.is_empty() {
    merge_hosts(&mut state, &host_list)?;
    state.insert("probe_running".to_string(), json!(false));
    save_state(&state, state_path)?;
    tracing::info!(hosts = host_list.len(), probed = 0, forced = force, "host_auth_probe_round");
    return Ok(state);
  }
  let mut claim = ProbeClaim::start(state_path, state)?;
  let results = join_all(due.iter().map(|&index| probe(host_list[index].0.clone())))
    .await
    .into_iter()
    .collect::<io::Result<Vec<_>>>()?;
  merge_hosts(&mut claim.state, &host_list)?;
  let moment = now;
  let probed = isoformat(moment);
  for (&index, (returncode, output)) in due.iter().zip(&results) {
    let entry = host_entry_mut(&mut claim.state, index)?;
    apply_probe_result(entry, classify(*returncode, output), &build_detail(*returncode, output), &probed);
  }
  claim.state.insert("probed_at".to_string(), json!(probed));
  let state = claim.release()?;
  tracing::info!(hosts = host_list.len(), probed = due.len(), forced = force, "host_auth_probe_round");
  Ok(state)
}

/// `probe_alias_now`: probe one host of the ssh config over a new connection, then write its verdict into the state
/// and return the host's record. The verdict is written under the round slot, after the state is read there, so a
/// round that holds the slot cannot publish a state that drops this verdict.
pub async fn probe_alias_with<P, F>(
  state_path: &Path,
  config_path: &Path,
  alias: &str,
  probe: P,
) -> anyhow::Result<HostLogin>
where
  P: Fn(String) -> F,
  F: Future<Output = io::Result<(i32, String)>>,
{
  let host_list = read_config_hosts(config_path)?;
  let index = host_list
    .iter()
    .position(|(name, _)| name == alias)
    .with_context(|| format!("host alias {alias:?} is not in the ssh config"))?;
  let (returncode, output) = probe(alias.to_string()).await?;
  let verdict = classify(returncode, &output);
  let detail = build_detail(returncode, &output);
  let probed = isoformat(timestamps::utc_now());
  let _slot = RoundSlot::wait().await;
  let mut state = read_state(state_path)?;
  merge_hosts(&mut state, &host_list)?;
  apply_probe_result(host_entry_mut(&mut state, index)?, verdict, &detail, &probed);
  let login = host_login(host_entries(&state)?[index])?.context("the probed host holds no verdict")?;
  save_state(&state, state_path)?;
  Ok(login)
}

/// Record a confirmed renewal for the named host without changing its probe verdict.
pub async fn record_host_renewal(state_path: &Path, alias: &str, renewed_at: OffsetDateTime) -> anyhow::Result<()> {
  let _slot = RoundSlot::wait().await;
  let mut state = read_state(state_path)?;
  let mut index = None;
  for (index_in_state, entry) in host_entries(&state)?.into_iter().enumerate() {
    if alias_of(entry)? == alias {
      index = Some(index_in_state);
      break;
    }
  }
  let index = index.with_context(|| format!("host alias {alias:?} has no host-auth state entry"))?;
  record_renewal(host_entry_mut(&mut state, index)?, &isoformat(renewed_at));
  save_state(&state, state_path)
}

/// The process-wide round slot: a manual round, the poller's tick and the cron gate's write-back never change the state
/// file together. `claim` takes it without waiting. `wait` waits while a round holds it. The slot is released when the
/// guard drops, on every path.
static ROUND_SLOT: Semaphore = Semaphore::const_new(1);

/// A held round slot. Dropping it releases the slot.
pub struct RoundSlot {
  _permit: SemaphorePermit<'static>,
}

impl RoundSlot {
  /// The slot, or `None` while a round holds it.
  pub fn claim() -> Option<RoundSlot> {
    ROUND_SLOT.try_acquire().ok().map(|permit| RoundSlot { _permit: permit })
  }

  /// The slot, once the round that holds it, if any, has released it.
  pub async fn wait() -> RoundSlot {
    RoundSlot { _permit: ROUND_SLOT.acquire().await.expect("the round slot is never closed") }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::probe::{STATUS_NEEDS_INTERACTIVE_AUTH, STATUS_UNREACHABLE, build_detail};

  fn entry(alias: &str, hostname: &str, overrides: &[(&str, Value)]) -> Map<String, Value> {
    let mut entry = new_entry(alias, hostname);
    for (key, value) in overrides {
      entry.insert(key.to_string(), value.clone());
    }
    entry
  }

  fn at(text: &str) -> OffsetDateTime {
    timestamps::parse(text).expect("the fixed stamp parses")
  }

  #[test]
  fn the_baseline_moves_on_the_held_to_direct_transition_and_not_on_a_repeat() {
    let mut adjacent = entry("login-a", "login-a.example.internal", &[("status", json!(STATUS_NEEDS_OKTA))]);
    let moment = "2026-09-15T08:05:00+00:00";
    apply_probe_result(&mut adjacent, STATUS_OK, "exit 0", moment);
    assert_eq!(adjacent["enrolled_observed_at"], json!(moment));
    apply_probe_result(&mut adjacent, STATUS_OK, "exit 0", "2026-09-15T08:35:00+00:00");
    assert_eq!(adjacent["enrolled_observed_at"], json!(moment));
  }

  #[test]
  fn ok_deletes_dm_status_and_non_ok_leaves_it() {
    let mut item = entry(
      "login-a",
      "login-a.example.internal",
      &[("status", json!(STATUS_NEEDS_INTERACTIVE_AUTH)), ("dm_status", json!("needs-interactive-auth"))],
    );
    apply_probe_result(&mut item, STATUS_OK, "exit 0", "2026-09-15T08:00:00+00:00");
    assert_eq!(item.get("dm_status"), None);

    let mut awaiting = entry(
      "login-b",
      "login-b.example.internal",
      &[("status", json!(STATUS_NEEDS_OKTA)), ("dm_status", json!("needs-okta"))],
    );
    apply_probe_result(&mut awaiting, STATUS_NEEDS_INTERACTIVE_AUTH, "check required", "2026-09-15T08:00:00+00:00");
    assert_eq!(awaiting["dm_status"], json!("needs-okta"));
  }

  #[test]
  fn a_held_host_backs_off_twelve_times_the_standing_period() {
    assert_eq!(BLOCKED_BACKOFF_SEC, 21600);
    let now = at("2026-09-15T08:00:00+00:00");
    let probed_30_min_ago = json!("2026-09-15T07:30:00+00:00");
    let held = entry(
      "login-a",
      "login-a.example.internal",
      &[("status", json!(STATUS_NEEDS_OKTA)), ("last_probe_at", probed_30_min_ago.clone())],
    );
    let direct = entry(
      "login-b",
      "login-b.example.internal",
      &[("status", json!(STATUS_OK)), ("last_probe_at", probed_30_min_ago)],
    );
    assert!(!host_due(Some(&held), now).expect("the stamp parses"));
    assert!(host_due(Some(&held), now + Duration::seconds(21600)).expect("the stamp parses"));
    assert!(host_due(Some(&direct), now).expect("the stamp parses"));
    assert!(host_due(None, now).expect("no record is due"));
  }

  #[test]
  fn an_estimate_counts_from_the_baseline_and_is_absent_without_one() {
    let now = at("2026-09-15T08:00:00+00:00");
    let baseline =
      entry("login-a", "login-a.example.internal", &[("enrolled_observed_at", json!("2026-09-15T07:00:00+00:00"))]);
    let estimate = derive_estimate(&baseline, now).expect("the baseline parses");
    assert_eq!(estimate.expires_at.as_deref(), Some("2026-09-22T07:00:00+00:00"));
    assert_eq!(estimate.remaining_sec, Some(604800.0 - 3600.0));
    let none = derive_estimate(&entry("login-b", "login-b.example.internal", &[]), now).expect("no baseline");
    assert!(none.expires_at.is_none() && none.remaining_sec.is_none());
  }

  #[test]
  fn the_state_file_is_json_with_indent_two_and_a_newline() {
    let dir = tempfile::tempdir().expect("a temporary home");
    let path = dir.path().join("nested").join(STATE_FILENAME);
    save_state(&empty_state(), &path).expect("the state is written");
    let expected = "{\n  \"probed_at\": null,\n  \"probe_running\": false,\n  \"interval_sec\": 1620,\n  \
                    \"ttl_sec\": 604800,\n  \"hosts\": []\n}\n";
    assert_eq!(fs::read_to_string(&path).expect("the state reads back"), expected);
  }

  #[test]
  fn a_corrupt_or_wrong_shaped_state_file_serves_the_empty_state() {
    let dir = tempfile::tempdir().expect("a temporary home");
    let path = dir.path().join(STATE_FILENAME);
    for text in ["{not json", "[1, 2]", "{\"hosts\": {}}"] {
      fs::write(&path, text).expect("the fixture writes");
      assert_eq!(load_state(&path), empty_state(), "for {text}");
    }
    assert_eq!(load_state(&dir.path().join("absent.json")), empty_state());
  }

  #[tokio::test]
  async fn a_round_probes_due_hosts_and_publishes_the_state() {
    let dir = tempfile::tempdir().expect("a temporary home");
    let config = dir.path().join("ssh_config");
    fs::write(
      &config,
      "Host login-a\n  HostName login-a.example.internal\n\nHost gpu-box-1\n  HostName gpu-box-1.example.internal\n",
    )
    .expect("the config writes");
    let state_path = dir.path().join("state.json");
    let minute_ago = isoformat(timestamps::utc_now() - Duration::minutes(1));
    let mut seed = empty_state();
    seed.insert(
      "hosts".to_string(),
      json!([entry(
        "login-a",
        "login-a.example.internal",
        &[("status", json!(STATUS_NEEDS_OKTA)), ("last_probe_at", json!(minute_ago)),]
      )]),
    );
    fs::write(&state_path, format!("{}\n", Value::Object(seed))).expect("the seed writes");

    let probed = std::sync::Mutex::new(Vec::new());
    let probe = |alias: String| {
      probed.lock().expect("the probe log is not poisoned").push(alias.clone());
      let outcome = if alias == "login-a" {
        (0, String::new())
      } else {
        (255, "ssh: connect to host port 22: Connection timed out".to_string())
      };
      async move { Ok(outcome) }
    };

    let state = run_round_with(false, &state_path, &config, &probe).await.expect("the round runs");
    // The held host probed a minute ago is inside its backoff. The new host is always due.
    assert_eq!(*probed.lock().expect("the probe log"), ["gpu-box-1"]);
    let hosts = host_entries(&state).expect("the hosts read");
    assert_eq!(hosts[0]["alias"], json!("login-a"));
    assert_eq!(hosts[0]["status"], json!(STATUS_NEEDS_OKTA));
    assert_eq!(hosts[1]["status"], json!(STATUS_UNREACHABLE));
    assert_eq!(state["probe_running"], json!(false));
    assert_eq!(state["interval_sec"], json!(1620));
    assert_eq!(state["ttl_sec"], json!(604800));
    let published = load_state(&state_path);
    let aliases: Vec<&str> =
      host_entries(&published).expect("the hosts read").iter().map(|e| e["alias"].as_str().unwrap()).collect();
    assert_eq!(aliases, ["login-a", "gpu-box-1"]);

    let state = run_round_with(true, &state_path, &config, &probe).await.expect("the forced round runs");
    assert_eq!(*probed.lock().expect("the probe log"), ["gpu-box-1", "login-a", "gpu-box-1"]);
    let hosts = host_entries(&state).expect("the hosts read");
    // The manual round ignores the backoff, and the held-to-direct transition writes the baseline.
    assert_eq!(hosts[0]["status"], json!(STATUS_OK));
    assert!(hosts[0]["enrolled_observed_at"].is_string());
    assert_eq!(hosts[1]["enrolled_observed_at"], Value::Null);
    let baseline = hosts[0]["enrolled_observed_at"].clone();

    let state = run_round_with(true, &state_path, &config, &probe).await.expect("the forced round runs");
    assert_eq!(host_entries(&state).expect("the hosts read")[0]["enrolled_observed_at"], baseline);
  }

  #[tokio::test]
  async fn a_failed_probe_leaves_the_claim_released_and_the_state_unmerged() {
    let dir = tempfile::tempdir().expect("a temporary home");
    let config = dir.path().join("ssh_config");
    fs::write(&config, "Host gpu-box-1\n  HostName gpu-box-1.example.internal\n").expect("the config writes");
    let state_path = dir.path().join("state.json");
    let probe = |_alias: String| async { Err::<(i32, String), _>(io::Error::other("ssh is missing")) };
    let error = run_round_with(true, &state_path, &config, probe).await.expect_err("the round fails");
    assert!(format!("{error:#}").contains("ssh is missing"));
    let published = load_state(&state_path);
    assert_eq!(published["probe_running"], json!(false));
    assert!(host_entries(&published).expect("the hosts read").is_empty());
  }

  const LOGIN_CONFIG: &str = "Host gpu-box-2\n  HostName gpu-box-2.example.internal\n\n\
                              Host gpu-box-1\n  HostName gpu-box-1.example.internal\n";
  const CHECK_OUTPUT: &str =
    "# Tailscale SSH requires an additional check.\nTo authenticate, visit: https://login.example.net/a\n";

  #[tokio::test]
  async fn a_gate_probe_writes_its_verdict_and_returns_the_host_record() {
    let dir = tempfile::tempdir().expect("a temporary home");
    let config = dir.path().join("ssh_config");
    fs::write(&config, LOGIN_CONFIG).expect("the config writes");
    let state_path = dir.path().join("state.json");
    let check = |_alias: String| async { Ok::<_, io::Error>((1, CHECK_OUTPUT.to_string())) };

    let login = probe_alias_with(&state_path, &config, "gpu-box-2", check).await.expect("the probe answers");
    assert_eq!(login.status, STATUS_NEEDS_INTERACTIVE_AUTH);
    assert!(login.awaits_approval);
    assert_eq!(login.detail, build_detail(1, CHECK_OUTPUT));
    // The state holds the verdict. gpu-box-1 has no record until a probe answers it, so the read lists gpu-box-2 alone.
    let state = read_state(&state_path).expect("the state reads");
    assert_eq!(host_logins(&state).expect("the records read"), std::slice::from_ref(&login));

    // The same verdict again keeps the time it first changed.
    let again = probe_alias_with(&state_path, &config, "gpu-box-2", check).await.expect("the probe answers");
    assert_eq!(again.last_change_at, login.last_change_at);

    let cleared = probe_alias_with(&state_path, &config, "gpu-box-2", |_alias: String| async {
      Ok::<_, io::Error>((0, String::new()))
    })
    .await
    .expect("the probe answers");
    assert_eq!(cleared.status, STATUS_OK);
    assert!(!cleared.awaits_approval);
    assert_eq!(cleared.detail, "exit 0");
  }

  #[tokio::test]
  async fn a_gate_probe_refuses_an_alias_the_ssh_config_does_not_name_before_it_probes() {
    let dir = tempfile::tempdir().expect("a temporary home");
    let config = dir.path().join("ssh_config");
    fs::write(&config, LOGIN_CONFIG).expect("the config writes");
    let state_path = dir.path().join("state.json");
    let probed = std::sync::atomic::AtomicBool::new(false);
    let probe = |_alias: String| {
      probed.store(true, std::sync::atomic::Ordering::Relaxed);
      async { Ok::<_, io::Error>((0, String::new())) }
    };

    let error = probe_alias_with(&state_path, &config, "host9", probe).await.expect_err("the alias is unknown");
    assert!(format!("{error:#}").contains("host9"), "{error:#}");
    assert!(!probed.load(std::sync::atomic::Ordering::Relaxed), "an unknown alias runs no probe");
    assert!(!state_path.exists(), "an unknown alias writes no state");
  }

  #[tokio::test(start_paused = true)]
  async fn a_gate_write_waits_for_the_round_that_holds_the_slot() {
    let _lock = crate::test_support::round_slot_lock().lock().await;
    let dir = tempfile::tempdir().expect("a temporary home");
    let config = dir.path().join("ssh_config");
    fs::write(&config, LOGIN_CONFIG).expect("the config writes");
    let state_path = dir.path().join("state.json");
    let held = RoundSlot::wait().await;
    let gate_state = state_path.clone();
    let gate = tokio::spawn(async move {
      let ok = |_alias: String| async { Ok::<_, io::Error>((0, String::new())) };
      probe_alias_with(&gate_state, &config, "gpu-box-1", ok).await
    });

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!gate.is_finished(), "the gate's write waits while a round holds the slot");
    assert!(!state_path.exists(), "nothing is written while the slot is held");
    drop(held);
    let login = gate.await.expect("the gate task joins").expect("the gate write succeeds");
    assert_eq!(login.status, STATUS_OK);
  }

  #[tokio::test]
  async fn a_gate_write_refuses_a_state_file_it_cannot_read_and_leaves_it_as_it_is() {
    let dir = tempfile::tempdir().expect("a temporary home");
    let config = dir.path().join("ssh_config");
    fs::write(&config, LOGIN_CONFIG).expect("the config writes");
    let state_path = dir.path().join("state.json");
    fs::write(&state_path, "{not json").expect("the corrupt state writes");
    let error = probe_alias_with(&state_path, &config, "gpu-box-2", |_alias: String| async {
      Ok::<_, io::Error>((0, String::new()))
    })
    .await
    .expect_err("the state file is corrupt");
    assert!(format!("{error:#}").contains(&state_path.display().to_string()), "{error:#}");
    assert_eq!(fs::read_to_string(&state_path).expect("the state reads"), "{not json");
  }

  #[test]
  fn read_state_refuses_a_corrupt_file_and_reads_an_absent_one_as_empty() {
    let dir = tempfile::tempdir().expect("a temporary home");
    let path = dir.path().join(STATE_FILENAME);
    fs::write(&path, "[1, 2]").expect("the fixture writes");
    assert!(read_state(&path).is_err());
    assert_eq!(read_state(&dir.path().join("absent.json")).expect("an absent state is empty"), empty_state());
  }
}
