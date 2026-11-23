//! Claude account membership, quota readings, reservations, selection, and transcript custody.
//!
//! Selection observes one tier rule: accounts whose headroom strictly exceeds `1 - WARNING_UTILIZATION` form the
//! under-line tier, and when that tier holds any account only it contends. An account at or above the warning line
//! is a last resort shared with no rule of its own.
//!
//! Every placement of a run on a login — a launch, a relay, or a reattach after a restart — reserves a fixed share
//! of the login's five-hour window under the reservation table's one lock, so runs placed together see one another
//! before the readings rise. A placement picks on *unreserved* headroom, the five-hour headroom minus the login's
//! live reservations capped by its raw headroom, and a reservation never refuses a run: the pick overflows to the
//! login whose reservations overshoot least. The probes (`select`, `cool_destination`,
//! `unreserved_cool_destination`) stay read-only.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::UNIX_EPOCH;

use anyhow::{Context, anyhow};
use charliebot_core::backend_types;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types::{RATE_LIMIT_RESETS_AT, RATE_LIMIT_STATUS, RATE_LIMIT_STATUS_REJECTED};
use charliebot_core::text_repr::is_truthy;
use charliebot_registry::backend::{
  IMPROVE_TOKEN_MIN_SECONDS, LaunchContext, MemberFailure, MemberReading, MemberWindow,
};
use charliebot_registry::usage::{self, FIVE_HOUR_WINDOW_MINUTES, WEEKLY_WINDOW_MINUTES};
use charliebot_store::metadata::UtcDateTime;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use walkdir::WalkDir;

use super::{claude_config, login_dirs};

pub const POOL_DOMAIN: &str = "pool";
pub const WARNING_UTILIZATION: f64 = usage::WARNING_UTILIZATION;
pub const AUTH_FAILURE_COOLDOWN: Duration = Duration::minutes(15);
pub const GUARD_REFUSAL_MARKER: &str = "refusing to overwrite newer transcript";
pub const PROBE_TAIL_BYTES: u64 = 16 * 1024;

const REJECTED_WITHOUT_RESET: Duration = Duration::hours(1);
/// The shortest hours-to-reset a score divides by, so a login minutes from its weekly reset stays finite.
const SCORE_HOURS_FLOOR: f64 = 0.5;
/// The weekly window's hours: it scales a score, and a login without a future weekly reset counts this many hours.
const SCORE_SCALE_HOURS: f64 = 168.0;
/// The per-hour headroom band that ties contenders, in units of `SCORE_SCALE_HOURS`: 0.02 × 168 score points.
const SCORE_TIE: f64 = 0.02;
/// The share of a login's five-hour window that every run placed on it reserves (`RESERVED_HEADROOM_PER_RUN`), for
/// as long as the run's placement lives or `RESERVATION_HOLD` passes.
pub const RESERVED_HEADROOM_PER_RUN: f64 = 0.05;
/// How long a placement's reservation counts, from the moment it was placed, even while its handle lives.
pub const RESERVATION_HOLD: Duration = Duration::minutes(30);
const STAGING_SUFFIX: &str = ".staging";

#[derive(Clone, Debug, PartialEq)]
pub struct RateLimitReading {
  pub at: UtcDateTime,
  pub utilization: f64,
  /// The same observation's five-hour utilization, when the observation read that window.
  pub five_hour: Option<f64>,
}

/// The rejection key that holds every model of a login out: a rejection whose reading is the login's whole window,
/// or one that names no model.
const ALL_MODELS: &str = "*";

#[derive(Clone, Debug)]
struct PanelReading {
  at: UtcDateTime,
  windows: Vec<Value>,
}

#[derive(Default)]
struct Readings {
  event: HashMap<String, RateLimitReading>,
  /// The reset of each login's rejection, keyed by `(login, model family)`. A family's rejection holds only
  /// that family. An [`ALL_MODELS`] rejection holds every model of the login.
  rejections: HashMap<(String, String), UtcDateTime>,
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
  /// The credentials warnings already logged: one `(event, credentials path)` pair each. A read of a path that
  /// returns a token forgets its pairs.
  pub(super) credentials: HashSet<(&'static str, PathBuf)>,
  /// The unrecognized limit shapes already warned about: one `(account, slot, reason)` triple each, so a shape
  /// logs once per process.
  pub(super) limit_shapes: HashSet<(String, String, String)>,
}

/// One live run's reservation of one login. The reservation table holds a weak reference. The placed run's launch
/// (`PooledLaunch`) or the recovered run's follow holds the strong one, so the reservation ends when the placement
/// ends, and never counts past `RESERVATION_HOLD`.
#[derive(Debug)]
pub struct ReservationHandle {
  run_id: String,
  label: String,
  placed_at: UtcDateTime,
}

/// The lock order of the account state's three locks: a turn's `pool_choice` first, then the reservation table,
/// then the readings.
#[derive(Default)]
pub struct ClaudeAccountState {
  readings: std::sync::Mutex<Readings>,
  reservations: std::sync::Mutex<Vec<Weak<ReservationHandle>>>,
  pub(super) pool_choice: tokio::sync::Mutex<()>,
  pub quota: QuotaState,
}

impl ClaudeAccountState {
  pub fn new() -> Self {
    Self::default()
  }

