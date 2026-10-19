//! Process-local backend rotation and quota cooldowns for unpinned work.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::context::FeatureContext;
use charliebot_registry::{Registry, RegistryError};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use time::{Duration, OffsetDateTime, UtcOffset};

use charliebot_store::metadata::UtcDateTime;

/// Error substrings that mark a backend as out of quota.
pub const QUOTA_ERROR_PATTERNS: &[&str] = &[
  "quota exhausted",
  "quota exceeded",
  "insufficient quota",
  "over quota",
  "rate limit exceeded",
  "rate limit reached",
  "rate limit rejected",
  "rate-limit exceeded",
  "rate-limit reached",
  "rate-limit rejected",
  "rate_limited",
  "rate_limit_error",
  "rate limited",
  "too many requests",
  "resource_exhausted",
  "429",
  "out of token",
  "out-of-token",
  "insufficient tokens",
  "tokens exhausted",
  "quota",
  "rate limit",
  "usage limit",
  "budget has been exceeded",
];

/// The cooldown for a quota error that names no reset time.
pub const DEFAULT_COOLDOWN: Duration = Duration::minutes(30);

#[derive(Default)]
struct PoolState {
  position: usize,
  cooling_until: HashMap<String, OffsetDateTime>,
}

/// The shared rotation position and cooldown map for every unpinned backend caller.
#[derive(Default)]
pub struct BackendPool {
  state: Mutex<PoolState>,
}

impl BackendPool {
  /// Select the next configured pool member that is neither cooling nor excluded.
  pub fn select(
    &self,
    cfg: &CharlieBotConfig,
    caller: &str,
    exclude: &[String],
  ) -> anyhow::Result<Option<BackendOption>> {
    let now = UtcDateTime::now().into_inner();
    let members = &cfg.backends.pool;
    let mut state = self.state.lock().expect("backend pool mutex is not poisoned");
    for offset in 0..members.len() {
      let index = (state.position + offset) % members.len();
      let member = &members[index];
      if exclude.iter().any(|excluded| excluded == member) {
        tracing::info!(target: "charliebot_agents", caller, member, reason = "excluded by caller", "backend_pool_skipped");
        continue;
      }
      if let Some(until) = state.cooling_until.get(member)
        && *until > now
      {
        tracing::info!(
          target: "charliebot_agents",
          caller,
          member,
          reason = %format!("cooling until {}", format_minutes(*until)),
          "backend_pool_skipped"
        );
        continue;
      }
      state.position = (index + 1) % members.len();
      tracing::info!(target: "charliebot_agents", caller, member, position = index, "backend_pool_selected");
      let option = cfg
        .get_backend_option(member)
        .with_context(|| format!("backends.pool entry backend '{member}' is not in backends.options"))?;
      return Ok(Some(option.clone()));
    }
    let mut excluded = exclude.to_vec();
    excluded.sort();
    tracing::warn!(target: "charliebot_agents", caller, members = ?members, excluded = ?excluded, "backend_pool_exhausted");
    Ok(None)
  }

  /// Keep `backend_id` out of selection until its reset time, or the default cooldown.
  /// Return the instant when the member becomes selectable again.
  pub fn cool(&self, backend_id: &str, resets_at: Option<UtcDateTime>) -> OffsetDateTime {
    let until =
      resets_at.map(UtcDateTime::into_inner).unwrap_or_else(|| UtcDateTime::now().into_inner() + DEFAULT_COOLDOWN);
    self.state.lock().expect("backend pool mutex is not poisoned").cooling_until.insert(backend_id.to_string(), until);
    tracing::warn!(
      target: "charliebot_agents",
      member = backend_id,
      until = %format_minutes(until),
      reset_known = resets_at.is_some(),
      "backend_pool_cooled"
    );
    until
  }

