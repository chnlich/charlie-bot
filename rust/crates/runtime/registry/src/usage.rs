//! Usage sources: where each backend's token usage is logged. A backend package registers one source and attributes
//! its backend types to it. A source declares the operations it defines in [`UsageOps`], and the usage package calls
//! only those that the source defines.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::bail;
use charliebot_core::config::CharlieBotConfig;
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::member::{MemberReading, MemberWindow};

/// Keys of the quota panel payload and of one window in it (`usage_sources.PANEL_*`). The producers, the usage page
/// and the browser share them.
pub const PANEL_WINDOWS: &str = "windows";
pub const PANEL_FETCHED_AT: &str = "fetched_at";
pub const PANEL_PROVIDER: &str = "provider";
pub const PANEL_PROVIDER_LABEL: &str = "provider_label";
/// The plan an account is on, as the provider's event names it. A row shows this string verbatim.
pub const PANEL_PLAN_TYPE: &str = "plan_type";
pub const PANEL_WINDOW_MINUTES: &str = "window_minutes";
pub const PANEL_UTILIZATION: &str = "utilization";
pub const PANEL_RESETS_AT: &str = "resets_at";
pub const PANEL_SCOPE_LABEL: &str = "scope_label";
pub const PANEL_EXPIRED: &str = "expired";
pub const PANEL_WINDOW_NAME: &str = "window_name";
pub const PANEL_STATUS_ROWS: &str = "status_rows";

/// The label a quota source gives the account that its config does not name. The producer's label and its provider
/// form the `<provider>:<label>` cache key of one account.
pub const DEFAULT_ACCOUNT_LABEL: &str = "main";

/// The utilization at and above which a quota account or Auto member is at the warning line.
pub const WARNING_UTILIZATION: f64 = 0.90;

/// A member is near exhaustion when a window at or above the 90% line resets more than this far ahead, or has no stated
/// reset.
pub const NEAR_EXHAUSTION_RESET_HORIZON: time::Duration = time::Duration::minutes(45);

/// The length of the weekly quota window, in minutes: the seven-day window each backend's quota panel states with
/// `PANEL_WINDOW_MINUTES`.
pub const WEEKLY_WINDOW_MINUTES: i64 = 10_080;

/// The reading-limits kind a backend registers when a session's context follows Claude Code's event stream. The
/// usage fold classifies such a session under this kind and looks its limits up.
pub const CLAUDE_READING_KIND: &str = "claude";
/// The reading-limits kind a backend registers when a session's context comes from snapshot events: the usage fold
/// classifies such a session under this kind and looks its limits up.
pub const SNAPSHOT_READING_KIND: &str = "snapshot";

/// The log event an external usage reader emits once per limit window it cannot classify. Every reader's alert
/// matches this one name.
pub const UNKNOWN_LIMIT_SHAPE_EVENT: &str = "ext_usage_unknown_limit_shape";

/// The log event a cold-storage sweep source emits when it cannot list one of its directories.
pub const SWEEP_DIR_SCAN_FAILED_EVENT: &str = "storage_cool_dir_scan_failed";
/// The log event a sweep source emits when it cannot read one file's metadata.
pub const SWEEP_FILE_STAT_FAILED_EVENT: &str = "storage_cool_file_stat_failed";
/// The log event a sweep source emits when it cannot remove one file.
pub const SWEEP_FILE_DELETE_FAILED_EVENT: &str = "storage_cool_file_delete_failed";

/// The cold-storage sweep category a source reports for Claude Code's transcript directories. The usage package looks
/// the category up by this name when it logs the sweep.
pub const CLAUDE_TRANSCRIPTS_CATEGORY: &str = "claude-transcripts";
/// The cold-storage sweep category a source reports for Codex's rollout files.
pub const CODEX_ROLLOUTS_CATEGORY: &str = "codex-rollouts";

/// The utilization a source reports for a window: a percentage, or `None` when the value is absent or not a number.
pub fn as_utilization(value: Option<&Value>) -> Option<f64> {
  match value {
    Some(Value::Number(number)) => number.as_f64(),
    _ => None,
  }
}

/// Whether one stored panel window is expired at `now` (`usage_sources.panel_window_expired`). It is expired when its
/// reset passed while the reading's sample time predates it, or when the sample is older than the window is long.
pub fn panel_window_expired(window: &Value, sampled: UtcDateTime, now: UtcDateTime) -> bool {
  let resets_at = window.get(PANEL_RESETS_AT).and_then(Value::as_str).and_then(|value| UtcDateTime::parse(value).ok());
  if resets_at.is_some_and(|reset| reset <= now && sampled < reset) {
    return true;
  }
  window.get(PANEL_WINDOW_MINUTES).and_then(Value::as_i64).is_some_and(|minutes| {
    let elapsed = now.into_inner() - sampled.into_inner();
    minutes < 0 || elapsed > std::time::Duration::from_secs(minutes as u64 * 60)
  })
}

