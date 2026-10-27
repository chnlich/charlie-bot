//! Claude account membership, quota readings, selection, and transcript custody.
//!
//! Selection observes one tier rule: accounts whose headroom strictly exceeds `1 - WARNING_UTILIZATION` form the
//! under-line tier, and when that tier holds any account only it contends. An account at or above the warning line
//! is a last resort shared with no rule of its own.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, anyhow};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::usage;
use charliebot_store::metadata::UtcDateTime;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use walkdir::WalkDir;

use super::{claude_config, login_dirs};

pub const POOL_DOMAIN: &str = "pool";
pub const WARNING_UTILIZATION: f64 = 0.90;
pub const AUTH_FAILURE_COOLDOWN: Duration = Duration::minutes(15);
pub const GUARD_REFUSAL_MARKER: &str = "refusing to overwrite newer transcript";
pub const PROBE_TAIL_BYTES: u64 = 16 * 1024;

const REJECTED_WITHOUT_RESET: Duration = Duration::hours(1);
const RESET_BONUS_SCALE: f64 = 0.10;
const RESET_BONUS_HORIZON: f64 = 24.0 * 3600.0;
const RESET_BONUS_MIN_HEADROOM: f64 = 0.10;
const SCORE_TIE: f64 = 0.02;
const STAGING_SUFFIX: &str = ".staging";

#[derive(Clone, Debug, PartialEq)]
pub struct RateLimitReading {
  pub at: UtcDateTime,
  pub utilization: f64,
  pub rejected_until: Option<UtcDateTime>,
}

#[derive(Clone, Debug)]
struct PanelReading {
  at: UtcDateTime,
  windows: Vec<Value>,
}

#[derive(Default)]
struct Readings {
  event: HashMap<String, RateLimitReading>,
  panel: HashMap<String, PanelReading>,
  auth_failures: HashMap<String, UtcDateTime>,
  login_notified: HashSet<String>,
}

/// The process-wide state of the quota accounts, kept on the shared [`ClaudeAccountState`]: the User-Agent the probe
/// resolved, and the warnings already logged.
#[derive(Default)]
pub struct QuotaState {
  pub(super) user_agent: tokio::sync::OnceCell<String>,
  pub(super) warned: std::sync::Mutex<Warned>,
}

#[derive(Default)]
pub(super) struct Warned {
  /// `(event, credentials path)` pairs. A read of a path that returns a token forgets its pairs.
  pub(super) credentials: HashSet<(&'static str, PathBuf)>,
  /// `(account, slot, reason)` triples of unrecognized limit shapes, logged once per process.
  pub(super) limit_shapes: HashSet<(String, String, String)>,
}

/// One server's mutable observations for every Claude account. The quota accounts of the usage panel keep their
/// process-wide state in `quota`.
#[derive(Default)]
pub struct ClaudeAccountState {
  readings: std::sync::Mutex<Readings>,
  pub(super) pool_choice: tokio::sync::Mutex<()>,
  pub quota: QuotaState,
}

impl ClaudeAccountState {
  pub fn new() -> Self {
    Self::default()
  }
}

pub fn now_or(now: Option<UtcDateTime>) -> UtcDateTime {
  now.unwrap_or_else(UtcDateTime::now)
}

pub fn pool(cfg: &CharlieBotConfig) -> anyhow::Result<Vec<claude_config::ClaudeAccount>> {
  let accounts = cfg.section::<claude_config::AccountsConfig>("accounts");
  accounts
    .claude
    .iter()
    .map(|account| {
      Ok(claude_config::ClaudeAccount {
        label: account.label.clone(),
        config_dir: expand_account_path(&account.config_dir)?.to_string_lossy().into_owned(),
      })
    })
    .collect()
}

fn expand_account_path(value: &str) -> anyhow::Result<PathBuf> {
  let home = std::env::var("HOME").ok();
  login_dirs::expanduser(value, home.as_deref())
}

pub fn is_pooled(option: &BackendOption, cfg: &CharlieBotConfig) -> bool {
  option.backend_type == "cc-claude" && !cfg.section::<claude_config::AccountsConfig>("accounts").claude.is_empty()
}

pub fn option_pool(option: &BackendOption) -> Option<String> {
  option
    .type_fields
    .get(serde_yaml::Value::String("account_pool".to_string()))
    .and_then(serde_yaml::Value::as_str)
    .map(str::to_string)
}

pub fn pool_accounts(
  cfg: &CharlieBotConfig,
  account_pool: Option<&str>,
) -> anyhow::Result<Vec<claude_config::ClaudeAccount>> {
  let accounts = pool(cfg)?;
  let Some(name) = account_pool else {
    return Ok(accounts);
  };
  let members = cfg
    .section::<claude_config::AccountsConfig>("accounts")
    .claude_pools
    .get(name)
    .ok_or_else(|| anyhow!("account pool {name:?} is not defined"))?
    .iter()
    .cloned()
    .collect::<HashSet<_>>();
  Ok(accounts.into_iter().filter(|account| members.contains(&account.label)).collect())
}

pub fn in_pool(cfg: &CharlieBotConfig, label: &str, account_pool: Option<&str>) -> anyhow::Result<bool> {
  let Some(name) = account_pool else {
    return Ok(true);
  };
  let pools = &cfg.section::<claude_config::AccountsConfig>("accounts").claude_pools;
  let members = pools.get(name).ok_or_else(|| anyhow!("account pool {name:?} is not defined"))?;
  Ok(members.iter().any(|member| member == label))
}

pub fn account_by_label(
  cfg: &CharlieBotConfig,
  label: Option<&str>,
) -> anyhow::Result<Option<claude_config::ClaudeAccount>> {
  let Some(label) = label.filter(|label| !label.is_empty()) else {
    return Ok(None);
  };
  Ok(pool(cfg)?.into_iter().find(|account| account.label == label))
}

pub fn continuation_domain(option: &BackendOption, cfg: &CharlieBotConfig) -> anyhow::Result<String> {
  if option.backend_type != "cc-claude" {
    return Ok(option.id.clone());
  }
  if is_pooled(option, cfg) {
    return Ok(POOL_DOMAIN.to_string());
  }
  Ok(login_dirs::claude_config_dir()?.display().to_string())
}

pub fn credentials_present(account: &claude_config::ClaudeAccount) -> bool {
  let path = Path::new(&account.config_dir).join(login_dirs::CREDENTIALS_FILE);
  let Ok(contents) = fs::read_to_string(path) else {
    return false;
  };
  let Ok(data) = serde_json::from_str::<Value>(&contents) else {
    return false;
  };
  data.get("claudeAiOauth").and_then(|oauth| oauth.get("accessToken")).is_some_and(json_truthy)
}

/// Python truthiness of a JSON value.
pub fn json_truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(value) => *value,
    Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
    Value::String(value) => !value.is_empty(),
    Value::Array(value) => !value.is_empty(),
    Value::Object(value) => !value.is_empty(),
  }
}