  /// Describe every configured member and whether its cooldown has ended.
  pub fn recovery_report(&self, cfg: &CharlieBotConfig) -> String {
    let now = UtcDateTime::now().into_inner();
    let state = self.state.lock().expect("backend pool mutex is not poisoned");
    let parts = cfg
      .backends
      .pool
      .iter()
      .map(|member| match state.cooling_until.get(member) {
        Some(until) if *until > now => format!("{member} cooling until {}", format_minutes(*until)),
        _ => format!("{member} not cooling"),
      })
      .collect::<Vec<_>>();
    if parts.is_empty() { "backends.pool is empty".to_string() } else { parts.join("; ") }
  }
}

/// Return whether an error string contains a known quota error substring.
pub fn is_quota_error(text: &str) -> bool {
  let lowered = text.to_lowercase();
  QUOTA_ERROR_PATTERNS.iter().any(|pattern| lowered.contains(pattern))
}

/// Read the process-wide pool inserted by the startup step.
pub fn backend_pool(features: &FeatureContext) -> anyhow::Result<Arc<BackendPool>> {
  Ok(features.startup.require::<BackendPool>()?)
}

/// Insert the empty process-wide pool before recovery starts.
pub fn pre_recovery_step(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    features.startup.insert(BackendPool::default())?;
    Ok(())
  })
}

/// Register the session metadata slot owned by `src.features.backend_pool`.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: "backend_pool",
      on: MetadataFile::Session,
      after: None,
      fields: vec![SlotField { name: "backend_from_pool", default: serde_json::json!(false) }],
    },
    validate_backend_pool_fields,
  )
}

fn validate_backend_pool_fields(values: &mut Map<String, Value>) -> Option<String> {
  let value = values.get_mut("backend_from_pool")?;
  let boolean = match value {
    Value::Bool(value) => *value,
    Value::Number(value) if value.as_i64() == Some(0) || value.as_f64() == Some(0.0) => false,
    Value::Number(value) if value.as_i64() == Some(1) || value.as_f64() == Some(1.0) => true,
    Value::String(value) => match value.to_ascii_lowercase().as_str() {
      "1" | "true" | "t" | "yes" | "y" | "on" => true,
      "0" | "false" | "f" | "no" | "n" | "off" => false,
      _ => return Some("backend_from_pool must be a boolean".to_string()),
    },
    _ => return Some("backend_from_pool must be a boolean".to_string()),
  };
  *value = Value::Bool(boolean);
  None
}