/// The entry as one emit shows it at `now` (`usage_sources.mark_expired_windows`): a copy whose expired windows carry
/// `expired: true`. An entry without a sample time, or without a window list, comes back as it is.
pub fn mark_expired_windows(
  entry: &Map<String, Value>,
  sampled: Option<UtcDateTime>,
  now: UtcDateTime,
) -> Map<String, Value> {
  let (Some(windows), Some(sampled)) = (entry.get(PANEL_WINDOWS).and_then(Value::as_array), sampled) else {
    return entry.clone();
  };
  let annotated = windows
    .iter()
    .map(|window| match window.as_object() {
      Some(object) if panel_window_expired(window, sampled, now) => {
        let mut marked = object.clone();
        marked.insert(PANEL_EXPIRED.to_string(), Value::Bool(true));
        Value::Object(marked)
      }
      _ => window.clone(),
    })
    .collect();
  let mut marked = entry.clone();
  marked.insert(PANEL_WINDOWS.to_string(), Value::Array(annotated));
  marked
}

/// Derive the Auto member reading from the same windows the usage panel displays.
/// Windows outside the sample's live period do not participate in the reading.
pub fn member_reading_from_windows(
  windows: &[Value],
  sampled: UtcDateTime,
  now: UtcDateTime,
) -> anyhow::Result<Option<MemberReading>> {
  let now_inner = now.into_inner();
  let near_horizon = now_inner + NEAR_EXHAUSTION_RESET_HORIZON;
  let mut member_windows = Vec::new();
  for window in windows.iter().filter(|window| !panel_window_expired(window, sampled, now)) {
    let object = window.as_object().ok_or_else(|| anyhow::anyhow!("a quota panel window is not an object"))?;
    let utilization = object
      .get(PANEL_UTILIZATION)
      .and_then(Value::as_f64)
      .ok_or_else(|| anyhow::anyhow!("a quota panel window has no numeric utilization"))?
      / 100.0;
    let window_minutes = object.get(PANEL_WINDOW_MINUTES).and_then(Value::as_i64);
    let resets_at = object
      .get(PANEL_RESETS_AT)
      .and_then(Value::as_str)
      .filter(|reset| !reset.is_empty())
      .map(UtcDateTime::parse)
      .transpose()?;
    member_windows.push(MemberWindow { utilization, window_minutes, resets_at });
  }
  if member_windows.is_empty() {
    return Ok(None);
  }

  let weekly_resets_at = member_windows
    .iter()
    .find(|window| window.window_minutes == Some(WEEKLY_WINDOW_MINUTES))
    .and_then(|window| window.resets_at);
  let exhausted = member_windows
    .iter()
    .any(|window| window.utilization >= 1.0 && window.resets_at.is_none_or(|reset| reset.into_inner() >= now_inner));
  let near_exhaustion = member_windows.iter().any(|window| {
    window.utilization >= WARNING_UTILIZATION && window.resets_at.is_none_or(|reset| reset.into_inner() > near_horizon)
  });

  Ok(Some(MemberReading { windows: member_windows, weekly_resets_at, near_exhaustion, exhausted }))
}

/// One place usage is logged. `name` is the ledger's source value and the usage page's card title. A config backend
/// id must start with one of `id_prefixes`. `run_logs_only` means CharlieBot's own run logs are the only source of
/// token usage; the source may still implement quota-panel reads.
pub struct UsageSource {
  pub name: &'static str,
  pub id_prefixes: &'static [&'static str],
  pub run_logs_only: bool,
  pub implementation: Option<Arc<dyn UsageSourceImpl>>,
}

/// The operations a source defines. Callers check a declaration before they call the operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageOps {
  pub logs: bool,
  pub read: bool,
  pub live_cli_sessions: bool,
  pub quota_accounts: bool,
  pub sweep: bool,
}

/// A source's implementation. [`UsageSourceImpl::ops`] declares the operations it defines; an undeclared operation
/// is a caller bug and panics.
pub trait UsageSourceImpl: Send + Sync {
  fn ops(&self) -> UsageOps;

  /// The source's log files on this host, each with its account label, read from the loaded config.
  fn logs(&self, _cfg: &CharlieBotConfig) -> anyhow::Result<Vec<(PathBuf, String)>> {
    undeclared("logs")
  }

  /// One log file's records and its new signature. `previous` is the signature the ledger stores for the file.
  fn read(&self, _path: &Path, _account: &str, _previous: Option<&str>) -> anyhow::Result<(String, Vec<UsageRecord>)> {
    undeclared("read")
  }