pub fn record_auth_failure(state: &ClaudeAccountState, label: &str, now: Option<UtcDateTime>) {
  state
    .readings
    .lock()
    .expect("Claude account state is not poisoned")
    .auth_failures
    .insert(label.to_string(), now_or(now));
}

pub fn auth_failed_recently(state: &ClaudeAccountState, label: &str, now: Option<UtcDateTime>) -> bool {
  state
    .readings
    .lock()
    .expect("Claude account state is not poisoned")
    .auth_failures
    .get(label)
    .is_some_and(|failed| now_or(now).into_inner() - failed.into_inner() < AUTH_FAILURE_COOLDOWN)
}

pub fn login_notice_due(state: &ClaudeAccountState, label: &str, unhealthy: bool) -> bool {
  let mut readings = state.readings.lock().expect("Claude account state is not poisoned");
  if !unhealthy {
    readings.login_notified.remove(label);
    return false;
  }
  readings.login_notified.insert(label.to_string())
}

pub fn healthy(state: &ClaudeAccountState, account: &claude_config::ClaudeAccount, now: Option<UtcDateTime>) -> bool {
  credentials_present(account) && !auth_failed_recently(state, &account.label, now)
}

pub fn observe_rate_limit(
  state: &ClaudeAccountState,
  label: &str,
  info: &Value,
  now: Option<UtcDateTime>,
) -> Option<RateLimitReading> {
  let moment = now_or(now);
  let windows = info.get("unifiedWindows").and_then(Value::as_object);
  let mut values = Vec::new();
  if let Some(windows) = windows {
    for name in ["five_hour", "seven_day"] {
      if let Some(value) = windows.get(name).and_then(|window| window.get("utilization")).and_then(Value::as_f64) {
        values.push(value);
      }
    }
  }
  let rejected = info.get("status").and_then(Value::as_str) == Some("rejected");
  if values.is_empty() && !rejected {
    return None;
  }
  let rejected_until = if rejected {
    Some(
      info
        .get("resetsAt")
        .and_then(Value::as_f64)
        .map_or_else(|| add_duration(moment, REJECTED_WITHOUT_RESET), unix_seconds),
    )
  } else {
    None
  };
  let reading =
    RateLimitReading { at: moment, utilization: values.into_iter().reduce(f64::max).unwrap_or(1.0), rejected_until };
  state.readings.lock().expect("Claude account state is not poisoned").event.insert(label.to_string(), reading.clone());
  Some(reading)
}

pub fn observe_usage_panel(state: &ClaudeAccountState, label: &str, usage_data: &Value) {
  let Some(windows) = usage_data.get(usage::PANEL_WINDOWS).and_then(Value::as_array) else {
    return;
  };
  let at = usage_data
    .get(usage::PANEL_FETCHED_AT)
    .and_then(Value::as_str)
    .and_then(|value| UtcDateTime::parse(value).ok())
    .unwrap_or_else(UtcDateTime::now);
  state.readings.lock().expect("Claude account state is not poisoned").panel.insert(
    label.to_string(),
    PanelReading { at, windows: windows.iter().filter(|window| window.is_object()).cloned().collect() },
  );
}

fn model_family(model: Option<&str>) -> String {
  let Some(model) = model.filter(|model| !model.is_empty()) else {
    return String::new();
  };
  let parts = model.to_lowercase().split('-').map(str::to_string).collect::<Vec<_>>();
  if parts.first().is_some_and(|part| part == "claude") && parts.len() > 1 {
    parts[1].clone()
  } else {
    parts[0].clone()
  }
}

