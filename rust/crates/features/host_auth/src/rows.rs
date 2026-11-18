//! The status table's rows: one per configured browser login, then one per ssh host that no login renews.
//!
//! A row is built from the state file, the config and the live attempts, by the one function the page, the status route
//! and the row route share. A row's state is the classification of its latest check, from `attention`, and a link probe
//! that found no link overrides it. A check runs through [`check_row`], which persists its result before it answers.

use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use anyhow::bail;
use serde_json::{Map, Value, json};
use time::{Duration, OffsetDateTime};

use crate::attention::{Finding, RowState, classify_row_attention};
use crate::browser_login::{self, LiveAttempt, Readback, timeout_text};
use crate::config::{BrowserLoginConfig, login_bound_to};
use crate::host_auth::{self, State};
use crate::login_state::{self, LinkMatch, LoginState, LoginStatus};
use crate::timestamps::{self, LocalFormat, isoformat};

/// The detail of a row whose link probe found no approval link.
const LINK_NOT_FOUND_DETAIL: &str =
  "The link probe found no approval link: the login command is missing, did not start, or printed no matching link.";

/// A row's id: `login:<name>` for a configured login, `host:<alias>` for an ssh host that no login renews.
enum RowId<'a> {
  Login(&'a str),
  Host(&'a str),
}

fn parse_id(id: &str) -> Option<RowId<'_>> {
  match id.split_once(':')? {
    ("login", name) => Some(RowId::Login(name)),
    ("host", alias) => Some(RowId::Host(alias)),
    _ => None,
  }
}

/// The rows whose check runs now. The row's JSON reports a check in progress from this set.
static CHECKING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Marks a row as checking until the guard drops, on every path.
struct CheckGuard(String);

impl CheckGuard {
  fn start(id: &str) -> Self {
    CHECKING.lock().expect("the checking set is never poisoned").insert(id.to_string());
    CheckGuard(id.to_string())
  }
}

impl Drop for CheckGuard {
  fn drop(&mut self) {
    CHECKING.lock().expect("the checking set is never poisoned").remove(&self.0);
  }
}

/// One row before it is rendered.
struct Row<'a> {
  id: String,
  name: String,
  alias: Option<String>,
  hostname: Option<String>,
  login: Option<&'a BrowserLoginConfig>,
  state: RowState,
  detail: String,
  checked_at: Option<String>,
  approved_at: Option<String>,
  record: LoginState,
  live: Option<LiveAttempt>,
}

/// The state a login's own status stands for.
fn state_of_status(status: LoginStatus) -> RowState {
  match status {
    LoginStatus::Ok => RowState::Ok,
    LoginStatus::NeedsUser => RowState::NeedsUser,
    LoginStatus::Broken => RowState::Broken,
  }
}

/// The host entry of `alias`, when the state holds one.
fn host_of<'s>(hosts: &[&'s Map<String, Value>], alias: &str) -> anyhow::Result<Option<&'s Map<String, Value>>> {
  for entry in hosts {
    if host_auth::alias_of(entry)? == alias {
      return Ok(Some(entry));
    }
  }
  Ok(None)
}

