//! The state file and the probe round.
//!
//! A round probes every host whose backoff has run out, publishes the result to `<home>/host_auth.json`, and holds
//! the `probe_running` claim while it probes. The remote side sees one read-only command per probe. A probe that
//! changes a host to `ok` from a router rejection or an interactive check approves the host's bound login.

use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use charliebot_core::{home, paths};
use charliebot_registry::registry::HostLogin;
use futures_util::future::join_all;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;

use crate::config::{BrowserLoginConfig, login_bound_to};
use crate::login_state;
use crate::probe::{ProbeStatus, build_detail, classify, probe_host_fresh};
use crate::ssh_config;
use crate::timestamps::{self, isoformat};

#[cfg(test)]
pub use crate::state_file::empty_state;
pub use crate::state_file::{PROBE_INTERVAL_SEC, RoundSlot, State, load_state, read_state, save_state};

pub const BLOCKED_BACKOFF_SEC: i64 = 21600;

pub const STATE_FILENAME: &str = "host_auth.json";

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

/// The verdict that the entry's stored `status` word names, or `None` when the entry holds no probe verdict.
fn stored_verdict(entry: &Map<String, Value>) -> Option<ProbeStatus> {
  entry.get("status").and_then(Value::as_str).and_then(ProbeStatus::from_word)
}

/// `host_due`: whether the host's backoff has run out. A host with no record, or with no probe yet, is always due.
pub fn host_due(entry: Option<&Map<String, Value>>, now: OffsetDateTime) -> anyhow::Result<bool> {
  let Some(entry) = entry else {
    return Ok(true);
  };
  let Some(last) = opt_str(entry, "last_probe_at")? else {
    return Ok(true);
  };
  let held = stored_verdict(entry) == Some(ProbeStatus::NeedsOkta);
  let period = if held { BLOCKED_BACKOFF_SEC } else { PROBE_INTERVAL_SEC };
  let last = timestamps::parse(last)?;
  Ok((now - last).whole_microseconds() >= i128::from(period) * 1_000_000)
}

/// `apply_probe_result`: fold one probe into the host's entry. Returns whether the probe approves the host's login: the
/// answer is `ok` and the host held a router rejection or an interactive check before it.
pub fn apply_probe_result(entry: &mut Map<String, Value>, status: ProbeStatus, detail: &str, probed: &str) -> bool {
  let previous = stored_verdict(entry);
  entry.insert("status".to_string(), json!(status.as_str()));
  entry.insert("detail".to_string(), json!(detail));
  entry.insert("last_probe_at".to_string(), json!(probed));
  if status == ProbeStatus::Ok {
    entry.insert("last_ok_at".to_string(), json!(probed));
    entry.remove("dm_status");
  }
  if previous != Some(status) {
    entry.insert("last_change_at".to_string(), json!(probed));
  }
  status == ProbeStatus::Ok && matches!(previous, Some(ProbeStatus::RouterRejected | ProbeStatus::NeedsInteractiveAuth))
}

/// `fold_probe`: apply one probe to the host at `index`. A probe that approves the host records the approval on the
/// login bound to that host. An ok probe clears that login's stored `broken` status and its stored link result, since
/// the host answering ok is the login's availability. The records are part of the same state write.
fn fold_probe(
  state: &mut State,
  logins: &[BrowserLoginConfig],
  index: usize,
  verdict: (ProbeStatus, &str),
  probed: &str,
) -> anyhow::Result<()> {
  let entry = host_entry_mut(state, index)?;
  let alias = alias_of(entry)?.to_string();
  let approved = apply_probe_result(entry, verdict.0, verdict.1, probed);
  if let Some(name) = login_bound_to(logins, &alias) {
    if approved {
      login_state::change_login(state, name, |record| record.last_approved_at = Some(probed.to_string()))?;
    }
    if verdict.0 == ProbeStatus::Ok {
      login_state::clear_broken(state, name)?;
      login_state::clear_link_match(state, name)?;
    }
  }
  Ok(())
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
    awaits_approval: stored_verdict(entry).is_some_and(ProbeStatus::awaits_approval),
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
  }) else {
    unreachable!("an object literal builds an object");
  };
  entry
}

/// `state`'s host list, rebuilt in config order. The config alone decides the set. An existing entry keeps
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
pub async fn run_round(
  force: bool,
  state_path: &Path,
  config_path: &Path,
  logins: &[BrowserLoginConfig],
) -> anyhow::Result<State> {
  run_round_with(force, state_path, config_path, logins, probe_host_fresh).await
}