/// Whether one stored usage-panel window is expired at `now`, judged against the time its reading was `sampled`
/// (`usage_sources.panel_window_expired`).
pub fn panel_window_expired(window: &Value, sampled: UtcDateTime, now: UtcDateTime) -> bool {
  let resets_at =
    window.get(usage::PANEL_RESETS_AT).and_then(Value::as_str).and_then(|value| UtcDateTime::parse(value).ok());
  if resets_at.is_some_and(|reset| reset <= now && sampled < reset) {
    return true;
  }
  window
    .get(usage::PANEL_WINDOW_MINUTES)
    .and_then(Value::as_i64)
    .is_some_and(|minutes| now.into_inner() - sampled.into_inner() > Duration::minutes(minutes))
}

fn panel_scope_matches(window: &Value, family: &str) -> bool {
  let scope = window.get(usage::PANEL_SCOPE_LABEL);
  if !scope.is_some_and(json_truthy) {
    return true;
  }
  let scope = scope.and_then(Value::as_str).map_or_else(|| scope.unwrap().to_string(), str::to_string).to_lowercase();
  !family.is_empty() && scope.contains(family)
}

fn live_windows<'a>(reading: &'a PanelReading, model: Option<&str>, now: UtcDateTime) -> Vec<&'a Value> {
  let family = model_family(model);
  reading
    .windows
    .iter()
    .filter(|window| !panel_window_expired(window, reading.at, now) && panel_scope_matches(window, &family))
    .collect()
}

fn panel_reading(reading: &PanelReading, model: Option<&str>, now: UtcDateTime) -> Option<RateLimitReading> {
  let utilization = live_windows(reading, model, now)
    .iter()
    .filter_map(|window| window.get(usage::PANEL_UTILIZATION).and_then(Value::as_f64))
    .map(|value| value / 100.0)
    .reduce(f64::max)?;
  Some(RateLimitReading { at: reading.at, utilization, rejected_until: None })
}

fn newest_reading(left: Option<RateLimitReading>, right: Option<RateLimitReading>) -> Option<RateLimitReading> {
  match (left, right) {
    (None, value) | (value, None) => value,
    (Some(left), Some(right)) => Some(if left.at >= right.at { left } else { right }),
  }
}

pub fn latest_reading(
  state: &ClaudeAccountState,
  label: &str,
  model: Option<&str>,
  now: Option<UtcDateTime>,
) -> Option<RateLimitReading> {
  let moment = now_or(now);
  let readings = state.readings.lock().expect("Claude account state is not poisoned");
  let panel = readings.panel.get(label);
  let general =
    newest_reading(readings.event.get(label).cloned(), panel.and_then(|value| panel_reading(value, None, moment)));
  let Some(model) = model else {
    return general;
  };
  let Some(panel) = panel else {
    return general;
  };
  if !live_windows(panel, Some(model), moment)
    .iter()
    .any(|window| window.get(usage::PANEL_SCOPE_LABEL).is_some_and(json_truthy))
  {
    return general;
  }
  let Some(scoped) = panel_reading(panel, Some(model), moment) else {
    return general;
  };
  let rejection = general.as_ref().and_then(|reading| reading.rejected_until).filter(|reset| *reset > moment);
  Some(RateLimitReading {
    at: general.as_ref().map_or(scoped.at, |reading| reading.at.max(scoped.at)),
    utilization: general.map_or(scoped.utilization, |reading| reading.utilization.max(scoped.utilization)),
    rejected_until: rejection,
  })
}

pub fn headroom(state: &ClaudeAccountState, label: &str, model: Option<&str>, now: Option<UtcDateTime>) -> f64 {
  let moment = now_or(now);
  let Some(reading) = latest_reading(state, label, model, Some(moment)) else {
    return 1.0;
  };
  if reading.rejected_until.is_some_and(|reset| reset > moment) {
    return 0.0;
  }
  (1.0 - reading.utilization).max(0.0)
}

#[allow(clippy::too_many_arguments)]
pub fn select(
  state: &ClaudeAccountState,
  cfg: &CharlieBotConfig,
  model: Option<&str>,
  exclude: &HashSet<String>,
  busy_accounts: Option<&BTreeSet<String>>,
  now: Option<UtcDateTime>,
  account_pool: Option<&str>,
  min_headroom: f64,
) -> anyhow::Result<Option<claude_config::ClaudeAccount>> {
  let moment = now_or(now);
  let accounts = pool_accounts(cfg, account_pool)?;
  let available = accounts
    .into_iter()
    .filter(|account| !exclude.contains(&account.label) && healthy(state, account, Some(moment)))
    .filter_map(|account| {
      let remaining = headroom(state, &account.label, model, Some(moment));
      (remaining > min_headroom).then_some((account, remaining))
    })
    .collect::<Vec<_>>();
  if available.is_empty() {
    return Ok(None);
  }
  let under_line =
    available.iter().filter(|(_, remaining)| *remaining > 1.0 - WARNING_UTILIZATION).cloned().collect::<Vec<_>>();
  let tier = if under_line.is_empty() { available } else { under_line };
  let idle = tier
    .iter()
    .filter(|(account, _)| busy_accounts.is_none_or(|busy| !busy.contains(&account.label)))
    .cloned()
    .collect::<Vec<_>>();
  let contenders = if idle.is_empty() { tier } else { idle };
  let scores = contenders
    .into_iter()
    .map(|(account, remaining)| {
      let score = remaining + reset_bonus(state, &account.label, model, moment, remaining);
      (account, remaining, score)
    })
    .collect::<Vec<_>>();
  let best = scores.iter().map(|(_, _, score)| *score).reduce(f64::max).expect("contenders is non-empty");
  let readings = state.readings.lock().expect("Claude account state is not poisoned");
  Ok(
    scores
      .into_iter()
      .filter(|(_, _, score)| best - score <= SCORE_TIE)
      .min_by_key(|(account, _, _)| {
        readings.event.get(&account.label).map_or(i128::MIN, |reading| reading.at.into_inner().unix_timestamp_nanos())
      })
      .map(|(account, _, _)| account),
  )
}

