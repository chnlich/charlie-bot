//! Usage sources (`src/runtime/hooks/usage_source_registration.py` and `usage_sources.py`): where each backend's token
//! usage is logged. A backend package registers one source and attributes its backend types to it. A source declares
//! the operations it defines in [`UsageOps`], and the usage package calls only those, as Python's `hasattr` checks do.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::bail;
use charliebot_core::config::CharlieBotConfig;
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

/// Keys of the quota panel payload and of one window in it (`usage_sources.PANEL_*`). The producers, the usage page
/// and the browser share them.
pub const PANEL_WINDOWS: &str = "windows";
pub const PANEL_FETCHED_AT: &str = "fetched_at";
pub const PANEL_PROVIDER: &str = "provider";
pub const PANEL_PROVIDER_LABEL: &str = "provider_label";
pub const PANEL_STATE_LABEL: &str = "state_label";
pub const PANEL_WINDOW_MINUTES: &str = "window_minutes";
pub const PANEL_UTILIZATION: &str = "utilization";
pub const PANEL_RESETS_AT: &str = "resets_at";
pub const PANEL_SCOPE_LABEL: &str = "scope_label";
pub const PANEL_EXPIRED: &str = "expired";

/// One place usage is logged. `name` is the ledger's source value and the usage page's card title. A config backend
/// id must start with one of `id_prefixes`. `run_logs_only` means CharlieBot's own run logs are the only home, and
/// such a source has no implementation.
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
  /// Sets or clears `login_required` on the cached entry, after each fetch of this account.
  fn mark_login_required(&self, _entry: &mut Map<String, Value>) {}
  /// The entry as one emit shows it at `now`. Never writes into `entry`.
  fn mark_expired(&self, entry: &Map<String, Value>, _now: UtcDateTime) -> Map<String, Value> {
    entry.clone()
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