/// [`run_round`] with the probe as a parameter, so the round can be driven without ssh.
pub async fn run_round_with<P, F>(
  force: bool,
  state_path: &Path,
  config_path: &Path,
  logins: &[BrowserLoginConfig],
  probe: P,
) -> anyhow::Result<State>
where
  P: Fn(String) -> F,
  F: Future<Output = io::Result<(i32, String)>>,
{
  run_round_with_now(force, state_path, config_path, logins, timestamps::utc_now(), probe).await
}

/// [`run_round_with`] with an explicit clock, for paused-clock verification.
pub async fn run_round_with_now<P, F>(
  force: bool,
  state_path: &Path,
  config_path: &Path,
  logins: &[BrowserLoginConfig],
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
    fold_probe(
      &mut claim.state,
      logins,
      index,
      (classify(*returncode, output), &build_detail(*returncode, output)),
      &probed,
    )?;
  }
  claim.state.insert("probed_at".to_string(), json!(probed));
  let state = claim.release()?;
  tracing::info!(hosts = host_list.len(), probed = due.len(), forced = force, "host_auth_probe_round");
  Ok(state)
}

/// The answer of one single-host probe: the host's record, and the probe's return code.
#[derive(Debug)]
pub struct ProbeAnswer {
  pub login: HostLogin,
  pub returncode: i32,
}