  /// The CLI session ids whose own logs still exist. A source that declares it is read under the Codex rule.
  fn live_cli_sessions(&self) -> anyhow::Result<HashSet<String>> {
    undeclared("live_cli_sessions")
  }

  /// The source's accounts on the quota panel, derived from the live config on every call. An account that stays in
  /// the config comes back as the same `Arc`, so its state (a 429 backoff, for one) survives between calls.
  fn quota_accounts(&self, _cfg: &CharlieBotConfig) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> {
    undeclared("quota_accounts")
  }

  /// The source's part of the cold-storage sweep. It runs after the ledger capture.
  fn sweep(&self, _scope: &SweepScope) -> anyhow::Result<SourceSweep> {
    undeclared("sweep")
  }
}

fn undeclared(operation: &str) -> ! {
  panic!("the usage source does not declare {operation}; check ops() before calling it")
}

/// How a usage row was learned: from a CLI-kept log (`Native`), or from a CharlieBot capture whose CLI log may be
/// pruned (`Fallback`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
  Native,
  Fallback,
}

/// One API call's usage as extracted from a captured file. `ts` is ISO 8601 UTC. `sessions` names the CLI sessions
/// the call's log belongs to; a fallback record must carry at least one, because exclusion is keyed on them.
/// `in_unsplit` is input whose cache hit and miss split was never logged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageRecord {
  pub record_id: String,
  pub kind: RecordKind,
  pub source: String,
  pub model: String,
  pub account: String,
  pub ts: String,
  pub in_fresh: i64,
  pub cache_write: i64,
  pub cache_read: i64,
  pub output: i64,
  pub in_unsplit: i64,
  pub sessions: Vec<String>,
}

impl UsageRecord {
  /// The record, or an error when a fallback record carries no session.
  pub fn checked(self) -> anyhow::Result<UsageRecord> {
    if self.kind == RecordKind::Fallback && self.sessions.is_empty() {
      bail!("fallback record {:?} carries no sessions", self.record_id);
    }
    Ok(self)
  }
}

/// One account of a source on the quota panel, kept by the source between rounds (`QuotaAccount`). `provider` and
/// `label` form the cache key `<provider>:<label>`. The poller awaits one `fetch` at a time.
pub trait QuotaAccount: Send + Sync {
  fn provider(&self) -> &str;
  fn provider_label(&self) -> &str;
  fn label(&self) -> &str;
  /// Why the last `fetch` returned `Ok(None)`.
  fn last_error(&self) -> String;
  fn fetch<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Option<Map<String, Value>>>>;
  /// The `login_required` flag on the cached entry, set or cleared after each fetch of this account.
  fn mark_login_required(&self, _entry: &mut Map<String, Value>) {}
  /// The entry as one emit shows it at `now`. Never writes into `entry`.
  fn mark_expired(&self, entry: &Map<String, Value>, _now: UtcDateTime) -> anyhow::Result<Map<String, Value>> {
    Ok(entry.clone())
  }
}

/// One output category of the sweep: what it freed, or what a dry run would free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CategoryResult {
  pub name: &'static str,
  pub unit: &'static str,
  pub count: u64,
  pub bytes: u64,
}

/// The free pages a source's database holds, which a manual vacuum hands back. `name` labels the report line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreelistResult {
  pub name: &'static str,
  pub bytes: u64,
}

/// What one source's `sweep` reports: its categories in report order, and its database's free pages, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSweep {
  pub categories: Vec<CategoryResult>,
  pub freelist: Option<FreelistResult>,
}

/// The metadata fields of one session that the sweep judges on. `cold` is the cold rule's verdict, and
/// `cc_session_id` is the backend session id the session's metadata references.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionFacts {
  pub id: String,
  pub cold: bool,
  pub cc_session_id: Option<String>,
}

/// Everything a source's `sweep` reads. `facts` maps a session id to its facts; `references` maps each backend session
/// id that metadata references to the sessions that reference it. A record goes under one rule: every referencing
/// session is cold, or nothing references it and its own idle clock is past `orphan_idle_days`.
pub struct SweepScope {
  pub cfg: Arc<CharlieBotConfig>,
  pub now: UtcDateTime,
  pub dry_run: bool,
  /// The scoped session, or `None` for a whole-host run.
  pub session_id: Option<String>,
  pub facts: HashMap<String, SessionFacts>,
  pub references: HashMap<String, Vec<SessionFacts>>,
  pub orphan_idle_days: i64,
  pub vacuum: bool,
  pub force: bool,
}