/// The row of a configured login. A link probe that found no link makes the row broken, whatever its check said. A
/// host-bound login's availability is its host's probe, unless its own check stands broken, which a failed start does.
/// A login with no host shows its own check.
fn login_row<'a>(
  state: &State,
  hosts: &[&Map<String, Value>],
  login: &'a BrowserLoginConfig,
  live: Option<LiveAttempt>,
) -> anyhow::Result<Row<'a>> {
  let record = login_state::login_state(state, &login.name)?;
  let host = match &login.host {
    Some(alias) => host_of(hosts, alias)?,
    None => None,
  };
  let mut row = Row {
    id: format!("login:{}", login.name),
    name: login.name.clone(),
    alias: login.host.clone(),
    hostname: host.and_then(|entry| entry.get("hostname")).and_then(Value::as_str).map(str::to_string),
    login: Some(login),
    state: RowState::Unchecked,
    detail: String::new(),
    checked_at: None,
    approved_at: record.last_approved_at.clone(),
    record: record.clone(),
    live,
  };
  if record.link_match == Some(LinkMatch::Fail) {
    row.state = classify_row_attention(Finding::LinkNotFound)?;
    row.detail = LINK_NOT_FOUND_DETAIL.to_string();
    row.checked_at = record.last_check_at.clone();
    return Ok(row);
  }
  match (&login.host, host) {
    (Some(_), host) => match (record.status, host) {
      (Some(LoginStatus::Broken), _) => {
        row.state = RowState::Broken;
        row.detail = record.detail.clone().unwrap_or_default();
        row.checked_at = record.last_check_at.clone();
      }
      (_, Some(entry)) => {
        if let Some(status) = host_auth::opt_str(entry, "status")? {
          row.state = classify_row_attention(Finding::Probe(status))?;
          row.detail = host_auth::opt_str(entry, "detail")?.unwrap_or("").to_string();
          row.checked_at = host_auth::opt_str(entry, "last_probe_at")?.map(str::to_string);
        }
      }
      (_, None) => {}
    },
    (None, _) => {
      if let Some(status) = record.status {
        row.state = state_of_status(status);
        row.detail = record.detail.clone().unwrap_or_default();
        row.checked_at = record.last_check_at.clone();
      }
    }
  }
  Ok(row)
}

/// The row of an ssh host that no login renews. Its state is the host's probe.
fn host_row(entry: &Map<String, Value>) -> anyhow::Result<Row<'static>> {
  let alias = host_auth::alias_of(entry)?.to_string();
  let mut row = Row {
    id: format!("host:{alias}"),
    name: alias.clone(),
    alias: Some(alias),
    hostname: entry.get("hostname").and_then(Value::as_str).map(str::to_string),
    login: None,
    state: RowState::Unchecked,
    detail: String::new(),
    checked_at: None,
    approved_at: None,
    record: LoginState::default(),
    live: None,
  };
  if let Some(status) = host_auth::opt_str(entry, "status")? {
    row.state = classify_row_attention(Finding::Probe(status))?;
    row.detail = host_auth::opt_str(entry, "detail")?.unwrap_or("").to_string();
    row.checked_at = host_auth::opt_str(entry, "last_probe_at")?.map(str::to_string);
  }
  Ok(row)
}

/// The rows of the table in table order: the states that need attention first, config order within a state.
pub fn rows(state: &State, logins: &[BrowserLoginConfig], now: OffsetDateTime) -> anyhow::Result<Vec<Value>> {
  let hosts = host_auth::host_entries(state)?;
  let mut table: Vec<Row> = Vec::new();
  for login in logins {
    let live = browser_login::live_attempt(&login.name);
    table.push(login_row(state, &hosts, login, live)?);
  }
  for entry in &hosts {
    if login_bound_to(logins, host_auth::alias_of(entry)?).is_none() {
      table.push(host_row(entry)?);
    }
  }
  table.sort_by_key(|row| row.state.rank());
  table.iter().map(|row| row_json(row, now)).collect()
}

/// The row of `id` in the table, or `None` when the table has no such row.
pub fn row(
  state: &State,
  logins: &[BrowserLoginConfig],
  id: &str,
  now: OffsetDateTime,
) -> anyhow::Result<Option<Value>> {
  Ok(rows(state, logins, now)?.into_iter().find(|row| row["id"] == json!(id)))
}

/// The ids of the rows that need the user and the rows that are broken. The two lists share no row.
pub fn summary(rows: &[Value]) -> Value {
  let ids = |wanted: &str| {
    rows.iter().filter(|row| row["state"] == json!(wanted)).map(|row| row["id"].clone()).collect::<Vec<Value>>()
  };
  json!({ "needs_user": ids(RowState::NeedsUser.id()), "broken": ids(RowState::Broken.id()) })
}