pub fn cool_destination(
  state: &ClaudeAccountState,
  cfg: &CharlieBotConfig,
  model: Option<&str>,
  account_pool: Option<&str>,
  now: Option<UtcDateTime>,
) -> anyhow::Result<Option<claude_config::ClaudeAccount>> {
  select(state, cfg, model, &HashSet::new(), None, now, account_pool, 1.0 - WARNING_UTILIZATION)
}

fn reset_bonus(
  state: &ClaudeAccountState,
  label: &str,
  model: Option<&str>,
  moment: UtcDateTime,
  remaining: f64,
) -> f64 {
  if remaining <= RESET_BONUS_MIN_HEADROOM {
    return 0.0;
  }
  let Some(reset) = time_to_reset(state, label, model, moment) else {
    return 0.0;
  };
  let seconds = (reset.into_inner() - moment.into_inner()).as_seconds_f64();
  if !(0.0..RESET_BONUS_HORIZON).contains(&seconds) {
    return 0.0;
  }
  RESET_BONUS_SCALE * (1.0 - seconds / RESET_BONUS_HORIZON)
}

fn time_to_reset(
  state: &ClaudeAccountState,
  label: &str,
  model: Option<&str>,
  now: UtcDateTime,
) -> Option<UtcDateTime> {
  let readings = state.readings.lock().expect("Claude account state is not poisoned");
  let panel = readings.panel.get(label)?;
  let limiting = live_windows(panel, model, now)
    .into_iter()
    .filter_map(|window| window.get(usage::PANEL_UTILIZATION).and_then(Value::as_f64).map(|value| (value, window)))
    .max_by(|left, right| match left.0.total_cmp(&right.0) {
      std::cmp::Ordering::Equal => std::cmp::Ordering::Greater,
      ordering => ordering,
    })?;
  limiting.1.get(usage::PANEL_RESETS_AT).and_then(Value::as_str).and_then(|value| UtcDateTime::parse(value).ok())
}

pub fn earliest_reset(
  state: &ClaudeAccountState,
  cfg: &CharlieBotConfig,
  now: Option<UtcDateTime>,
  account_pool: Option<&str>,
) -> anyhow::Result<Option<UtcDateTime>> {
  let moment = now_or(now);
  let labels = pool_accounts(cfg, account_pool)?.into_iter().map(|account| account.label).collect::<HashSet<_>>();
  Ok(
    state
      .readings
      .lock()
      .expect("Claude account state is not poisoned")
      .event
      .iter()
      .filter(|(label, _)| labels.contains(*label))
      .filter_map(|(_, reading)| reading.rejected_until.filter(|reset| *reset > moment))
      .min(),
  )
}

pub fn unavailable_detail(
  state: &ClaudeAccountState,
  cfg: &CharlieBotConfig,
  now: Option<UtcDateTime>,
  account_pool: Option<&str>,
) -> anyhow::Result<String> {
  let moment = now_or(now);
  let mut entries = Vec::new();
  for account in pool_accounts(cfg, account_pool)? {
    if let Some(reason) = unavailable_reason(state, &account, moment) {
      entries.push(format!("{}: {reason}", account.label));
    }
  }
  Ok(entries.join("; "))
}

fn unavailable_reason(
  state: &ClaudeAccountState,
  account: &claude_config::ClaudeAccount,
  now: UtcDateTime,
) -> Option<String> {
  if !credentials_present(account) {
    return Some("missing credentials".to_string());
  }
  let failed =
    state.readings.lock().expect("Claude account state is not poisoned").auth_failures.get(&account.label).copied();
  if let Some(failed) = failed
    && now.into_inner() - failed.into_inner() < AUTH_FAILURE_COOLDOWN
  {
    let until = add_duration(failed, AUTH_FAILURE_COOLDOWN).into_inner();
    return Some(format!("auth-failure cooldown until {:02}:{:02} UTC", until.hour(), until.minute()));
  }
  if headroom(state, &account.label, None, Some(now)) <= 0.0 {
    return Some("zero headroom".to_string());
  }
  None
}

fn add_duration(value: UtcDateTime, duration: Duration) -> UtcDateTime {
  to_utc_datetime(value.into_inner() + duration)
}