impl SweepScope {
  /// The referenced half of the rule: metadata references the record and every referencing session is cold.
  pub fn referenced_and_cold(&self, backend_session: &str) -> bool {
    self
      .references
      .get(backend_session)
      .is_some_and(|owners| !owners.is_empty() && owners.iter().all(|owner| owner.cold))
  }

  /// The orphan half of the rule: the record's own idle clock has passed `orphan_idle_days`.
  pub fn idle_past(&self, idle_since_epoch: f64) -> bool {
    let now = self.now.into_inner().unix_timestamp_nanos() as f64 / 1e9;
    now - idle_since_epoch >= (self.orphan_idle_days * 86400) as f64
  }

  /// Every backend session id the scoped session references. Empty for a whole-host run.
  pub fn scoped_backend_sessions(&self) -> HashSet<String> {
    let Some(session_id) = self.session_id.as_deref() else {
      return HashSet::new();
    };
    let mut backend_sessions: HashSet<String> =
      self.facts.get(session_id).and_then(|owner| owner.cc_session_id.clone()).into_iter().collect();
    for (backend_session, owners) in &self.references {
      if owners.iter().any(|reference| reference.id == session_id) {
        backend_sessions.insert(backend_session.clone());
      }
    }
    backend_sessions
  }
}

#[cfg(test)]
mod member_reading_tests {
  use serde_json::json;

  use super::*;

  fn time_at(offset_seconds: i64) -> UtcDateTime {
    let now = UtcDateTime::parse("2026-11-07T08:19:00Z").unwrap().into_inner();
    UtcDateTime::from(now + time::Duration::seconds(offset_seconds))
  }

  fn window(utilization: f64, reset_seconds: Option<i64>, minutes: Option<i64>) -> Value {
    let mut window = Map::from_iter([(PANEL_UTILIZATION.to_string(), json!(utilization))]);
    if let Some(minutes) = minutes {
      window.insert(PANEL_WINDOW_MINUTES.to_string(), json!(minutes));
    }
    if let Some(reset_seconds) = reset_seconds {
      window.insert(PANEL_RESETS_AT.to_string(), Value::String(time_at(reset_seconds).to_str()));
    }
    Value::Object(window)
  }

  #[test]
  fn the_weekly_reset_is_read_from_the_seven_day_window() {
    let now = time_at(0);
    let reading = member_reading_from_windows(
      &[window(10.0, Some(20 * 60), Some(300)), window(97.0, Some(14 * 24 * 60 * 60), Some(WEEKLY_WINDOW_MINUTES))],
      now,
      now,
    )
    .unwrap()
    .unwrap();

    assert!(reading.near_exhaustion);
    assert!(!reading.exhausted);
    assert_eq!(reading.weekly_resets_at, Some(time_at(14 * 24 * 60 * 60)));
  }

  #[test]
  fn a_monthly_window_uses_its_own_reset_for_near_exhaustion() {
    let now = time_at(0);
    let reading = member_reading_from_windows(
      &[window(10.0, Some(20 * 60), Some(300)), window(97.0, Some(14 * 24 * 60 * 60), None)],
      now,
      now,
    )
    .unwrap()
    .unwrap();

    assert!(reading.near_exhaustion);
  }

  #[test]
  fn a_window_at_one_hundred_percent_with_a_future_reset_is_exhausted() {
    let now = time_at(0);
    let reading =
      member_reading_from_windows(&[window(100.0, Some(2 * 24 * 60 * 60), Some(300))], now, now).unwrap().unwrap();

    assert!(reading.exhausted);
  }

  #[test]
  fn a_window_that_resets_within_forty_five_minutes_is_not_near_exhaustion() {
    let now = time_at(0);
    let reading = member_reading_from_windows(&[window(92.0, Some(20 * 60), Some(300))], now, now).unwrap().unwrap();

    assert!(!reading.near_exhaustion);
  }

  #[test]
  fn a_fully_used_window_whose_reset_passed_is_not_exhausted() {
    let sampled = time_at(0);
    let now = time_at(60);
    let reading = member_reading_from_windows(&[window(100.0, Some(-300), Some(300))], sampled, now).unwrap().unwrap();

    assert!(!reading.exhausted);
  }

  #[test]
  fn a_window_at_the_warning_line_without_a_reset_is_near_exhaustion() {
    let now = time_at(0);
    let reading = member_reading_from_windows(&[window(92.0, None, None)], now, now).unwrap().unwrap();

    assert!(reading.near_exhaustion);
  }

  #[test]
  fn expired_windows_do_not_produce_a_member_reading() {
    let now = time_at(0);
    let sampled = time_at(-120);

    assert_eq!(member_reading_from_windows(&[window(100.0, Some(-60), Some(300))], sampled, now).unwrap(), None);
  }
}