/// `check_row`: run the row's check, persist what it found, and report whether the table has the row. A host row, and a
/// host-bound login, check with the single-host probe that `probe` runs. A login with no host runs its readback. With
/// `link_probe`, a login row also records its link probe, and a host row, which has no login, refuses the request.
pub async fn check_row<P, F>(
  state_path: &Path,
  config_path: &Path,
  logins: &[BrowserLoginConfig],
  id: &str,
  link_probe: bool,
  probe: P,
) -> anyhow::Result<bool>
where
  P: Fn(String) -> F,
  F: Future<Output = io::Result<(i32, String)>>,
{
  let Some(target) = parse_id(id) else {
    return Ok(false);
  };
  if link_probe && !matches!(target, RowId::Login(_)) {
    bail!("row {id} has no login whose link to probe");
  }
  let _checking = CheckGuard::start(id);
  match target {
    RowId::Login(name) => {
      let Some(login) = logins.iter().find(|login| login.name == name) else {
        return Ok(false);
      };
      if link_probe && browser_login::live_attempt(name).is_some() {
        bail!("login {name} has an attempt in progress, so its link probe waits for that attempt");
      }
      match &login.host {
        Some(alias) => {
          host_auth::probe_alias_with(state_path, config_path, alias, std::slice::from_ref(login), probe).await?;
        }
        None => {
          let (finding, detail) = match browser_login::check_readback(&login.readback_command).await {
            Readback::Exited { code, output } => (Finding::Check { exit: Some(code) }, output),
            Readback::TimedOut { after_sec } => (Finding::Check { exit: None }, timeout_text(after_sec)),
            Readback::CannotRun { reason } => (Finding::CannotRun, reason),
          };
          let state = classify_row_attention(finding)?;
          let detail = browser_login::shown_output(&detail);
          login_state::record_check(state_path, name, state, detail, timestamps::utc_now()).await?;
        }
      }
      if link_probe {
        let matched = browser_login::probe_link(login).await?;
        login_state::record_link_match(state_path, name, matched).await?;
      }
    }
    RowId::Host(alias) => {
      host_auth::probe_alias_with(state_path, config_path, alias, &[], probe).await?;
    }
  }
  Ok(true)
}

/// The row's JSON. The page renders its first paint from these fields, and the row route answers them on each refresh.
/// Every text field is ready to show; a field the row lacks is an empty string, a null, or `false`.
fn row_json(row: &Row, now: OffsetDateTime) -> anyhow::Result<Value> {
  let lifetime = row.login.and_then(|login| login.lifetime_sec);
  let (expiry, expiry_soon) = expiry_text(lifetime, row.approved_at.as_deref(), row.state, now)?;
  let checking = CHECKING.lock().expect("the checking set is never poisoned").contains(&row.id);
  let busy = match (&row.live, checking) {
    (Some(_), _) => "login",
    (None, true) => "check",
    (None, false) => "",
  };
  let checked_text = match &row.checked_at {
    Some(checked) => format!("Checked {}", local_text(checked)?),
    None => String::new(),
  };
  let attempt = row.record.last_attempt.as_ref();
  let last_text = match attempt {
    Some(record) => match &record.finished_at {
      Some(finished) => format!("Last attempt: {}, finished {}.", record.outcome.label(), local_text(finished)?),
      None => format!("Last attempt: {}.", record.outcome.label()),
    },
    None => String::new(),
  };
  let last_output = match attempt {
    Some(record) if !record.readback_output.is_empty() => record.readback_output.clone(),
    Some(record) => record.last_line.clone(),
    None => String::new(),
  };
  let live = row.live.as_ref();
  let live_text = live.map_or_else(String::new, |attempt| {
    format!("In progress: {}. User code: {}.", attempt.phase.label(), code_or_none(attempt.user_code()))
  });
  let busy_text = match busy {
    "login" => "A login is in progress.",
    "check" => "Checking.",
    _ => "",
  };
  let kind = if row.login.is_some() { "login" } else { "host" };
  let login = row.login;
  Ok(json!({
    "id": row.id,
    "kind": kind,
    "name": row.name,
    "alias": row.alias,
    "hostname": row.hostname,
    "label": login.map_or_else(|| row.name.clone(), |login| login.label.clone()),
    "state": row.state.id(),
    "state_label": row.state.label(),
    "detail": row.detail,
    "checked_text": checked_text,
    "expiry_text": expiry,
    "expiry_soon": expiry_soon,
    "next": next_step(row),
    "live": live.is_some(),
    "live_text": live_text,
    "live_link": live.and_then(|attempt| attempt.approval_url.clone()).unwrap_or_default(),
    "input_open": live.is_some_and(|attempt| attempt.input_open),
    "input_prompt": login.and_then(|login| login.input_prompt.clone()).unwrap_or_default(),
    "last": attempt.is_some(),
    "last_text": last_text,
    "last_output": last_output,
    "busy": busy,
    "busy_text": busy_text,
    "check_url": format!("/api/host-auth/rows/{}/check", row.id),
    "row_url": format!("/api/host-auth/rows/{}", row.id),
    "start_url": login.map(|login| format!("/api/host-auth/logins/{}/start", login.name)),
    "input_url": login
      .filter(|login| login.input_prompt.is_some())
      .map(|login| format!("/api/host-auth/logins/{}/input", login.name)),
  }))
}