fn unix_seconds(value: f64) -> UtcDateTime {
  let seconds = value.floor() as i64;
  let nanos = ((value - seconds as f64) * 1_000_000_000.0) as i64;
  let datetime = OffsetDateTime::from_unix_timestamp(seconds).expect("rate-limit reset timestamp is in range")
    + Duration::nanoseconds(nanos);
  to_utc_datetime(datetime)
}

fn to_utc_datetime(value: OffsetDateTime) -> UtcDateTime {
  let text = value.format(&Rfc3339).expect("UTC timestamps format as RFC3339");
  UtcDateTime::parse(&text).expect("RFC3339 timestamps parse")
}

/// The transcripts a config directory holds for `session_id`: `projects/<slug>/<session_id>.jsonl`. The config root is
/// a literal path, and the session id joins the pattern as Python's `Path.glob` interpolates it.
pub fn transcript_matches(config_dir: &Path, session_id: &str) -> anyhow::Result<Vec<PathBuf>> {
  let root = expand_account_path(&config_dir.to_string_lossy())?.join("projects");
  let pattern = format!("{}/*/{session_id}.jsonl", glob::Pattern::escape(&root.to_string_lossy()));
  let mut matches = Vec::new();
  for entry in glob::glob(&pattern).with_context(|| format!("reading transcript projects {}", root.display()))? {
    let path = entry.with_context(|| format!("reading transcript projects {}", root.display()))?;
    if path.is_file() {
      matches.push(path);
    }
  }
  matches.sort();
  Ok(matches)
}

pub fn transcript_path(config_dir: &Path, session_id: &str) -> anyhow::Result<Option<PathBuf>> {
  Ok(transcript_matches(config_dir, session_id)?.into_iter().next())
}

pub fn find_transcript_account(
  cfg: &CharlieBotConfig,
  session_id: &str,
) -> anyhow::Result<Option<claude_config::ClaudeAccount>> {
  for account in pool(cfg)? {
    if transcript_path(Path::new(&account.config_dir), session_id)?.is_some() {
      return Ok(Some(account));
    }
  }
  Ok(None)
}

pub fn is_newer_transcript_refusal(error: &anyhow::Error) -> bool {
  error.to_string().contains(GUARD_REFUSAL_MARKER)
}

fn file_mtime_ns(path: &Path) -> anyhow::Result<i128> {
  let modified = fs::metadata(path)?.modified()?;
  let duration = modified.duration_since(UNIX_EPOCH).map_err(|error| anyhow!("invalid file mtime: {error}"))?;
  Ok(duration.as_nanos() as i128)
}

fn file_size(path: &Path) -> anyhow::Result<u64> {
  Ok(fs::metadata(path)?.len())
}

fn dst_holds_newer_transcript(src: &Path, dst: &Path) -> anyhow::Result<bool> {
  if !dst.exists() {
    return Ok(false);
  }
  let src_time = file_mtime_ns(src)?;
  let dst_time = file_mtime_ns(dst)?;
  Ok(dst_time > src_time || (dst_time == src_time && file_size(dst)? > file_size(src)?))
}

fn iso_mtime(path: &Path) -> anyhow::Result<String> {
  let modified = fs::metadata(path)?.modified()?;
  let duration = modified.duration_since(UNIX_EPOCH).map_err(|error| anyhow!("invalid file mtime: {error}"))?;
  let nanos = i64::try_from(duration.as_nanos()).context("file mtime is outside the supported timestamp range")?;
  Ok(to_utc_datetime(OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos))?).to_python_isoformat())
}

fn refusal_detail(src: &Path, dst: &Path) -> anyhow::Result<String> {
  Ok(format!(
    "dst {} (mtime {}, size {}) vs src {} (mtime {}, size {})",
    dst.display(),
    iso_mtime(dst)?,
    file_size(dst)?,
    src.display(),
    iso_mtime(src)?,
    file_size(src)?
  ))
}

fn is_staging_name(name: &std::ffi::OsStr) -> bool {
  name.to_string_lossy().ends_with(STAGING_SUFFIX)
}

fn walk_files(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
  let mut files = Vec::new();
  for entry in WalkDir::new(root).follow_links(false) {
    let entry = entry.with_context(|| format!("reading sidecar {}", root.display()))?;
    if entry.depth() == 0 || entry.path().is_dir() {
      continue;
    }
    if entry.path().is_file() {
      files.push(entry.into_path());
    }
  }
  files.sort();
  Ok(files)
}

fn sidecar_files(root: &Path) -> anyhow::Result<BTreeSet<PathBuf>> {
  if !root.is_dir() {
    return Ok(BTreeSet::new());
  }
  Ok(
    walk_files(root)?
      .into_iter()
      .filter(|path| !path.file_name().is_some_and(is_staging_name))
      .map(|path| path.strip_prefix(root).expect("walked path is under root").to_path_buf())
      .collect(),
  )
}

fn python_list(paths: impl IntoIterator<Item = PathBuf>) -> String {
  let values = paths.into_iter().map(|path| format!("'{}'", path.display())).collect::<Vec<_>>();
  format!("[{}]", values.join(", "))
}