  /// The live reservations per login: every placement still held and inside `RESERVATION_HOLD`, one per run on its
  /// newest handle's login.
  pub fn reservation_counts(&self, now: UtcDateTime) -> HashMap<String, usize> {
    let mut table = self.reservations.lock().expect("Claude account reservation table is not poisoned");
    live_counts(&mut table, now)
  }
}

/// The login a placement picked and the handle that holds its reservation.
pub struct LoginReservation {
  pub account: claude_config::ClaudeAccount,
  pub handle: Arc<ReservationHandle>,
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
  login_dirs::config_dir_path(value, home.as_deref())
}

pub fn is_pooled(option: &BackendOption, cfg: &CharlieBotConfig) -> bool {
  option.backend_type == backend_types::CC_CLAUDE
    && !cfg.section::<claude_config::AccountsConfig>("accounts").claude.is_empty()
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
  if option.backend_type != backend_types::CC_CLAUDE {
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
  data.get("claudeAiOauth").and_then(|oauth| oauth.get("accessToken")).is_some_and(is_truthy)
}

/// Whether a login's access token lives at least `IMPROVE_TOKEN_MIN_SECONDS` past `now`. An error names why the login
/// cannot be used for a keyed round. It never carries credential contents.
fn token_life_suffices(account: &claude_config::ClaudeAccount, now: UtcDateTime) -> Result<(), &'static str> {
  let path = Path::new(&account.config_dir).join(login_dirs::CREDENTIALS_FILE);
  let contents = fs::read_to_string(path).map_err(|_| "credentials_unreadable")?;
  let data = serde_json::from_str::<Value>(&contents).map_err(|_| "credentials_invalid_json")?;
  let oauth = data.get("claudeAiOauth").ok_or("claudeAiOauth_missing")?;
  let expires_at = oauth.get("expiresAt").ok_or("expiresAt_missing")?;
  let expires_at_ms = expires_at.as_i64().ok_or("expiresAt_not_numeric")?;
  if expires_at_ms / 1000 - now.into_inner().unix_timestamp() >= IMPROVE_TOKEN_MIN_SECONDS as i64 {
    Ok(())
  } else {
    Err("less_than_round_minimum")
  }
}

/// The logins of the pool that a keyed round cannot start on, because their access token lives less than
/// `IMPROVE_TOKEN_MIN_SECONDS`. The improve launcher checks the same life before the round starts.
pub fn short_token_logins(
  cfg: &CharlieBotConfig,
  account_pool: Option<&str>,
  now: Option<UtcDateTime>,
) -> anyhow::Result<HashSet<String>> {
  let moment = now_or(now);
  let mut short = HashSet::new();
  for account in pool_accounts(cfg, account_pool)? {
    if !credentials_present(&account) {
      continue;
    }
    if let Err(reason) = token_life_suffices(&account, moment) {
      tracing::warn!(
        target: "charliebot_agents",
        account = %account.label,
        reason,
        minimum_seconds = IMPROVE_TOKEN_MIN_SECONDS,
        "claude_account_excluded_for_improve_token_life"
      );
      short.insert(account.label);
    }
  }
  Ok(short)
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

/// Record one login's rate-limit event for `model`. A rejection is held under the model's family, or under
/// [`ALL_MODELS`] when the login's reading is its whole window (1.0), which holds every model out.
pub fn observe_rate_limit(
  state: &ClaudeAccountState,
  label: &str,
  info: &Value,
  now: Option<UtcDateTime>,
  model: Option<&str>,
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
  let five_hour = windows
    .and_then(|windows| windows.get("five_hour"))
    .and_then(|window| window.get("utilization"))
    .and_then(Value::as_f64);
  let rejected = info.get(RATE_LIMIT_STATUS).and_then(Value::as_str) == Some(RATE_LIMIT_STATUS_REJECTED);
  if values.is_empty() && !rejected {
    return None;
  }
  let utilization = values.into_iter().reduce(f64::max).unwrap_or(1.0);
  let mut readings = state.readings.lock().expect("Claude account state is not poisoned");
  if rejected {
    let reset = info
      .get(RATE_LIMIT_RESETS_AT)
      .and_then(Value::as_f64)
      .map_or_else(|| add_duration(moment, REJECTED_WITHOUT_RESET), unix_seconds);
    let key = if utilization >= 1.0 { ALL_MODELS.to_string() } else { rejection_key(model) };
    readings.rejections.insert((label.to_string(), key), reset);
  }
  let reading = RateLimitReading { at: moment, utilization, five_hour };
  readings.event.insert(label.to_string(), reading.clone());
  Some(reading)
}

/// The rejection key of `model`: its family, or [`ALL_MODELS`] when the model names no family.
fn rejection_key(model: Option<&str>) -> String {
  let family = model_family(model);
  if family.is_empty() { ALL_MODELS.to_string() } else { family }
}

/// The reset that holds `model` out on `label`: the later of the unexpired rejections that cover it, the login's
/// whole-window rejection and the model's own. `None` when no unexpired rejection covers the model.
pub fn rejected_until(
  state: &ClaudeAccountState,
  label: &str,
  model: Option<&str>,
  now: Option<UtcDateTime>,
) -> Option<UtcDateTime> {
  let moment = now_or(now);
  let family = rejection_key(model);
  let readings = state.readings.lock().expect("Claude account state is not poisoned");
  [ALL_MODELS, family.as_str()]
    .into_iter()
    .filter_map(|key| readings.rejections.get(&(label.to_string(), key.to_string())).copied())
    .filter(|reset| *reset > moment)
    .max()
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

fn panel_scope_matches(window: &Value, family: &str) -> bool {
  let scope = window.get(usage::PANEL_SCOPE_LABEL);
  if !scope.is_some_and(is_truthy) {
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
    .filter(|window| !usage::panel_window_expired(window, reading.at, now) && panel_scope_matches(window, &family))
    .collect()
}

fn panel_reading(reading: &PanelReading, model: Option<&str>, now: UtcDateTime) -> Option<RateLimitReading> {
  let windows = live_windows(reading, model, now);
  let utilization = windows
    .iter()
    .filter_map(|window| window.get(usage::PANEL_UTILIZATION).and_then(Value::as_f64))
    .map(|value| value / 100.0)
    .reduce(f64::max)?;
  let five_hour = windows
    .iter()
    .filter(|window| {
      window.get(usage::PANEL_WINDOW_MINUTES).and_then(Value::as_i64) == Some(FIVE_HOUR_WINDOW_MINUTES as i64)
    })
    .filter_map(|window| window.get(usage::PANEL_UTILIZATION).and_then(Value::as_f64))
    .map(|value| value / 100.0)
    .reduce(f64::max);
  Some(RateLimitReading { at: reading.at, utilization, five_hour })
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
    .any(|window| window.get(usage::PANEL_SCOPE_LABEL).is_some_and(is_truthy))
  {
    return general;
  }
  let Some(scoped) = panel_reading(panel, Some(model), moment) else {
    return general;
  };
  Some(RateLimitReading {
    at: general.as_ref().map_or(scoped.at, |reading| reading.at.max(scoped.at)),
    utilization: general.as_ref().map_or(scoped.utilization, |reading| reading.utilization.max(scoped.utilization)),
    five_hour: match (general.and_then(|reading| reading.five_hour), scoped.five_hour) {
      (Some(left), Some(right)) => Some(left.max(right)),
      (left, right) => left.or(right),
    },
  })
}

/// The login's headroom for `model`: 0 while a rejection holds the model out, else one minus the reading.
pub fn headroom(state: &ClaudeAccountState, label: &str, model: Option<&str>, now: Option<UtcDateTime>) -> f64 {
  let moment = now_or(now);
  if rejected_until(state, label, model, Some(moment)).is_some() {
    return 0.0;
  }
  let Some(reading) = latest_reading(state, label, model, Some(moment)) else {
    return 1.0;
  };
  (1.0 - reading.utilization).max(0.0)
}

/// The candidates tiering lets contend for selection. The healthy, unexcluded accounts of the pool whose headroom
/// exceeds `min_headroom` contend, those below the 90% line first and the accounts at or above it as the last
/// resort.
/// Login selection and the later member selection tier through this one function.
#[allow(clippy::too_many_arguments)]
pub fn tiered_candidates(
  state: &ClaudeAccountState,
  cfg: &CharlieBotConfig,
  model: Option<&str>,
  exclude: &HashSet<String>,
  now: Option<UtcDateTime>,
  account_pool: Option<&str>,
  min_headroom: f64,
) -> anyhow::Result<Vec<(claude_config::ClaudeAccount, f64)>> {
  let moment = now_or(now);
  let available = pool_accounts(cfg, account_pool)?
    .into_iter()
    .filter(|account| !exclude.contains(&account.label) && healthy(state, account, Some(moment)))
    .filter_map(|account| {
      let remaining = headroom(state, &account.label, model, Some(moment));
      (remaining > min_headroom).then_some((account, remaining))
    })
    .collect::<Vec<_>>();
  if available.is_empty() {
    return Ok(available);
  }
  let under_line =
    available.iter().filter(|(_, remaining)| *remaining > 1.0 - WARNING_UTILIZATION).cloned().collect::<Vec<_>>();
  Ok(if under_line.is_empty() { available } else { under_line })
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
  let tier = tiered_candidates(state, cfg, model, exclude, Some(moment), account_pool, min_headroom)?;
  scored_pick(state, model, busy_accounts, moment, tier)
}

/// The contenders' pick: an idle login ahead of a busy one, the balancing score on `weight` with its tie group, and
/// inside the group the most weight, then the least recently read `rate_limit_event`, then pool order.
fn scored_pick(
  state: &ClaudeAccountState,
  model: Option<&str>,
  busy: Option<&BTreeSet<String>>,
  moment: UtcDateTime,
  contenders: Vec<(claude_config::ClaudeAccount, f64)>,
) -> anyhow::Result<Option<claude_config::ClaudeAccount>> {
  if contenders.is_empty() {
    return Ok(None);
  }
  let idle = contenders
    .iter()
    .filter(|(account, _)| busy.is_none_or(|busy| !busy.contains(&account.label)))
    .cloned()
    .collect::<Vec<_>>();
  let contenders = if idle.is_empty() { contenders } else { idle };
  let scores = contenders
    .into_iter()
    .map(|(account, weight)| {
      let weekly_resets_at =
        login_reading(state, &account.label, model, Some(moment)).and_then(|reading| reading.weekly_resets_at);
      let hours_to_reset = match weekly_resets_at {
        Some(reset) if reset > moment => {
          let hours = (reset.into_inner() - moment.into_inner()).as_seconds_f64() / 3600.0;
          hours.max(SCORE_HOURS_FLOOR)
        }
        Some(_) | None => SCORE_SCALE_HOURS,
      };
      let score = weight * SCORE_SCALE_HOURS / hours_to_reset;
      (account, weight, score)
    })
    .collect::<Vec<_>>();
  let best = scores.iter().map(|(_, _, score)| *score).reduce(f64::max).expect("contenders is non-empty");
  let readings = state.readings.lock().expect("Claude account state is not poisoned");
  let read_at =
    |label: &str| readings.event.get(label).map_or(i128::MIN, |reading| reading.at.into_inner().unix_timestamp_nanos());
  Ok(
    scores
      .into_iter()
      .filter(|(_, _, score)| (best - score) / SCORE_SCALE_HOURS <= SCORE_TIE)
      .min_by(|(left, left_weight, _), (right, right_weight, _)| {
        right_weight.total_cmp(left_weight).then(read_at(&left.label).cmp(&read_at(&right.label)))
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

/// The headroom a login's reservations leave: the five-hour headroom minus every live run's share, capped by the raw
/// headroom. A login whose newest reading read no five-hour window reserves against the raw headroom.
fn unreserved_headroom(reading: Option<&RateLimitReading>, remaining: f64, handles: usize) -> f64 {
  let reserved = RESERVED_HEADROOM_PER_RUN * handles as f64;
  match reading.and_then(|reading| reading.five_hour.map(|utilization| (1.0 - utilization).max(0.0))) {
    Some(five_hour) => (five_hour - reserved).min(remaining),
    None => remaining - reserved,
  }
}

/// One login's unreserved candidacy: its raw headroom and the headroom its reservations leave.
struct UnreservedCandidate {
  account: claude_config::ClaudeAccount,
  unreserved: f64,
}

/// The pool's healthy logins with headroom left, with the headroom their reservations leave.
fn unreserved_candidates(
  state: &ClaudeAccountState,
  cfg: &CharlieBotConfig,
  model: Option<&str>,
  exclude: &HashSet<String>,
  moment: UtcDateTime,
  account_pool: Option<&str>,
  counts: &HashMap<String, usize>,
) -> anyhow::Result<Vec<UnreservedCandidate>> {
  Ok(
    pool_accounts(cfg, account_pool)?
      .into_iter()
      .filter(|account| !exclude.contains(&account.label) && healthy(state, account, Some(moment)))
      .filter_map(|account| {
        let remaining = headroom(state, &account.label, model, Some(moment));
        if remaining <= 0.0 {
          return None;
        }
        let reading = latest_reading(state, &account.label, model, Some(moment));
        let unreserved =
          unreserved_headroom(reading.as_ref(), remaining, counts.get(&account.label).copied().unwrap_or(0));
        Some(UnreservedCandidate { account, unreserved })
      })
      .collect(),
  )
}

/// The tier a placement takes and the contenders it ranks: the under-line tier first, then the logins with any
/// unreserved headroom, then the overflow on every healthy login with headroom. The tier names the
/// `claude_login_reserved` outcome.
fn unreserved_tier(
  candidates: Vec<UnreservedCandidate>,
) -> Option<(&'static str, Vec<(claude_config::ClaudeAccount, f64)>)> {
  let into = |candidates: Vec<UnreservedCandidate>| {
    candidates.into_iter().map(|candidate| (candidate.account, candidate.unreserved)).collect::<Vec<_>>()
  };
  let under_line = candidates.iter().filter(|candidate| candidate.unreserved > 1.0 - WARNING_UTILIZATION).count();
  if under_line > 0 {
    let tier = candidates.into_iter().filter(|candidate| candidate.unreserved > 1.0 - WARNING_UTILIZATION).collect();
    return Some(("redirect", into(tier)));
  }
  let positive = candidates.iter().filter(|candidate| candidate.unreserved > 0.0).count();
  if positive > 0 {
    let tier = candidates.into_iter().filter(|candidate| candidate.unreserved > 0.0).collect();
    return Some(("redirect", into(tier)));
  }
  (!candidates.is_empty()).then(|| ("overflow", into(candidates)))
}

/// Count the live handles per login, pruning the ones whose placement ended or whose hold passed.
fn live_counts(table: &mut Vec<Weak<ReservationHandle>>, now: UtcDateTime) -> HashMap<String, usize> {
  let mut counts = HashMap::new();
  table.retain(|weak| match weak.upgrade() {
    Some(handle) if now.into_inner() - handle.placed_at.into_inner() < RESERVATION_HOLD => {
      *counts.entry(handle.label.clone()).or_insert(0) += 1;
      true
    }
    Some(_) | None => false,
  });
  counts
}

/// Insert a run's handle, dropping its earlier handles: only the run's newest handle's login counts.
fn register_reservation(
  table: &mut Vec<Weak<ReservationHandle>>,
  run_id: &str,
  label: &str,
  moment: UtcDateTime,
) -> Arc<ReservationHandle> {
  table.retain(|weak| weak.upgrade().is_none_or(|handle| handle.run_id != run_id));
  let handle = Arc::new(ReservationHandle { run_id: run_id.to_string(), label: label.to_string(), placed_at: moment });
  table.push(Arc::downgrade(&handle));
  handle
}

/// Pick a login under the reservation table's lock, read the reservations, and insert the run's handle. The pick
/// never refuses a run while any healthy login has headroom: it overflows to the login whose reservations overshoot
/// least. `None` means no healthy login has headroom, and the caller refuses on the pool's own terms.
#[allow(clippy::too_many_arguments)]
pub fn reserve_pick(
  state: &ClaudeAccountState,
  cfg: &CharlieBotConfig,
  model: Option<&str>,
  exclude: &HashSet<String>,
  busy: Option<&BTreeSet<String>>,
  now: Option<UtcDateTime>,
  account_pool: Option<&str>,
  run_id: &str,
) -> anyhow::Result<Option<LoginReservation>> {
  let moment = now_or(now);
  let mut table = state.reservations.lock().expect("Claude account reservation table is not poisoned");
  let counts = live_counts(&mut table, moment);
  let candidates = unreserved_candidates(state, cfg, model, exclude, moment, account_pool, &counts)?;
  let Some((outcome, contenders)) = unreserved_tier(candidates) else {
    return Ok(None);
  };
  let Some(account) = scored_pick(state, model, busy, moment, contenders)? else {
    return Ok(None);
  };
  let handles = counts.get(&account.label).copied().unwrap_or(0);
  let handle = register_reservation(&mut table, run_id, &account.label, moment);
  tracing::info!(
    target: "charliebot_agents",
    account = %account.label,
    run_id,
    outcome,
    handles,
    "claude_login_reserved"
  );
  Ok(Some(LoginReservation { account, handle }))
}

/// Register a run's handle on a known login, without picking: a holder's return, a kept turn, or a reattach.
pub fn reserve_on(
  state: &ClaudeAccountState,
  run_id: &str,
  label: &str,
  outcome: &str,
  now: Option<UtcDateTime>,
) -> Arc<ReservationHandle> {
  let moment = now_or(now);
  let mut table = state.reservations.lock().expect("Claude account reservation table is not poisoned");
  let handle = register_reservation(&mut table, run_id, label, moment);
  tracing::info!(target: "charliebot_agents", account = label, run_id, outcome, "claude_login_reserved");
  handle
}

/// The login a warning relay can move a run to: one whose reservations leave it more than `1 - WARNING_UTILIZATION`
/// unreserved headroom. It is a read-only probe. A move registers through [`reserve_pick`] or [`reserve_on`].
pub fn unreserved_cool_destination(
  state: &ClaudeAccountState,
  cfg: &CharlieBotConfig,
  model: Option<&str>,
  account_pool: Option<&str>,
  now: Option<UtcDateTime>,
) -> anyhow::Result<Option<claude_config::ClaudeAccount>> {
  let moment = now_or(now);
  let mut table = state.reservations.lock().expect("Claude account reservation table is not poisoned");
  let counts = live_counts(&mut table, moment);
  let candidates = unreserved_candidates(state, cfg, model, &HashSet::new(), moment, account_pool, &counts)?;
  let under_line = candidates
    .into_iter()
    .filter(|candidate| candidate.unreserved > 1.0 - WARNING_UTILIZATION)
    .map(|candidate| (candidate.account, candidate.unreserved))
    .collect::<Vec<_>>();
  scored_pick(state, model, None, moment, under_line)
}

/// The login whose credentials directory the process `pid` carries as `CLAUDE_CONFIG_DIR` in its environment.
fn process_login(cfg: &CharlieBotConfig, pid: u32) -> anyhow::Result<Option<claude_config::ClaudeAccount>> {
  let data = match fs::read(format!("/proc/{pid}/environ")) {
    Ok(data) => data,
    Err(_) => return Ok(None),
  };
  let prefix = format!("{}=", login_dirs::CLAUDE_CONFIG_DIR_ENV);
  let Some(config_dir) = data
    .split(|byte| *byte == 0)
    .filter_map(|entry| String::from_utf8(entry.to_vec()).ok())
    .find_map(|entry| entry.strip_prefix(&prefix).map(str::to_string))
  else {
    return Ok(None);
  };
  let config_dir = PathBuf::from(config_dir);
  Ok(
    pool(cfg)?
      .into_iter()
      .find(|account| expand_account_path(&account.config_dir).is_ok_and(|expanded| expanded == config_dir)),
  )
}

/// The hold a restart reattach takes: the login that the recovered run's process already uses, reserved for the run.
/// `None` when the process carries no pool login's directory. The caller logs the unknown login and follows anyway.
pub fn reattach_hold(
  state: &ClaudeAccountState,
  cfg: &CharlieBotConfig,
  pid: u32,
  run_id: &str,
  now: Option<UtcDateTime>,
) -> anyhow::Result<Option<Arc<ReservationHandle>>> {
  let Some(account) = process_login(cfg, pid)? else {
    return Ok(None);
  };
  Ok(Some(reserve_on(state, run_id, &account.label, "reattach", now)))
}

/// The dedup key of a placement: the task-tree Run id, or the session id when the run records none.
pub fn run_key(ctx: &LaunchContext) -> String {
  ctx
    .run_id
    .clone()
    .filter(|run_id| !run_id.is_empty())
    .unwrap_or_else(|| ctx.session_meta.lock().expect("launch metadata mutex is not poisoned").id.clone())
}

/// One login's per-window reading: its utilization, the limiting window's reset, its weekly window's reset, and every
/// live window.
#[derive(Clone, Debug, PartialEq)]
pub struct LoginReading {
  /// The login's utilization, `1.0` once it rejects requests.
  pub utilization: f64,
  /// The reset of the window with the highest utilization, or the rejection's reset.
  pub limiting_resets_at: Option<UtcDateTime>,
  /// The reset of the weekly (7-day) window.
  pub weekly_resets_at: Option<UtcDateTime>,
  /// The login's live windows.
  pub windows: Vec<MemberWindow>,
}

/// Read one login's windows from its newest event and panel observations. `None` when no observation exists.
pub fn login_reading(
  state: &ClaudeAccountState,
  label: &str,
  model: Option<&str>,
  now: Option<UtcDateTime>,
) -> Option<LoginReading> {
  let moment = now_or(now);
  let reading = latest_reading(state, label, model, Some(moment));
  let windows = {
    let readings = state.readings.lock().expect("Claude account state is not poisoned");
    readings
      .panel
      .get(label)
      .map(|panel| {
        live_windows(panel, model, moment)
          .into_iter()
          .filter_map(|window| {
            let utilization = window.get(usage::PANEL_UTILIZATION).and_then(Value::as_f64)? / 100.0;
            let window_minutes = window.get(usage::PANEL_WINDOW_MINUTES).and_then(Value::as_i64);
            let resets_at = window
              .get(usage::PANEL_RESETS_AT)
              .and_then(Value::as_str)
              .and_then(|value| UtcDateTime::parse(value).ok());
            Some(MemberWindow { utilization, window_minutes, resets_at })
          })
          .collect::<Vec<_>>()
      })
      .unwrap_or_default()
  };
  let reading = reading?;
  let rejection = rejected_until(state, label, model, Some(moment));
  let limiting_resets_at = if rejection.is_some() {
    rejection
  } else {
    windows
      .iter()
      .filter(|window| window.resets_at.is_some())
      .max_by(|left, right| {
        left
          .utilization
          .total_cmp(&right.utilization)
          .then(left.window_minutes.unwrap_or_default().cmp(&right.window_minutes.unwrap_or_default()))
      })
      .and_then(|window| window.resets_at)
  };
  let weekly_resets_at = windows
    .iter()
    .filter(|window| window.window_minutes == Some(WEEKLY_WINDOW_MINUTES))
    .max_by(|left, right| left.utilization.total_cmp(&right.utilization))
    .and_then(|window| window.resets_at);
  Some(LoginReading {
    utilization: if rejection.is_some() { 1.0 } else { reading.utilization },
    limiting_resets_at,
    weekly_resets_at,
    windows,
  })
}

/// The Auto set's reading of one Claude member. It carries the best login's windows and weekly reset, and the
/// earliest reset among the healthy logins. It also says whether every healthy login sits at or above the 90% line
/// with a reset beyond 45 minutes, whether no healthy login has headroom left, and whether every login that has
/// headroom has all of it reserved by the runs placed on it.
pub fn member_reading(
  state: &ClaudeAccountState,
  cfg: &CharlieBotConfig,
  option: &BackendOption,
  now: UtcDateTime,
) -> anyhow::Result<Option<MemberReading>> {
  let accounts = pool_accounts(cfg, option_pool(option).as_deref())?;
  if accounts.is_empty() {
    return Ok(None);
  }
  let model = Some(option.model.as_str());
  let healthy = accounts.iter().filter(|account| healthy(state, account, Some(now))).collect::<Vec<_>>();
  if healthy.is_empty() {
    return Ok(Some(MemberReading {
      windows: Vec::new(),
      weekly_resets_at: None,
      near_exhaustion: false,
      exhausted: true,
      fully_reserved: false,
    }));
  }
  let counts = state.reservation_counts(now);
  let mut with_headroom = 0_usize;
  let mut unreserved_left = false;
  for account in &healthy {
    let remaining = headroom(state, &account.label, model, Some(now));
    if remaining > 0.0 {
      with_headroom += 1;
      let reading = latest_reading(state, &account.label, model, Some(now));
      if unreserved_headroom(reading.as_ref(), remaining, counts.get(&account.label).copied().unwrap_or(0)) > 0.0 {
        unreserved_left = true;
      }
    }
  }
  let fully_reserved = with_headroom > 0 && !unreserved_left;
  let readings =
    healthy.iter().filter_map(|account| login_reading(state, &account.label, model, Some(now))).collect::<Vec<_>>();
  let exhausted = healthy.iter().all(|account| headroom(state, &account.label, model, Some(now)) <= 0.0);
  let Some(best) = readings.iter().min_by(|left, right| left.utilization.total_cmp(&right.utilization)) else {
    // Health checks pass, but the panel has reported no reading yet: the member has none until it does.
    return Ok(None);
  };
  let earliest_reset = readings.iter().filter_map(|reading| reading.limiting_resets_at).min();
  let near_exhaustion = readings.len() == healthy.len()
    && readings.iter().all(|reading| reading.utilization >= WARNING_UTILIZATION)
    && earliest_reset.is_some_and(|reset| reset.into_inner() > now.into_inner() + usage::NEAR_EXHAUSTION_RESET_HORIZON);
  Ok(Some(MemberReading {
    windows: best.windows.clone(),
    weekly_resets_at: best.weekly_resets_at,
    near_exhaustion,
    exhausted,
    fully_reserved,
  }))
}

/// The earliest instant a login of the pool can serve `model` again, from the rejections that hold it out.
pub fn earliest_reset(
  state: &ClaudeAccountState,
  cfg: &CharlieBotConfig,
  model: Option<&str>,
  now: Option<UtcDateTime>,
  account_pool: Option<&str>,
) -> anyhow::Result<Option<UtcDateTime>> {
  let moment = now_or(now);
  Ok(
    pool_accounts(cfg, account_pool)?
      .iter()
      .filter_map(|account| rejected_until(state, &account.label, model, Some(moment)))
      .min(),
  )
}

/// Why a pool has no login it can launch, and when its earliest login can run again.
pub struct NoUsableLogin {
  /// The shared sort of the refusal: a spent quota when any login is out for quota, else an access refusal.
  pub failure: MemberFailure,
  /// The earliest instant a login of the pool can run again, or `None` when no login states one.
  pub recovers_at: Option<UtcDateTime>,
}

/// Why the pool has no login to launch, and when its earliest login can run again.
///
/// A login without credentials runs as soon as credentials are written, so it recovers now. A login under its
/// authentication-failure disable recovers when the disable ends. A login out for quota recovers at the reset its
/// rejection stated, when it stated one. When no login is out for quota, the refusal is an access refusal.
pub fn no_usable_login(
  state: &ClaudeAccountState,
  cfg: &CharlieBotConfig,
  model: Option<&str>,
  now: Option<UtcDateTime>,
  account_pool: Option<&str>,
) -> anyhow::Result<NoUsableLogin> {
  let moment = now_or(now);
  let mut out_for_quota = false;
  let mut recovers_at: Option<UtcDateTime> = None;
  for account in pool_accounts(cfg, account_pool)? {
    let (quota, recovery) = login_unavailability(state, &account, model, moment);
    out_for_quota |= quota;
    if let Some(time) = recovery {
      recovers_at = Some(recovers_at.map_or(time, |earliest| earliest.min(time)));
    }
  }
  let failure = if out_for_quota { MemberFailure::Quota } else { MemberFailure::AccessRefused };
  Ok(NoUsableLogin { failure, recovers_at })
}

/// When one login can run again, and whether a spent quota is what holds it out. A login held out for quota and under
/// its authentication disable runs again once both have ended.
fn login_unavailability(
  state: &ClaudeAccountState,
  account: &claude_config::ClaudeAccount,
  model: Option<&str>,
  now: UtcDateTime,
) -> (bool, Option<UtcDateTime>) {
  if !credentials_present(account) {
    return (false, Some(now));
  }
  let failed =
    state.readings.lock().expect("Claude account state is not poisoned").auth_failures.get(&account.label).copied();
  let disabled_until = failed
    .filter(|failed| now.into_inner() - failed.into_inner() < AUTH_FAILURE_COOLDOWN)
    .map(|failed| add_duration(failed, AUTH_FAILURE_COOLDOWN));
  if headroom(state, &account.label, model, Some(now)) <= 0.0 {
    let reset = rejected_until(state, &account.label, model, Some(now));
    let recovers = match disabled_until {
      Some(disabled) => reset.map(|reset| reset.max(disabled)),
      None => reset,
    };
    return (true, recovers);
  }
  (false, Some(disabled_until.unwrap_or(now)))
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
/// a literal path, and the session id joins the pattern unescaped, so its glob characters take effect.
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
  Ok(to_utc_datetime(OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos))?).to_isoformat_text())
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

fn path_list_repr(paths: impl IntoIterator<Item = PathBuf>) -> String {
  let values = paths.into_iter().map(|path| format!("'{}'", path.display())).collect::<Vec<_>>();
  format!("[{}]", values.join(", "))
}

fn sidecar_file_diff(src: &Path, dst: &Path) -> anyhow::Result<String> {
  let src_files = sidecar_files(src)?;
  let dst_files = sidecar_files(dst)?;
  let mut parts = Vec::new();
  let missing = src_files.difference(&dst_files).cloned().collect::<Vec<_>>();
  if !missing.is_empty() {
    parts.push(format!("missing at dst: {}", path_list_repr(missing)));
  }
  let extra = dst_files.difference(&src_files).cloned().collect::<Vec<_>>();
  if !extra.is_empty() {
    parts.push(format!("only at dst: {}", path_list_repr(extra)));
  }
  let mut differing = Vec::new();
  for relative in src_files.intersection(&dst_files) {
    if file_size(&src.join(relative))? != file_size(&dst.join(relative))? {
      differing.push(relative.clone());
    }
  }
  if !differing.is_empty() {
    parts.push(format!("size differs: {}", path_list_repr(differing)));
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
    registry.register_package("backends.claude_code", crate::register).unwrap();
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
    observe_rate_limit(&state, "a", &window(0.20), Some(now), None);
    observe_rate_limit(&state, "b", &window(0.60), Some(now), None);
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
    observe_rate_limit(&state, "a", &window(0.89), Some(now), None);
    observe_rate_limit(&state, "b", &window(0.90), Some(now), None);
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
    observe_rate_limit(&state, "a", &window(0.95), Some(now), None);
    observe_rate_limit(&state, "b", &window(0.92), Some(now), None);
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
    observe_rate_limit(&state, "a", &window(0.20), Some(now), None);
    assert_eq!(latest_reading(&state, "a", Some("claude-fable-5-1"), Some(now)).unwrap().utilization, 0.85);
    assert!((headroom(&state, "a", Some("claude-fable-5-1"), Some(now)) - 0.15).abs() < 1e-12);
    assert!((headroom(&state, "a", Some("claude-opus-4-8"), Some(now)) - 0.80).abs() < 1e-12);
    let _ = cfg;
  }

  #[test]
  fn scoped_panel_usage_is_kept_when_no_general_reading_exists() {
    let state = ClaudeAccountState::new();
    let now = timestamp("2025-01-01T12:00:00Z");
    observe_usage_panel(
      &state,
      "a",
      &json!({
        "fetched_at":"2025-01-01T12:00:00Z",
        "windows":[{
          "utilization":45.0,
          "scope_label":"Fable five-hour",
          "window_minutes":FIVE_HOUR_WINDOW_MINUTES,
          "resets_at":"2025-01-01T17:00:00Z"
        }]
      }),
    );

    let reading = latest_reading(&state, "a", Some("claude-fable-5-1"), Some(now))
      .expect("the scoped panel reading exists without a general reading");

    assert_eq!(reading.at, now);
    assert_eq!(reading.utilization, 0.45);
    assert_eq!(reading.five_hour, Some(0.45));
    // A caller that asks for the general reading gets none: the panel holds only the model's window.
    assert_eq!(latest_reading(&state, "a", None, Some(now)), None);
  }

  #[test]
  fn a_general_panel_window_reads_for_a_model_with_no_scoped_window() {
    let state = ClaudeAccountState::new();
    let now = timestamp("2025-01-01T12:00:00Z");
    observe_usage_panel(
      &state,
      "a",
      &json!({
        "fetched_at":"2025-01-01T12:00:00Z",
        "windows":[{
          "utilization":45.0,
          "window_minutes":FIVE_HOUR_WINDOW_MINUTES,
          "resets_at":"2025-01-01T17:00:00Z"
        }]
      }),
    );

    for model in [Some("claude-fable-5-1"), Some("claude-opus-4-8"), None] {
      let reading = latest_reading(&state, "a", model, Some(now)).expect("the general panel reading exists");

      assert_eq!(reading.at, now);
      assert_eq!(reading.utilization, 0.45);
      assert_eq!(reading.five_hour, Some(0.45));
    }
  }

  #[test]
  fn a_panel_with_general_and_scoped_windows_merges_both_readings() {
    let state = ClaudeAccountState::new();
    let now = timestamp("2025-01-01T12:00:00Z");
    observe_usage_panel(
      &state,
      "a",
      &json!({
        "fetched_at":"2025-01-01T12:00:00Z",
        "windows":[
          {"utilization":90.0,"window_minutes":WEEKLY_WINDOW_MINUTES,"resets_at":"2025-01-05T12:00:00Z"},
          {"utilization":20.0,"window_minutes":FIVE_HOUR_WINDOW_MINUTES,"resets_at":"2025-01-01T17:00:00Z"},
          {
            "utilization":50.0,
            "scope_label":"Fable five-hour",
            "window_minutes":FIVE_HOUR_WINDOW_MINUTES,
            "resets_at":"2025-01-01T17:00:00Z"
          }
        ]
      }),
    );

    let reading = latest_reading(&state, "a", Some("claude-fable-5-1"), Some(now)).expect("both readings exist");

    // The general window carries the larger utilization. The scoped five-hour window carries the larger five-hour.
    assert_eq!(reading.at, now);
    assert_eq!(reading.utilization, 0.90);
    assert_eq!(reading.five_hour, Some(0.50));
  }

  #[test]
  fn a_panel_with_no_window_for_the_model_reads_nothing() {
    let state = ClaudeAccountState::new();
    let now = timestamp("2025-01-01T12:00:00Z");
    assert_eq!(latest_reading(&state, "a", Some("claude-fable-5-1"), Some(now)), None);
    assert_eq!(latest_reading(&state, "a", None, Some(now)), None);

    observe_usage_panel(
      &state,
      "a",
      &json!({
        "fetched_at":"2025-01-01T12:00:00Z",
        "windows":[{
          "utilization":80.0,
          "scope_label":"Opus five-hour",
          "window_minutes":FIVE_HOUR_WINDOW_MINUTES,
          "resets_at":"2025-01-01T17:00:00Z"
        }]
      }),
    );
    assert_eq!(latest_reading(&state, "a", Some("claude-fable-5-1"), Some(now)), None);
    assert_eq!(latest_reading(&state, "a", None, Some(now)), None);
  }

  fn panel_window(utilization: f64, minutes: i64, resets_at: &str) -> Value {
    json!({"fetched_at":"2025-01-01T11:59:00Z","windows":[{"utilization":utilization,"window_minutes":minutes,"resets_at":resets_at}]})
  }

  #[test]
  fn the_member_reading_takes_the_best_login_and_orders_near_exhaustion() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    add_credentials(&a);
    add_credentials(&b);
    let cfg = config(&home, &[("a", &a), ("b", &b)]);
    let option = cfg.get_backend_option("fable").expect("the fable option").clone();
    let state = ClaudeAccountState::new();
    let now = timestamp("2025-01-01T12:00:00Z");
    let weekly = "2025-01-08T12:00:00Z";
    observe_usage_panel(&state, "a", &panel_window(20.0, WEEKLY_WINDOW_MINUTES, weekly));
    observe_usage_panel(&state, "b", &panel_window(40.0, WEEKLY_WINDOW_MINUTES, weekly));

    let reading = member_reading(&state, &cfg, &option, now).unwrap().expect("both logins read");
    assert!((reading.windows[0].utilization - 0.20).abs() < 1e-12, "the lower login is the member's reading");
    assert_eq!(reading.windows[0].window_minutes, Some(WEEKLY_WINDOW_MINUTES));
    assert_eq!(reading.weekly_resets_at, Some(timestamp(weekly)));
    assert!(!reading.near_exhaustion, "a login below the line keeps the member selectable");
    assert!(!reading.exhausted);

    // Every healthy login at the line, with its reset beyond the 45-minute horizon, is near exhaustion.
    observe_usage_panel(&state, "b", &panel_window(97.0, WEEKLY_WINDOW_MINUTES, weekly));
    observe_usage_panel(&state, "a", &panel_window(95.0, WEEKLY_WINDOW_MINUTES, weekly));
    let reading = member_reading(&state, &cfg, &option, now).unwrap().expect("both logins read");
    assert!(reading.near_exhaustion);
    assert!(!reading.exhausted);

    // No healthy login has headroom once the last one rejects.
    let rejected =
      json!({"unifiedWindows":{"five_hour":{"utilization":1.0}},"status":"rejected","resetsAt":4102462800.0});
    observe_rate_limit(&state, "a", &rejected, Some(now), None);
    observe_rate_limit(&state, "b", &rejected, Some(now), None);
    let reading = member_reading(&state, &cfg, &option, now).unwrap().expect("rejected logins still read");
    assert!(reading.exhausted);
  }

  #[test]
  fn the_member_reading_of_a_pool_with_no_reading_is_none() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let a = root.path().join("a");
    add_credentials(&a);
    let cfg = config(&home, &[("a", &a)]);
    let option = cfg.get_backend_option("fable").expect("the fable option").clone();
    let state = ClaudeAccountState::new();

    assert_eq!(member_reading(&state, &cfg, &option, timestamp("2025-01-01T12:00:00Z")).unwrap(), None);
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

  /// A pool of freshly credentialed logins in the given order, with its config and an empty account state.
  fn pool_fixture(labels: &[&str]) -> (tempfile::TempDir, CharlieBotConfig, ClaudeAccountState) {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let dirs = labels.iter().map(|label| (*label, root.path().join(label))).collect::<Vec<_>>();
    for (_, dir) in &dirs {
      add_credentials(dir);
    }
    let entries = dirs.iter().map(|(label, dir)| (*label, dir.as_path())).collect::<Vec<_>>();
    let cfg = config(&home, &entries);
    (root, cfg, ClaudeAccountState::new())
  }

  fn select_now(state: &ClaudeAccountState, cfg: &CharlieBotConfig, now: UtcDateTime) -> String {
    select(state, cfg, None, &HashSet::new(), None, Some(now), None, 0.0).unwrap().unwrap().label
  }

  #[test]
  fn less_headroom_with_reset_hours_away_beats_more_headroom_with_far_reset() {
    let (_root, cfg, state) = pool_fixture(&["a", "b"]);
    let now = timestamp("2025-01-01T12:00:00Z");
    // "a": 30% headroom, reset in 2 hours -> 0.30 * 168 / 2 = 25.2.
    observe_usage_panel(&state, "a", &panel_window(70.0, WEEKLY_WINDOW_MINUTES, "2025-01-01T14:00:00Z"));
    // "b": 70% headroom, reset in 100 hours -> 0.70 * 168 / 100 = 1.176.
    observe_usage_panel(&state, "b", &panel_window(30.0, WEEKLY_WINDOW_MINUTES, "2025-01-05T16:00:00Z"));
    assert_eq!(select_now(&state, &cfg, now), "a");
  }

  #[test]
  fn reset_minutes_away_scores_on_the_half_hour_floor() {
    let (_root, cfg, state) = pool_fixture(&["a", "b"]);
    let now = timestamp("2025-01-01T12:00:00Z");
    // "a": 15% headroom, reset in 6 minutes -> floored to 0.5 h -> 0.15 * 168 / 0.5 = 50.4.
    // Without the floor its 0.1 h gives 252 and it wins instead.
    observe_usage_panel(&state, "a", &panel_window(85.0, WEEKLY_WINDOW_MINUTES, "2025-01-01T12:06:00Z"));
    // "b": 70% headroom, reset in 2 hours -> 0.70 * 168 / 2 = 58.8.
    observe_usage_panel(&state, "b", &panel_window(30.0, WEEKLY_WINDOW_MINUTES, "2025-01-01T14:00:00Z"));
    assert_eq!(select_now(&state, &cfg, now), "b");
  }

  #[test]
  fn a_login_without_a_weekly_reading_counts_168_hours() {
    let now = timestamp("2025-01-01T12:00:00Z");
    {
      // "x" has no weekly reading, so its 30% headroom scores 0.30 over 168 h. "y" has 50% headroom at 10 h
      // and scores 8.4. Without the 168 h value "x" would score at the 0.5 h floor (100.8) and win.
      let (_root, cfg, state) = pool_fixture(&["x", "y"]);
      observe_rate_limit(&state, "x", &window(0.70), Some(now), None);
      observe_usage_panel(&state, "y", &panel_window(50.0, WEEKLY_WINDOW_MINUTES, "2025-01-01T22:00:00Z"));
      assert_eq!(select_now(&state, &cfg, now), "y");
    }
    {
      // "x" (30% over 168 h) and "z" (20% over 168 h) tie. "x" carries the most headroom.
      let (_root, cfg, state) = pool_fixture(&["x", "z"]);
      observe_rate_limit(&state, "x", &window(0.70), Some(now), None);
      observe_usage_panel(&state, "z", &panel_window(80.0, WEEKLY_WINDOW_MINUTES, "2025-01-08T12:00:00Z"));
      assert_eq!(select_now(&state, &cfg, now), "x");
    }
  }

  #[test]
  fn a_weekly_reset_that_already_passed_counts_as_a_fresh_week_not_the_floor() {
    let (_root, cfg, state) = pool_fixture(&["a", "b"]);
    let now = timestamp("2025-01-01T12:00:00Z");
    // "a": reset passed an hour ago -> counts 168 h, not the 0.5 h floor. 0.50 * 168 / 168 = 0.50.
    observe_usage_panel(&state, "a", &panel_window(50.0, WEEKLY_WINDOW_MINUTES, "2025-01-01T11:00:00Z"));
    // "b": 60% headroom, reset in 100 hours -> 0.60 * 168 / 100 = 1.008, so "b" wins. On the floor "a" would
    // score 168 and win.
    observe_usage_panel(&state, "b", &panel_window(40.0, WEEKLY_WINDOW_MINUTES, "2025-01-05T16:00:00Z"));
    assert_eq!(select_now(&state, &cfg, now), "b");
  }

  #[test]
  fn the_most_headroom_wins_the_tie_even_when_read_more_recently() {
    let (_root, cfg, state) = pool_fixture(&["a", "b"]);
    let now = timestamp("2025-01-01T12:00:00Z");
    let weekly = "2025-01-02T12:00:00Z"; // 24 hours away
    // "a": 50% headroom -> 0.50 * 168 / 24 = 3.50. "b": 51% headroom -> 3.57.
    observe_usage_panel(&state, "a", &panel_window(50.0, WEEKLY_WINDOW_MINUTES, weekly));
    observe_usage_panel(&state, "b", &panel_window(49.0, WEEKLY_WINDOW_MINUTES, weekly));
    // The scores differ by 0.01 per hour, inside the 0.02 band. "b" was read more recently and still wins.
    observe_rate_limit(&state, "a", &window(0.50), Some(timestamp("2025-01-01T10:00:00Z")), None);
    observe_rate_limit(&state, "b", &window(0.49), Some(timestamp("2025-01-01T11:00:00Z")), None);
    assert_eq!(select_now(&state, &cfg, now), "b");
  }

  #[test]
  fn equal_headroom_in_the_tie_band_falls_back_to_the_least_recently_read_login() {
    let (_root, cfg, state) = pool_fixture(&["a", "b"]);
    let now = timestamp("2025-01-01T12:00:00Z");
    let weekly = "2025-01-02T12:00:00Z";
    observe_usage_panel(&state, "a", &panel_window(50.0, WEEKLY_WINDOW_MINUTES, weekly));
    observe_usage_panel(&state, "b", &panel_window(50.0, WEEKLY_WINDOW_MINUTES, weekly));
    observe_rate_limit(&state, "a", &window(0.50), Some(timestamp("2025-01-01T10:00:00Z")), None);
    observe_rate_limit(&state, "b", &window(0.50), Some(timestamp("2025-01-01T11:00:00Z")), None);
    assert_eq!(select_now(&state, &cfg, now), "a");
  }

  #[test]
  fn the_tie_band_separates_or_joins_logins_by_their_hourly_headroom() {
    let now = timestamp("2025-01-01T12:00:00Z");
    {
      // "a": 30% at 10 h -> 0.030/h. "b": 40% at 50 h -> 0.008/h. 0.022 apart, outside the band, so "a" wins.
      let (_root, cfg, state) = pool_fixture(&["a", "b"]);
      observe_usage_panel(&state, "a", &panel_window(70.0, WEEKLY_WINDOW_MINUTES, "2025-01-01T22:00:00Z"));
      observe_usage_panel(&state, "b", &panel_window(60.0, WEEKLY_WINDOW_MINUTES, "2025-01-03T14:00:00Z"));
      assert_eq!(select_now(&state, &cfg, now), "a");
    }
    {
      // "a": 30% at 10 h -> 0.030/h. "b": 40% at 25 h -> 0.016/h. 0.014 apart, inside the band, so the most
      // headroom ("b") wins.
      let (_root, cfg, state) = pool_fixture(&["a", "b"]);
      observe_usage_panel(&state, "a", &panel_window(70.0, WEEKLY_WINDOW_MINUTES, "2025-01-01T22:00:00Z"));
      observe_usage_panel(&state, "b", &panel_window(60.0, WEEKLY_WINDOW_MINUTES, "2025-01-02T13:00:00Z"));
      assert_eq!(select_now(&state, &cfg, now), "b");
    }
  }

  #[test]
  fn a_keyed_round_leaves_out_only_the_logins_with_too_little_token_life() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let now = timestamp("2025-01-01T12:00:00Z");
    let seconds = now.into_inner().unix_timestamp();
    let min = IMPROVE_TOKEN_MIN_SECONDS as i64;
    // A login's `expiresAt` is in milliseconds, as the Claude CLI writes it.
    let login = |name: &str, expires_at_ms: Option<i64>| {
      let dir = root.path().join(name);
      fs::create_dir_all(&dir).unwrap();
      let mut oauth = json!({"accessToken": "synthetic"});
      if let Some(ms) = expires_at_ms {
        oauth["expiresAt"] = json!(ms);
      }
      fs::write(dir.join(login_dirs::CREDENTIALS_FILE), json!({"claudeAiOauth": oauth}).to_string()).unwrap();
      dir
    };
    let at_minimum = login("at_minimum", Some((seconds + min) * 1000));
    let one_short = login("one_short", Some((seconds + min - 1) * 1000));
    let unnamed = login("unnamed", None);
    let nonnumeric = login("nonnumeric", None);
    let missing_credentials = root.path().join("missing_credentials");
    fs::write(
      nonnumeric.join(login_dirs::CREDENTIALS_FILE),
      json!({"claudeAiOauth": {"accessToken": "synthetic", "expiresAt": "soon"}}).to_string(),
    )
    .unwrap();
    let cfg = config(
      &home,
      &[
        ("at_minimum", &at_minimum),
        ("one_short", &one_short),
        ("unnamed", &unnamed),
        ("nonnumeric", &nonnumeric),
        ("missing_credentials", &missing_credentials),
      ],
    );

    let short = short_token_logins(&cfg, None, Some(now)).unwrap();
    assert_eq!(short, HashSet::from(["one_short".to_string(), "unnamed".to_string(), "nonnumeric".to_string()]));
    let accounts = pool_accounts(&cfg, None).unwrap();
    let unnamed_account = accounts.iter().find(|account| account.label == "unnamed").unwrap();
    let nonnumeric_account = accounts.iter().find(|account| account.label == "nonnumeric").unwrap();
    assert_eq!(token_life_suffices(unnamed_account, now), Err("expiresAt_missing"));
    assert_eq!(token_life_suffices(nonnumeric_account, now), Err("expiresAt_not_numeric"));
  }

  #[test]
  fn no_usable_login_names_the_refusal_cause_and_the_earliest_recovery() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    add_credentials(&a);
    add_credentials(&b);
    let cfg = config(&home, &[("a", &a), ("b", &b)]);
    let now = timestamp("2025-01-01T12:00:00Z");

    // Every login under its authentication-failure disable is a refusal, recovering at the earliest disable end.
    let state = ClaudeAccountState::new();
    record_auth_failure(&state, "a", Some(now));
    record_auth_failure(&state, "b", Some(timestamp("2025-01-01T12:05:00Z")));
    let answer = no_usable_login(&state, &cfg, Some("claude-fable-5-1"), Some(now), None).unwrap();
    assert_eq!(answer.failure, MemberFailure::AccessRefused);
    assert_eq!(answer.recovers_at, Some(timestamp("2025-01-01T12:15:00Z")));

    // A login without credentials recovers as soon as credentials are written, so the record expires at once.
    let missing = root.path().join("missing");
    let cfg = config(&home, &[("a", &missing)]);
    let state = ClaudeAccountState::new();
    let answer = no_usable_login(&state, &cfg, Some("claude-fable-5-1"), Some(now), None).unwrap();
    assert_eq!(answer.failure, MemberFailure::AccessRefused);
    assert_eq!(answer.recovers_at, Some(now));

    // A login out for quota keeps the quota class, and its stated reset is the earliest recovery time.
    let cfg = config(&home, &[("a", &a), ("b", &b)]);
    let state = ClaudeAccountState::new();
    let reset = now.into_inner() + Duration::minutes(5);
    observe_rate_limit(
      &state,
      "a",
      &json!({
        "unifiedWindows": {"five_hour": {"utilization": 1.0}},
        "status": "rejected",
        "resetsAt": reset.unix_timestamp() as f64,
      }),
      Some(now),
      None,
    );
    record_auth_failure(&state, "b", Some(now));
    let answer = no_usable_login(&state, &cfg, Some("claude-fable-5-1"), Some(now), None).unwrap();
    assert_eq!(answer.failure, MemberFailure::Quota);
    assert_eq!(answer.recovers_at, Some(UtcDateTime::from(reset)));

    // A login both out for quota and under its disable is still a quota refusal.
    // It recovers when the later of the two has ended.
    // Here the reset, 30 minutes out, outlasts the 15-minute disable.
    let cfg = config(&home, &[("a", &a)]);
    let state = ClaudeAccountState::new();
    let reset = now.into_inner() + Duration::minutes(30);
    observe_rate_limit(
      &state,
      "a",
      &json!({
        "unifiedWindows": {"five_hour": {"utilization": 1.0}},
        "status": "rejected",
        "resetsAt": reset.unix_timestamp() as f64,
      }),
      Some(now),
      None,
    );
    record_auth_failure(&state, "a", Some(now));
    let answer = no_usable_login(&state, &cfg, Some("claude-fable-5-1"), Some(now), None).unwrap();
    assert_eq!(answer.failure, MemberFailure::Quota);
    assert_eq!(answer.recovers_at, Some(UtcDateTime::from(reset)));
  }

  fn unreserved_candidate(label: &str, unreserved: f64) -> UnreservedCandidate {
    UnreservedCandidate {
      account: claude_config::ClaudeAccount { label: label.to_string(), config_dir: "/synthetic".to_string() },
      unreserved,
    }
  }

  #[test]
  fn the_unreserved_tiers_take_the_under_line_first_then_positive_then_overflow() {
    let (outcome, contenders) =
      unreserved_tier(vec![unreserved_candidate("a", 0.37), unreserved_candidate("b", 0.05)]).unwrap();
    assert_eq!(outcome, "redirect");
    assert_eq!(contenders.iter().map(|(account, _)| account.label.as_str()).collect::<Vec<_>>(), ["a"]);

    let (outcome, contenders) =
      unreserved_tier(vec![unreserved_candidate("a", 0.08), unreserved_candidate("b", 0.05)]).unwrap();
    assert_eq!(outcome, "redirect");
    assert_eq!(contenders.iter().map(|(account, _)| account.label.as_str()).collect::<Vec<_>>(), ["a", "b"]);

    // The overflow takes every candidate and never refuses.
    let (outcome, contenders) = unreserved_tier(vec![unreserved_candidate("a", -0.02)]).unwrap();
    assert_eq!(outcome, "overflow");
    assert_eq!(contenders.iter().map(|(account, _)| account.label.as_str()).collect::<Vec<_>>(), ["a"]);
  }

  #[test]
  fn a_reset_login_absorbs_a_herd_in_tiers_and_overflows_instead_of_refusing() {
    let (_root, cfg, state) = pool_fixture(&["r", "x1", "x2", "x3", "x4", "x5", "x6"]);
    let now = timestamp("2025-01-01T12:00:00Z");
    let rejected =
      json!({"unifiedWindows":{"five_hour":{"utilization":1.0}},"status":"rejected","resetsAt":4102462800.0});
    for label in ["x1", "x2", "x3", "x4", "x5", "x6"] {
      observe_rate_limit(&state, label, &rejected, Some(now), None);
    }
    // "r" just reset: five-hour 0%, seven-day 63% -> raw headroom 0.37, five-hour headroom 1.0.
    observe_rate_limit(
      &state,
      "r",
      &json!({"unifiedWindows":{"five_hour":{"utilization":0.0},"seven_day":{"utilization":0.63}}}),
      Some(now),
      None,
    );

    // Placement 18 leaves unreserved at the 0.10 line, 20 at zero. The rest overflow. None refuses.
    let mut counts = HashMap::new();
    let mut live = Vec::new();
    for index in 0..24 {
      let pick = reserve_pick(&state, &cfg, None, &HashSet::new(), None, Some(now), None, &format!("run-{index}"))
        .unwrap()
        .expect("a reservation never refuses a run while a login has headroom");
      *counts.entry(pick.account.label.clone()).or_insert(0) += 1;
      live.push(pick.handle);
    }
    assert_eq!(counts, HashMap::from([("r".to_string(), 24)]));
  }

  #[test]
  fn reservations_spread_a_reset_across_the_logins_instead_of_piling_one() {
    let (_root, cfg, state) = pool_fixture(&["a", "b", "c"]);
    let now = timestamp("2025-01-01T12:00:00Z");
    // "a" just reset: five-hour 0%, seven-day 10%. "b" and "c" sit at 50% on both windows. All share the same
    // hours-to-weekly-reset, so the score compares unreserved headroom alone.
    let both = |five_hour: f64, seven_day: f64| json!({"unifiedWindows":{"five_hour":{"utilization":five_hour},"seven_day":{"utilization":seven_day}}});
    observe_rate_limit(&state, "a", &both(0.0, 0.10), Some(now), None);
    observe_rate_limit(&state, "b", &both(0.5, 0.5), Some(now), None);
    observe_rate_limit(&state, "c", &both(0.5, 0.5), Some(now), None);

    let mut counts = HashMap::new();
    let mut live = Vec::new();
    for index in 0..24 {
      let pick = reserve_pick(&state, &cfg, None, &HashSet::new(), None, Some(now), None, &format!("run-{index}"))
        .unwrap()
        .expect("a reservation never refuses a run while a login has headroom");
      *counts.entry(pick.account.label.clone()).or_insert(0) += 1;
      live.push(pick.handle);
    }
    // "a" takes the herd until its unreserved headroom sinks under "b" and "c"'s, then the three share it. Today's
    // rule would put all 24 on "a".
    assert!(counts["a"] >= 14 && counts["a"] <= 15, "the reset login still takes the bulk: {counts:?}");
    assert!(counts["a"] <= 15, "the reservations cap the reset login's herd: {counts:?}");
    assert!(counts["b"] >= 4 && counts["b"] <= 5, "the second login takes the overflow: {counts:?}");
    assert!(counts["c"] >= 4 && counts["c"] <= 5, "the third login takes the overflow: {counts:?}");
  }

  #[test]
  fn two_launches_see_each_others_reservations() {
    let (_root, cfg, state) = pool_fixture(&["a", "b"]);
    let now = timestamp("2025-01-01T12:00:00Z");
    // "a" has 8 points of five-hour headroom left, "b" 5: both under the line, "a" ahead on the score.
    observe_rate_limit(&state, "a", &window(0.92), Some(now), None);
    observe_rate_limit(&state, "b", &window(0.95), Some(now), None);

    let first = reserve_pick(&state, &cfg, None, &HashSet::new(), None, Some(now), None, "run-1").unwrap().unwrap();
    assert_eq!(first.account.label, "a");
    let second = reserve_pick(&state, &cfg, None, &HashSet::new(), None, Some(now), None, "run-2").unwrap().unwrap();
    assert_eq!(second.account.label, "b", "the second launch sees the first run's reservation");
  }

  #[test]
  fn a_run_counts_once_on_its_newest_handles_login() {
    let (_root, _cfg, state) = pool_fixture(&["a", "b"]);
    let now = timestamp("2025-01-01T12:00:00Z");
    let first = reserve_on(&state, "run-1", "a", "reattach", Some(now));
    assert_eq!(state.reservation_counts(now), HashMap::from([("a".to_string(), 1)]));
    // The same run placed on "b" moves its one count there.
    let second = reserve_on(&state, "run-1", "b", "redirect", Some(now));
    assert_eq!(state.reservation_counts(now), HashMap::from([("b".to_string(), 1)]));
    drop((first, second));
  }

  #[test]
  fn a_reservation_counts_no_longer_after_thirty_minutes() {
    let (_root, _cfg, state) = pool_fixture(&["a"]);
    let now = timestamp("2025-01-01T12:00:00Z");
    let handle = reserve_on(&state, "run-1", "a", "reattach", Some(now));
    assert_eq!(state.reservation_counts(add_duration(now, Duration::minutes(29))).len(), 1);
    assert!(state.reservation_counts(add_duration(now, Duration::minutes(30))).is_empty());
    drop(handle);
  }

  #[test]
  fn the_member_reading_says_when_every_headroom_login_is_fully_reserved() {
    let (_root, cfg, state) = pool_fixture(&["a"]);
    let option = cfg.get_backend_option("fable").expect("the fable option").clone();
    let now = timestamp("2025-01-01T12:00:00Z");
    observe_rate_limit(&state, "a", &window(0.20), Some(now), None);

    let reading = member_reading(&state, &cfg, &option, now).unwrap().expect("the login reads");
    assert!(!reading.fully_reserved, "headroom with no reservations is not fully reserved");

    // Twenty runs reserve the login's whole five-hour window while its headroom stays untouched.
    let handles =
      (0..20).map(|index| reserve_on(&state, &format!("run-{index}"), "a", "redirect", Some(now))).collect::<Vec<_>>();
    let reading = member_reading(&state, &cfg, &option, now).unwrap().expect("the login reads");
    assert!(reading.fully_reserved);
    assert!(!reading.exhausted, "exhausted keeps its raw meaning");
    drop(handles);

    // A rejected login is exhausted, not fully reserved.
    let rejected =
      json!({"unifiedWindows":{"five_hour":{"utilization":1.0}},"status":"rejected","resetsAt":4102462800.0});
    observe_rate_limit(&state, "a", &rejected, Some(now), None);
    let reading = member_reading(&state, &cfg, &option, now).unwrap().expect("the login reads");
    assert!(reading.exhausted);
    assert!(!reading.fully_reserved);
  }

  #[test]
  fn a_login_without_a_five_hour_reading_reserves_against_its_headroom() {
    let (_root, cfg, state) = pool_fixture(&["a"]);
    let now = timestamp("2025-01-01T12:00:00Z");
    // A seven-day-only panel reading: headroom 0.40, no five-hour value.
    observe_usage_panel(&state, "a", &panel_window(60.0, WEEKLY_WINDOW_MINUTES, "2025-01-08T12:00:00Z"));

    // Eight runs reserve the whole 0.40 headroom. The login still takes the run, as the overflow.
    let handles =
      (0..8).map(|index| reserve_on(&state, &format!("run-{index}"), "a", "redirect", Some(now))).collect::<Vec<_>>();
    let pick = reserve_pick(&state, &cfg, None, &HashSet::new(), None, Some(now), None, "run-8").unwrap().unwrap();
    assert_eq!(pick.account.label, "a");
    let reading =
      member_reading(&state, &cfg, cfg.get_backend_option("fable").unwrap(), now).unwrap().expect("the login reads");
    assert!(reading.fully_reserved);
    drop((handles, pick));
  }

  #[test]
  fn the_five_hour_reading_follows_its_observation() {
    let (_root, _cfg, state) = pool_fixture(&["a"]);
    let now = timestamp("2025-01-01T12:00:00Z");
    observe_rate_limit(
      &state,
      "a",
      &json!({"unifiedWindows":{"five_hour":{"utilization":0.20},"seven_day":{"utilization":0.60}}}),
      Some(now),
      None,
    );
    assert_eq!(latest_reading(&state, "a", None, Some(now)).unwrap().five_hour, Some(0.20));

    // A windowless rejection reads no five-hour value.
    let rejected = json!({"unifiedWindows":{},"status":"rejected","resetsAt":4102462800.0});
    observe_rate_limit(&state, "a", &rejected, Some(now), None);
    assert_eq!(latest_reading(&state, "a", None, Some(now)).unwrap().five_hour, None);

    // The panel's 300-minute window supplies it, and the newest reading wins whole.
    observe_usage_panel(
      &state,
      "a",
      &json!({"fetched_at":"2025-01-01T12:01:00Z","windows":[{"utilization":45.0,"window_minutes":300}]}),
    );
    let reading = latest_reading(&state, "a", None, Some(now)).unwrap();
    assert_eq!(reading.utilization, 0.45);
    assert_eq!(reading.five_hour, Some(0.45));
  }

  #[test]
  #[ignore = "integration"]
  fn a_reattach_holds_the_login_the_process_environ_names() {
    let (root, cfg, state) = pool_fixture(&["a", "b"]);
    let now = timestamp("2025-01-01T12:00:00Z");

    let mut held = std::process::Command::new("/bin/sleep")
      .arg("30")
      .env("CLAUDE_CONFIG_DIR", root.path().join("a"))
      .spawn()
      .unwrap();
    // Between fork and exec the child's /proc environ still shows the test runner's environment, so the
    // variable appears only after exec. Poll instead of racing the window.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let handle = loop {
      match reattach_hold(&state, &cfg, held.id(), "run-1", Some(now)).unwrap() {
        Some(handle) => break handle,
        None => {
          assert!(std::time::Instant::now() < deadline, "the environ names a pool login");
          std::thread::sleep(std::time::Duration::from_millis(10));
        }
      }
    };
    assert_eq!(state.reservation_counts(now), HashMap::from([("a".to_string(), 1)]));
    drop(handle);
    assert!(state.reservation_counts(now).is_empty(), "the hold ends with its handle");

    let mut plain = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(reattach_hold(&state, &cfg, plain.id(), "run-2", Some(now)).unwrap().is_none());
    assert!(state.reservation_counts(now).is_empty());

    held.kill().unwrap();
    plain.kill().unwrap();
    held.wait().unwrap();
    plain.wait().unwrap();
  }
}