fn code_or_none(code: &str) -> &str {
  if code.is_empty() { "not in the link yet" } else { code }
}

/// The next step a row's state asks of the user. A host row has no login to start, so it names the terminal command.
fn next_step(row: &Row) -> String {
  match (row.state, row.login) {
    (RowState::Ok, _) => String::new(),
    (RowState::NeedsUser, Some(_)) => "Start login, approve it in the new tab, and this row follows.".to_string(),
    (RowState::NeedsUser, None) => {
      format!("No browser login renews this host. Run ssh {} true in a terminal and approve its prompt.", row.name)
    }
    (RowState::Broken, Some(_)) => {
      "Start login again. If it fails the same way, fix this entry in host_auth.browser_logins.".to_string()
    }
    (RowState::Broken, None) => unreachable!("a host row takes its state from its probe, which never reads broken"),
    (RowState::Unreachable, _) => "The host gave no usable answer. Check the network, then check this row.".to_string(),
    (RowState::Unchecked, _) => "Not checked yet.".to_string(),
  }
}

/// The expiry line of a row, and whether the time left is under a day. A login with a lifetime counts from its last
/// approval. Past one lifetime, a row that still answers reads available, not expired.
fn expiry_text(
  lifetime_sec: Option<u64>,
  approved_at: Option<&str>,
  state: RowState,
  now: OffsetDateTime,
) -> anyhow::Result<(String, bool)> {
  let Some(approved) = approved_at else {
    return Ok((String::new(), false));
  };
  let approved = timestamps::parse(approved)?;
  let Some(lifetime) = lifetime_sec else {
    return Ok((format!("Last approved {}", local_text(&isoformat(approved))?), false));
  };
  let expires = approved + Duration::seconds(i64::try_from(lifetime)?);
  let left = (expires - now).whole_seconds();
  if left > 0 {
    let text = format!("Expires {} · {} left", local_text(&isoformat(expires))?, format_remaining(left));
    return Ok((text, left < 24 * 3600));
  }
  let ended = local_text(&isoformat(expires))?;
  let text = if state == RowState::Ok {
    format!("Available past one lifetime (it ended {ended})")
  } else {
    format!("One lifetime ended {ended}")
  };
  Ok((text, false))
}

/// The stored instant in the server's zone, as the page shows it.
fn local_text(stamp: &str) -> anyhow::Result<String> {
  Ok(timestamps::local_timestamp(Some(stamp), LocalFormat::DateZone)?.unwrap_or_default())
}