fn sidecar_file_diff(src: &Path, dst: &Path) -> anyhow::Result<String> {
  let src_files = sidecar_files(src)?;
  let dst_files = sidecar_files(dst)?;
  let mut parts = Vec::new();
  let missing = src_files.difference(&dst_files).cloned().collect::<Vec<_>>();
  if !missing.is_empty() {
    parts.push(format!("missing at dst: {}", python_list(missing)));
  }
  let extra = dst_files.difference(&src_files).cloned().collect::<Vec<_>>();
  if !extra.is_empty() {
    parts.push(format!("only at dst: {}", python_list(extra)));
  }
  let mut differing = Vec::new();
  for relative in src_files.intersection(&dst_files) {
    if file_size(&src.join(relative))? != file_size(&dst.join(relative))? {
      differing.push(relative.clone());
    }
  }
  if !differing.is_empty() {
    parts.push(format!("size differs: {}", python_list(differing)));
  }
  Ok(if parts.is_empty() { "file sets equal".to_string() } else { parts.join("; ") })
}

fn copy2(src: &Path, dst: &Path) -> anyhow::Result<()> {
  fs::copy(src, dst).with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
  let modified = fs::metadata(src)?.modified()?;
  fs::File::open(dst)?.set_times(fs::FileTimes::new().set_modified(modified))?;
  Ok(())
}

fn move_sidecar(src: &Path, dst: &Path) -> anyhow::Result<Option<(usize, usize, u64)>> {
  if !src.is_dir() {
    return Ok(None);
  }
  let mut staged = Vec::new();
  let result = (|| -> anyhow::Result<()> {
    for src_file in walk_files(src)? {
      let relative = src_file.strip_prefix(src).expect("walked path is under source sidecar");
      let dst_file = dst.join(relative);
      fs::create_dir_all(dst_file.parent().expect("sidecar file has parent"))?;
      let temporary =
        dst_file.with_file_name(format!("{}{}", dst_file.file_name().unwrap().to_string_lossy(), STAGING_SUFFIX));
      copy2(&src_file, &temporary)?;
      staged.push(temporary.clone());
      if file_size(&temporary)? != file_size(&src_file)? {
        return Err(anyhow!("transcript sidecar copy size mismatch: {} -> {}", src_file.display(), dst_file.display()));
      }
      fs::rename(&temporary, &dst_file)?;
      staged.pop();
    }
    Ok(())
  })();
  for temporary in staged {
    if let Err(error) = fs::remove_file(&temporary)
      && error.kind() != std::io::ErrorKind::NotFound
    {
      tracing::warn!(target: "charliebot_agents", path = %temporary.display(), error = %error, "claude_account_transcript_staging_remove_failed");
    }
  }
  if let Err(error) = result {
    let detail = sidecar_file_diff(src, dst)?;
    return Err(anyhow!(
      "transcript sidecar file-set mismatch: {} -> {} ({}); last error: {}",
      src.display(),
      dst.display(),
      detail,
      error
    ));
  }
  let src_files = sidecar_files(src)?;
  let dst_files = sidecar_files(dst)?;
  let bytes =
    src_files.iter().try_fold(0_u64, |sum, relative| file_size(&dst.join(relative)).map(|size| sum + size))?;
  Ok(Some((src_files.len(), dst_files.difference(&src_files).count(), bytes)))
}

pub fn move_transcript(session_id: &str, src_dir: &Path, dst_dir: &Path) -> anyhow::Result<PathBuf> {
  let Some(src) = transcript_path(src_dir, session_id)? else {
    return Err(anyhow!(
      "no transcript {session_id}.jsonl under {}",
      expand_account_path(&src_dir.to_string_lossy())?.join("projects").display()
    ));
  };
  let dst = expand_account_path(&dst_dir.to_string_lossy())?
    .join("projects")
    .join(src.parent().expect("transcript has a project directory").file_name().expect("project directory has a name"))
    .join(src.file_name().expect("transcript has a filename"));
  fs::create_dir_all(dst.parent().expect("transcript has parent"))?;
  if dst_holds_newer_transcript(&src, &dst)? {
    tracing::warn!(
      target: "charliebot_agents",
      cc_session_id = session_id,
      src = %src.display(),
      dst = %dst.display(),
      src_mtime_ns = file_mtime_ns(&src)?,
      src_size = file_size(&src)?,
      dst_mtime_ns = file_mtime_ns(&dst)?,
      dst_size = file_size(&dst)?,
      "master_cc_transcript_move_stale_refused"
    );
    return Err(anyhow!("{GUARD_REFUSAL_MARKER}: {}", refusal_detail(&src, &dst)?));
  }
  let staged = dst.with_file_name(format!("{}{}", dst.file_name().unwrap().to_string_lossy(), STAGING_SUFFIX));
  let move_result = (|| -> anyhow::Result<()> {
    copy2(&src, &staged)?;
    if file_size(&staged)? != file_size(&src)? {
      return Err(anyhow!("transcript copy size mismatch: {} -> {}", src.display(), dst.display()));
    }
    fs::rename(&staged, &dst)?;
    Ok(())
  })();
  if let Err(error) = move_result {
    if let Err(remove_error) = fs::remove_file(&staged)
      && remove_error.kind() != std::io::ErrorKind::NotFound
    {
      tracing::warn!(target: "charliebot_agents", path = %staged.display(), error = %remove_error, "claude_account_transcript_staging_remove_failed");
    }
    return Err(error);
  }
  let sidecar_report = move_sidecar(&src.with_extension(""), &dst.with_extension(""))?;
  tracing::info!(
    target: "charliebot_agents",
    cc_session_id = session_id,
    src = %src.display(),
    dst = %dst.display(),
    sidecar_src_files = sidecar_report.map(|report| report.0),
    sidecar_dst_only_files = sidecar_report.map(|report| report.1),
    sidecar_bytes = sidecar_report.map(|report| report.2),
    "claude_account_transcript_moved"
  );
  Ok(dst)
}