fn format_minutes(datetime: OffsetDateTime) -> String {
  let value = datetime.to_offset(UtcOffset::UTC);
  format!(
    "{:04}-{:02}-{:02}T{:02}:{:02}+00:00",
    value.year(),
    u8::from(value.month()),
    value.day(),
    value.hour(),
    value.minute()
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_core::config::{BackendsConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig};
  use serde_yaml::Mapping;
  use std::path::PathBuf;

  fn config(pool: &[&str]) -> CharlieBotConfig {
    let options = ["a", "b", "c"]
      .into_iter()
      .map(|id| BackendOption {
        id: id.to_string(),
        label: id.to_string(),
        model: "synthetic-model".to_string(),
        prompt_overlay: None,
        backend_type: "codex".to_string(),
        type_fields: Mapping::new(),
      })
      .collect();
    CharlieBotConfig {
      charliebot_home: PathBuf::from("/synthetic/home"),
      headless_chrome_bin: "/synthetic/bin/chrome".to_string(),
      server: ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18650,
        subprocess_buffer_limit_mb: 1,
        session_memory_max_mb: 0,
        session_swap_max_mb: 0,
        session_tasks_max: 0,
        min_free_disk_gib: 0,
      },
      paths: PathsConfig { workspace_dirs: Vec::new(), worktree_dir: "/synthetic/worktrees".to_string() },
      backends: BackendsConfig { pool: pool.iter().map(|member| (*member).to_string()).collect(), options },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      sections: Sections::default(),
    }
  }

  fn select_id(pool: &BackendPool, cfg: &CharlieBotConfig, caller: &str, exclude: &[&str]) -> Option<String> {
    let excluded = exclude.iter().map(|member| (*member).to_string()).collect::<Vec<_>>();
    pool.select(cfg, caller, &excluded).unwrap().map(|option| option.id)
  }

  #[test]
  fn rotation_follows_the_pool_order() {
    let cfg = config(&["a", "b", "c"]);
    let pool = BackendPool::default();
    assert_eq!((0..4).map(|_| select_id(&pool, &cfg, "test", &[]).unwrap()).collect::<Vec<_>>(), ["a", "b", "c", "a"]);
  }

  #[test]
  fn selection_skips_cooling_members_and_callers_exclusions() {
    let cfg = config(&["a", "b", "c"]);
    let pool = BackendPool::default();
    pool.cool("a", None);
    assert_eq!(select_id(&pool, &cfg, "test", &["c"]), Some("b".to_string()));
    assert_eq!(select_id(&pool, &cfg, "test", &["b", "c"]), None);
  }

  #[test]
  fn cooldown_ends_at_the_reported_reset() {
    let cfg = config(&["a"]);
    let pool = BackendPool::default();
    let reset = UtcDateTime::parse("2030-01-02T03:04:00Z").unwrap();
    pool.cool("a", Some(reset));
    assert_eq!(select_id(&pool, &cfg, "test", &[]), None);
    let expired = UtcDateTime::parse("2000-01-02T03:04:00Z").unwrap();
    pool.cool("a", Some(expired));
    assert_eq!(select_id(&pool, &cfg, "test", &[]), Some("a".to_string()));
  }

  #[test]
  fn unknown_reset_uses_the_default_cooldown() {
    let cfg = config(&["a"]);
    let pool = BackendPool::default();
    let until = pool.cool("a", None);
    let after = OffsetDateTime::now_utc();
    let remaining = until - after;
    assert!(remaining >= DEFAULT_COOLDOWN - Duration::seconds(5));
    assert!(remaining <= DEFAULT_COOLDOWN + Duration::seconds(5));
    assert_eq!(select_id(&pool, &cfg, "test", &[]), None);
  }

  #[test]
  fn recovery_report_names_cooldowns_in_pool_order() {
    let cfg = config(&["a", "b"]);
    let pool = BackendPool::default();
    let reset = UtcDateTime::parse("2030-01-02T03:04:00Z").unwrap();
    pool.cool("a", Some(reset));
    pool.cool("b", Some(UtcDateTime::parse("2030-01-02T04:04:00Z").unwrap()));
    assert_eq!(
      pool.recovery_report(&cfg),
      "a cooling until 2030-01-02T03:04+00:00; b cooling until 2030-01-02T04:04+00:00"
    );
  }

  #[test]
  fn empty_pool_recovery_report_names_the_empty_pool() {
    assert_eq!(BackendPool::default().recovery_report(&config(&[])), "backends.pool is empty");
  }

  #[test]
  fn callers_share_cooldowns_and_rotation_position() {
    let cfg = config(&["a", "b", "c"]);
    let pool = BackendPool::default();
    assert_eq!(select_id(&pool, &cfg, "caller-a", &[]), Some("a".to_string()));
    assert_eq!(select_id(&pool, &cfg, "caller-b", &[]), Some("b".to_string()));
    pool.cool("a", Some(UtcDateTime::parse("2030-01-02T03:04:00Z").unwrap()));
    assert_eq!(select_id(&pool, &cfg, "caller-b", &[]), Some("c".to_string()));
    assert_eq!(select_id(&pool, &cfg, "caller-a", &[]), Some("b".to_string()));
  }

  #[test]
  fn quota_error_patterns_match_case_insensitively() {
    for text in [
      "You've hit your usage limit. Upgrade to Pro.",
      "Budget has been exceeded! Current cost: 12.3, Max budget: 10",
      "Disk quota exceeded",
      "rate limit reached for this key",
      "HTTP 429 Too Many Requests",
    ] {
      assert!(is_quota_error(text), "{text}");
    }
    for text in ["syntax error in the diff", "connection reset by peer"] {
      assert!(!is_quota_error(text), "{text}");
    }
  }

  #[test]
  fn pool_entries_without_options_fail_selection() {
    let mut cfg = config(&["missing"]);
    cfg.backends.options.clear();
    assert!(BackendPool::default().select(&cfg, "test", &[]).is_err());
  }
}