/// The countdown in its two most significant units.
pub fn format_remaining(seconds: i64) -> String {
  let (days, rest) = (seconds / 86400, seconds % 86400);
  let (hours, rest) = (rest / 3600, rest % 3600);
  let minutes = rest / 60;
  if days != 0 {
    format!("{days} d {hours} h")
  } else if hours != 0 {
    format!("{hours} h {minutes} m")
  } else {
    format!("{minutes} m")
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::probe::STATUS_OK;

  #[test]
  fn the_countdown_shows_its_two_largest_units() {
    assert_eq!(format_remaining(3 * 86400 + 21 * 3600 + 59), "3 d 21 h");
    assert_eq!(format_remaining(12 * 3600 + 36 * 60 + 5), "12 h 36 m");
    assert_eq!(format_remaining(9 * 60 + 30), "9 m");
  }

  #[test]
  fn a_row_id_names_a_login_or_a_host_and_nothing_else() {
    assert!(matches!(parse_id("login:router-host1"), Some(RowId::Login("router-host1"))));
    assert!(matches!(parse_id("host:host0"), Some(RowId::Host("host0"))));
    assert!(parse_id("hosts:host0").is_none());
    assert!(parse_id("host0").is_none());
  }

  #[test]
  fn the_status_route_summary_lists_each_row_once() {
    let rows = vec![
      json!({"id": "login:a", "state": "needs_user"}),
      json!({"id": "login:b", "state": "broken"}),
      json!({"id": "host:c", "state": "unreachable"}),
      json!({"id": "host:d", "state": "ok"}),
    ];
    assert_eq!(summary(&rows), json!({"needs_user": ["login:a"], "broken": ["login:b"]}));
  }

  #[test]
  fn expiry_counts_from_the_approval_when_a_lifetime_is_set() {
    let now = timestamps::parse("2026-11-18T00:30:00+00:00").expect("the stamp parses");
    let approved = Some("2026-11-18T00:16:29+00:00");
    let (text, soon) = expiry_text(Some(86400), approved, RowState::Ok, now).expect("the expiry builds");
    assert!(text.starts_with("Expires "), "{text}");
    assert!(text.contains("left"), "{text}");
    assert!(soon, "less than a day is marked soon");
    let (text, _) = expiry_text(Some(86400), approved, RowState::NeedsUser, now).expect("the expiry builds");
    assert!(text.starts_with("Expires "), "a counting row is counting whatever its state: {text}");
  }

  #[test]
  fn a_row_without_a_lifetime_shows_only_its_last_approval() {
    let now = timestamps::parse("2026-11-18T00:30:00+00:00").expect("the stamp parses");
    let (text, soon) =
      expiry_text(None, Some("2026-11-18T00:16:29+00:00"), RowState::Ok, now).expect("the expiry builds");
    assert!(text.starts_with("Last approved "), "{text}");
    assert!(!soon);
    let (text, _) = expiry_text(None, None, RowState::Ok, now).expect("the expiry builds");
    assert_eq!(text, "", "a row never approved shows no expiry");
  }

  #[test]
  fn a_lifetime_that_passed_reads_available_while_the_row_still_answers() {
    let now = timestamps::parse("2026-11-20T00:00:00+00:00").expect("the stamp parses");
    let approved = Some("2026-11-18T00:16:29+00:00");
    let (text, soon) = expiry_text(Some(86400), approved, RowState::Ok, now).expect("the expiry builds");
    assert!(text.starts_with("Available past one lifetime"), "{text}");
    assert!(!soon);
    let (text, _) = expiry_text(Some(86400), approved, RowState::NeedsUser, now).expect("the expiry builds");
    assert!(text.starts_with("One lifetime ended"), "a row that no longer answers is not available: {text}");
  }

  #[test]
  fn a_status_that_is_not_a_probe_verdict_is_refused() {
    assert!(classify_row_attention(Finding::Probe(STATUS_OK)).is_ok());
    assert!(classify_row_attention(Finding::Probe("no-such-verdict")).is_err());
  }
}