/// `probe_alias_with`: probe one host of the ssh config over a new connection, then write its verdict into the state
/// and return the host's record. A verdict that approves the host records the approval on the login bound to it in
/// `logins`. The verdict is written under the round slot, after the state is read there, so a round that holds the
/// slot cannot publish a state that drops this verdict.
pub async fn probe_alias_with<P, F>(
  state_path: &Path,
  config_path: &Path,
  alias: &str,
  logins: &[BrowserLoginConfig],
  probe: P,
) -> anyhow::Result<ProbeAnswer>
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
  fold_probe(&mut state, logins, index, (verdict, &detail), &probed)?;
  let login = host_login(host_entries(&state)?[index])?.context("the probed host holds no verdict")?;
  save_state(&state, state_path)?;
  Ok(ProbeAnswer { login, returncode })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::login_state::{LinkMatch, LoginStatus};
  use crate::probe::{ProbeStatus, build_detail};
  use time::Duration;

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
  fn only_a_router_or_interactive_host_that_answers_ok_is_approved() {
    let moment = "2026-09-15T08:05:00+00:00";
    let mut router =
      entry("login-a", "login-a.example.internal", &[("status", json!(ProbeStatus::RouterRejected.as_str()))]);
    assert!(apply_probe_result(&mut router, ProbeStatus::Ok, "exit 0", moment), "router-rejected to ok approves");
    assert_eq!(router["last_ok_at"], json!(moment));

    let mut interactive =
      entry("login-b", "login-b.example.internal", &[("status", json!(ProbeStatus::NeedsInteractiveAuth.as_str()))]);
    assert!(
      apply_probe_result(&mut interactive, ProbeStatus::Ok, "exit 0", moment),
      "needs-interactive-auth to ok approves"
    );

    let mut held = entry("login-c", "login-c.example.internal", &[("status", json!(ProbeStatus::NeedsOkta.as_str()))]);
    assert!(
      !apply_probe_result(&mut held, ProbeStatus::Ok, "exit 0", moment),
      "needs-okta to ok does not approve a login"
    );

    let mut repeat = entry("login-d", "login-d.example.internal", &[("status", json!(ProbeStatus::Ok.as_str()))]);
    assert!(!apply_probe_result(&mut repeat, ProbeStatus::Ok, "exit 0", moment), "a repeated ok approves nothing");
  }

  #[test]
  fn an_approving_probe_records_the_bound_logins_approval_in_the_same_state() {
    let logins = vec![BrowserLoginConfig {
      name: "router-login-a".to_string(),
      label: "login-a router".to_string(),
      host: Some("login-a".to_string()),
      login_command: vec!["/usr/bin/true".to_string()],
      approval_url_pattern: r"https://\S*".to_string(),
      readback_command: vec!["/usr/bin/true".to_string()],
      use_pty: false,
      enter_after_pattern: None,
      user_code_pattern: None,
      input_prompt: None,
      lifetime_sec: None,
    }];
    let mut state = empty_state();
    state.insert(
      "hosts".to_string(),
      json!([entry("login-a", "login-a.example.internal", &[("status", json!(ProbeStatus::RouterRejected.as_str()))])]),
    );
    let probed = "2026-09-15T08:05:00+00:00";
    fold_probe(&mut state, &logins, 0, (ProbeStatus::Ok, "exit 0"), probed).expect("the probe folds");
    let record = login_state::login_state(&state, "router-login-a").expect("the record reads");
    assert_eq!(record.last_approved_at.as_deref(), Some(probed));
    fold_probe(&mut state, &logins, 0, (ProbeStatus::Unreachable, "exit 255"), "2026-09-15T08:35:00+00:00")
      .expect("the probe folds");
    let record = login_state::login_state(&state, "router-login-a").expect("the record reads");
    assert_eq!(record.last_approved_at.as_deref(), Some(probed), "a failing probe leaves the approval");
  }

  #[test]
  fn an_ok_probe_of_the_bound_host_clears_the_broken_status_and_a_row_check_does_not() {
    let logins = vec![BrowserLoginConfig {
      name: "router-login-a".to_string(),
      label: "login-a router".to_string(),
      host: Some("login-a".to_string()),
      login_command: vec!["/usr/bin/true".to_string()],
      approval_url_pattern: r"https://\S*".to_string(),
      readback_command: vec!["/usr/bin/true".to_string()],
      use_pty: false,
      enter_after_pattern: None,
      user_code_pattern: None,
      input_prompt: None,
      lifetime_sec: None,
    }];
    let mut state = empty_state();
    state.insert(
      "hosts".to_string(),
      json!([entry("login-a", "login-a.example.internal", &[("status", json!(ProbeStatus::RouterRejected.as_str()))])]),
    );
    let broken = |state: &mut State| {
      login_state::change_login(state, "router-login-a", |record| {
        record.status = Some(LoginStatus::Broken);
        record.link_match = Some(LinkMatch::Fail);
      })
      .expect("the record changes");
    };
    let probed = "2026-11-19T09:54:09+00:00";

    broken(&mut state);
    fold_probe(&mut state, &[], 0, (ProbeStatus::Ok, "exit 0"), probed).expect("the row check's probe folds");
    let record = login_state::login_state(&state, "router-login-a").expect("the record reads");
    assert_eq!(record.status, Some(LoginStatus::Broken), "a host row check passes no login, so it clears nothing");
    assert_eq!(record.link_match, Some(LinkMatch::Fail));

    fold_probe(&mut state, &logins, 0, (ProbeStatus::Unreachable, "exit 255"), probed).expect("the probe folds");
    let record = login_state::login_state(&state, "router-login-a").expect("the record reads");
    assert_eq!(record.status, Some(LoginStatus::Broken), "a probe that does not answer ok keeps the broken status");

    fold_probe(&mut state, &logins, 0, (ProbeStatus::Ok, "exit 0"), probed).expect("the bound probe folds");
    let record = login_state::login_state(&state, "router-login-a").expect("the record reads");
    assert_eq!(record.status, None, "the bound host's ok clears the stored broken status");
    assert_eq!(record.link_match, None, "and its stored link result");
  }

  #[test]
  fn ok_deletes_dm_status_and_non_ok_leaves_it() {
    let mut item = entry(
      "login-a",
      "login-a.example.internal",
      &[("status", json!(ProbeStatus::NeedsInteractiveAuth.as_str())), ("dm_status", json!("needs-interactive-auth"))],
    );
    apply_probe_result(&mut item, ProbeStatus::Ok, "exit 0", "2026-09-15T08:00:00+00:00");
    assert_eq!(item.get("dm_status"), None);

    let mut awaiting = entry(
      "login-b",
      "login-b.example.internal",
      &[("status", json!(ProbeStatus::NeedsOkta.as_str())), ("dm_status", json!("needs-okta"))],
    );
    apply_probe_result(&mut awaiting, ProbeStatus::NeedsInteractiveAuth, "check required", "2026-09-15T08:00:00+00:00");
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
      &[("status", json!(ProbeStatus::NeedsOkta.as_str())), ("last_probe_at", probed_30_min_ago.clone())],
    );
    let direct = entry(
      "login-b",
      "login-b.example.internal",
      &[("status", json!(ProbeStatus::Ok.as_str())), ("last_probe_at", probed_30_min_ago)],
    );
    assert!(!host_due(Some(&held), now).expect("the stamp parses"));
    assert!(host_due(Some(&held), now + Duration::seconds(21600)).expect("the stamp parses"));
    assert!(host_due(Some(&direct), now).expect("the stamp parses"));
    assert!(host_due(None, now).expect("no record is due"));
  }

  #[test]
  fn the_state_file_is_json_with_indent_two_and_a_newline() {
    let dir = tempfile::tempdir().expect("a temporary home");
    let path = dir.path().join("nested").join(STATE_FILENAME);
    save_state(&empty_state(), &path).expect("the state is written");
    let expected = "{\n  \"probed_at\": null,\n  \"probe_running\": false,\n  \"interval_sec\": 1620,\n  \
                    \"hosts\": []\n}\n";
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
        &[("status", json!(ProbeStatus::NeedsOkta.as_str())), ("last_probe_at", json!(minute_ago)),]
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

    let state = run_round_with(false, &state_path, &config, &[], &probe).await.expect("the round runs");
    // The held host probed a minute ago is inside its backoff. The new host is always due.
    assert_eq!(*probed.lock().expect("the probe log"), ["gpu-box-1"]);
    let hosts = host_entries(&state).expect("the hosts read");
    assert_eq!(hosts[0]["alias"], json!("login-a"));
    assert_eq!(hosts[0]["status"], json!(ProbeStatus::NeedsOkta.as_str()));
    assert_eq!(hosts[1]["status"], json!(ProbeStatus::Unreachable.as_str()));
    assert_eq!(state["probe_running"], json!(false));
    assert_eq!(state["interval_sec"], json!(1620));
    let published = load_state(&state_path);
    let aliases: Vec<&str> =
      host_entries(&published).expect("the hosts read").iter().map(|e| e["alias"].as_str().unwrap()).collect();
    assert_eq!(aliases, ["login-a", "gpu-box-1"]);

    let state = run_round_with(true, &state_path, &config, &[], &probe).await.expect("the forced round runs");
    assert_eq!(*probed.lock().expect("the probe log"), ["gpu-box-1", "login-a", "gpu-box-1"]);
    // The manual round ignores the backoff, and the held host's answer is now the host's status.
    let hosts = host_entries(&state).expect("the hosts read");
    assert_eq!(hosts[0]["status"], json!(ProbeStatus::Ok.as_str()));
    assert!(hosts[0]["last_ok_at"].is_string());
  }

  #[tokio::test]
  async fn a_failed_probe_leaves_the_claim_released_and_the_state_unmerged() {
    let dir = tempfile::tempdir().expect("a temporary home");
    let config = dir.path().join("ssh_config");
    fs::write(&config, "Host gpu-box-1\n  HostName gpu-box-1.example.internal\n").expect("the config writes");
    let state_path = dir.path().join("state.json");
    let probe = |_alias: String| async { Err::<(i32, String), _>(io::Error::other("ssh is missing")) };
    let error = run_round_with(true, &state_path, &config, &[], probe).await.expect_err("the round fails");
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

    let login = probe_alias_with(&state_path, &config, "gpu-box-2", &[], check).await.expect("the probe answers").login;
    assert_eq!(login.status, ProbeStatus::NeedsInteractiveAuth.as_str());
    assert!(login.awaits_approval);
    assert_eq!(login.detail, build_detail(1, CHECK_OUTPUT));
    // The state holds the verdict. gpu-box-1 has no record until a probe answers it, so the read lists gpu-box-2 alone.
    let state = read_state(&state_path).expect("the state reads");
    assert_eq!(host_logins(&state).expect("the records read"), std::slice::from_ref(&login));

    // The same verdict again keeps the time it first changed.
    let again = probe_alias_with(&state_path, &config, "gpu-box-2", &[], check).await.expect("the probe answers").login;
    assert_eq!(again.last_change_at, login.last_change_at);

    let cleared = probe_alias_with(&state_path, &config, "gpu-box-2", &[], |_alias: String| async {
      Ok::<_, io::Error>((0, String::new()))
    })
    .await
    .expect("the probe answers")
    .login;
    assert_eq!(cleared.status, ProbeStatus::Ok.as_str());
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

    let error = probe_alias_with(&state_path, &config, "host9", &[], probe).await.expect_err("the alias is unknown");
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
      probe_alias_with(&gate_state, &config, "gpu-box-1", &[], ok).await
    });

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!gate.is_finished(), "the gate's write waits while a round holds the slot");
    assert!(!state_path.exists(), "nothing is written while the slot is held");
    drop(held);
    let login = gate.await.expect("the gate task joins").expect("the gate write succeeds").login;
    assert_eq!(login.status, ProbeStatus::Ok.as_str());
  }

  #[tokio::test]
  async fn a_gate_write_refuses_a_state_file_it_cannot_read_and_leaves_it_as_it_is() {
    let dir = tempfile::tempdir().expect("a temporary home");
    let config = dir.path().join("ssh_config");
    fs::write(&config, LOGIN_CONFIG).expect("the config writes");
    let state_path = dir.path().join("state.json");
    fs::write(&state_path, "{not json").expect("the corrupt state writes");
    let error = probe_alias_with(&state_path, &config, "gpu-box-2", &[], |_alias: String| async {
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