pub fn newest_transcript_copy(
  cfg: &CharlieBotConfig,
  session_id: &str,
) -> anyhow::Result<Option<(claude_config::ClaudeAccount, PathBuf)>> {
  let mut best: Option<(i128, PathBuf, claude_config::ClaudeAccount)> = None;
  for account in pool(cfg)? {
    for path in transcript_matches(Path::new(&account.config_dir), session_id)? {
      let candidate = (file_mtime_ns(&path)?, path, account.clone());
      if best.as_ref().is_none_or(|current| (candidate.0, &candidate.1) > (current.0, &current.1)) {
        best = Some(candidate);
      }
    }
  }
  Ok(best.map(|(_, path, account)| (account, path)))
}

fn tail_window(path: &Path) -> anyhow::Result<String> {
  use std::io::{Read, Seek, SeekFrom};

  let mut file = fs::File::open(path).with_context(|| format!("opening transcript {}", path.display()))?;
  let end = file.seek(SeekFrom::End(0))?;
  file.seek(SeekFrom::Start(end.saturating_sub(PROBE_TAIL_BYTES)))?;
  let mut bytes = Vec::new();
  file.read_to_end(&mut bytes)?;
  Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn transcript_tail_line(path: &Path) -> anyhow::Result<Option<String>> {
  Ok(tail_window(path)?.lines().rfind(|line| !line.trim().is_empty()).map(str::to_string))
}

pub fn transcript_lineage_split(label_copy: &Path, newest_copy: &Path) -> anyhow::Result<bool> {
  let Some(tail) = transcript_tail_line(label_copy)? else {
    return Ok(false);
  };
  Ok(!tail_window(newest_copy)?.contains(&tail))
}

pub fn retire_transcript_copies(cfg: &CharlieBotConfig, session_id: &str, keep: usize) -> anyhow::Result<()> {
  let mut copies = Vec::new();
  for account in pool(cfg)? {
    for path in transcript_matches(Path::new(&account.config_dir), session_id)? {
      copies.push((file_mtime_ns(&path)?, path));
    }
  }
  copies.sort_by_key(|copy| std::cmp::Reverse(copy.0));
  for (_, path) in copies.into_iter().skip(keep) {
    let result = (|| -> anyhow::Result<()> {
      fs::remove_file(&path)?;
      let sidecar = path.with_extension("");
      if sidecar.is_dir() {
        fs::remove_dir_all(&sidecar)?;
      }
      Ok(())
    })();
    if let Err(error) = result {
      tracing::warn!(target: "charliebot_agents", path = %path.display(), error = %error, "claude_account_transcript_retire_failed");
      continue;
    }
    tracing::info!(target: "charliebot_agents", cc_session_id = session_id, path = %path.display(), "claude_account_transcript_retired");
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::time::Duration as StdDuration;

  use charliebot_registry::Registry;
  use serde_json::json;

  use super::*;

  fn config(home: &Path, dirs: &[(&str, &Path)]) -> CharlieBotConfig {
    let mut accounts = String::new();
    let mut labels = Vec::new();
    for (label, directory) in dirs {
      accounts.push_str(&format!("    - label: {label}\n      config_dir: '{}'\n", directory.display()));
      labels.push((*label).to_string());
    }
    let config = format!(
      "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n      account_pool: alpha\naccounts:\n  claude:\n{accounts}  claude_pools:\n    alpha: [{}]\n",
      labels.join(", ")
    );
    fs::write(home.join("config.yaml"), config).unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", crate::register).unwrap();
    registry.load_config(home).unwrap()
  }

  fn add_credentials(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::write(path.join(login_dirs::CREDENTIALS_FILE), r#"{"claudeAiOauth":{"accessToken":"synthetic"}}"#).unwrap();
  }

  fn timestamp(value: &str) -> UtcDateTime {
    UtcDateTime::parse(value).unwrap()
  }

  fn window(utilization: f64) -> Value {
    json!({"unifiedWindows":{"five_hour":{"utilization":utilization}}})
  }

  #[test]
  fn selection_uses_idle_accounts_and_falls_back_when_all_are_busy() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    add_credentials(&a);
    add_credentials(&b);
    let cfg = config(&home, &[("a", &a), ("b", &b)]);
    let state = ClaudeAccountState::new();
    let now = timestamp("2025-01-01T12:00:00Z");
    observe_rate_limit(&state, "a", &window(0.20), Some(now));
    observe_rate_limit(&state, "b", &window(0.60), Some(now));
    let busy = BTreeSet::from(["a".to_string()]);
    let chosen = select(&state, &cfg, Some("claude-fable-5-1"), &HashSet::new(), Some(&busy), Some(now), None, 0.0)
      .unwrap()
      .unwrap();
    assert_eq!(chosen.label, "b");

    let all_busy = BTreeSet::from(["a".to_string(), "b".to_string()]);
    let chosen = select(&state, &cfg, Some("claude-fable-5-1"), &HashSet::new(), Some(&all_busy), Some(now), None, 0.0)
      .unwrap()
      .unwrap();
    assert_eq!(chosen.label, "a");
  }

  #[test]
  fn busy_under_line_account_beats_an_idle_account_at_the_line() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    add_credentials(&a);
    add_credentials(&b);
    let cfg = config(&home, &[("a", &a), ("b", &b)]);
    let state = ClaudeAccountState::new();
    let now = timestamp("2025-01-01T12:00:00Z");
    observe_rate_limit(&state, "a", &window(0.89), Some(now));
    observe_rate_limit(&state, "b", &window(0.90), Some(now));
    let busy = BTreeSet::from(["a".to_string()]);
    let chosen = select(&state, &cfg, Some("claude-fable-5-1"), &HashSet::new(), Some(&busy), Some(now), None, 0.0)
      .unwrap()
      .unwrap();
    assert_eq!(chosen.label, "a");
  }

  #[test]
  fn selection_without_an_under_line_account_contends_every_account() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    add_credentials(&a);
    add_credentials(&b);
    let cfg = config(&home, &[("a", &a), ("b", &b)]);
    let state = ClaudeAccountState::new();
    let now = timestamp("2025-01-01T12:00:00Z");
    observe_rate_limit(&state, "a", &window(0.95), Some(now));
    observe_rate_limit(&state, "b", &window(0.92), Some(now));
    let busy = BTreeSet::from(["b".to_string()]);
    let chosen = select(&state, &cfg, Some("claude-fable-5-1"), &HashSet::new(), Some(&busy), Some(now), None, 0.0)
      .unwrap()
      .unwrap();
    assert_eq!(chosen.label, "a");
  }

  #[test]
  fn scoped_panel_usage_is_not_hidden_by_a_newer_rate_event() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let a = root.path().join("a");
    add_credentials(&a);
    let cfg = config(&home, &[("a", &a)]);
    let state = ClaudeAccountState::new();
    let now = timestamp("2025-01-01T12:00:00Z");
    observe_usage_panel(
      &state,
      "a",
      &json!({
        "fetched_at":"2025-01-01T11:59:00Z",
        "windows":[{"utilization":85.0,"scope_label":"Fable weekly","window_minutes":10080}]
      }),
    );
    observe_rate_limit(&state, "a", &window(0.20), Some(now));
    assert_eq!(latest_reading(&state, "a", Some("claude-fable-5-1"), Some(now)).unwrap().utilization, 0.85);
    assert!((headroom(&state, "a", Some("claude-fable-5-1"), Some(now)) - 0.15).abs() < 1e-12);
    assert!((headroom(&state, "a", Some("claude-opus-4-8"), Some(now)) - 0.80).abs() < 1e-12);
    let _ = cfg;
  }

  #[test]
  fn transcript_moves_copy_sidecars_in_both_directions_and_refuse_newer_destinations() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    let transcript = source.join("projects").join("cwd").join("session.jsonl");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, "one transcript row\n").unwrap();
    fs::create_dir_all(transcript.with_extension("").join("tool-results")).unwrap();
    fs::write(transcript.with_extension("").join("tool-results/result.txt"), "synthetic result").unwrap();
    let destination_only = destination.join("projects/cwd/session/tool-results/retained.txt");
    fs::create_dir_all(destination_only.parent().unwrap()).unwrap();
    fs::write(&destination_only, "destination-only").unwrap();

    move_transcript("session", &source, &destination).unwrap();
    assert_eq!(fs::read_to_string(destination.join("projects/cwd/session.jsonl")).unwrap(), "one transcript row\n");
    assert_eq!(
      fs::read_to_string(destination.join("projects/cwd/session/tool-results/result.txt")).unwrap(),
      "synthetic result"
    );
    assert_eq!(fs::read_to_string(destination_only).unwrap(), "destination-only");
    move_transcript("session", &destination, &source).unwrap();

    let source_file = source.join("projects/cwd/newer.jsonl");
    let destination_file = destination.join("projects/cwd/newer.jsonl");
    fs::write(&source_file, "old").unwrap();
    fs::write(&destination_file, "newer transcript").unwrap();
    let source_time = UNIX_EPOCH + StdDuration::from_secs(1_600_000_000);
    let destination_time = UNIX_EPOCH + StdDuration::from_secs(1_700_000_000);
    fs::File::open(&source_file).unwrap().set_times(fs::FileTimes::new().set_modified(source_time)).unwrap();
    fs::File::open(&destination_file).unwrap().set_times(fs::FileTimes::new().set_modified(destination_time)).unwrap();
    let error = move_transcript("newer", &source, &destination).unwrap_err().to_string();
    assert!(error.starts_with(GUARD_REFUSAL_MARKER));
    assert!(error.contains("mtime 2023-11-14T22:13:20+00:00"));
    assert_eq!(fs::read_to_string(destination_file).unwrap(), "newer transcript");
  }
}
