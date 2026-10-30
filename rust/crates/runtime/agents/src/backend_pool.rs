//! Process-local backend rotation and quota cooldowns for unpinned work.

use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io::ErrorKind;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use anyhow::Context;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_core::text_repr;
use charliebot_core::timeouts::ROUTER_TIMEOUT_SECONDS;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::{Registry, RegistryError};
use futures_util::future::BoxFuture;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use time::{Duration, OffsetDateTime, UtcOffset};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

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

/// Error substrings of a temporary provider or gateway failure, or of a no-output stall. A pool-chosen run that hits
/// one fails over to the next member. Bare HTTP status digits, which also occur in request ids, and BadRequest or HTTP
/// 400 text stay out: those are request errors that must fail fast.
pub const BACKEND_TEMPORARY_UNAVAILABLE_PATTERNS: &[&str] = &[
  "at capacity",
  "bad gateway",
  "badgateway",
  "internal server error",
  "internalservererror",
  "service unavailable",
  "serviceunavailable",
  "gateway timeout",
  "apiconnectionerror",
  "connection error",
  "peer closed connection",
  "no output for",
  "no session progress for",
];

/// The session metadata slot that records whether the backend pool selected a worker's backend.
pub const METADATA_OWNER: &str = "backend_pool";

/// The cooldown for a quota error that names no reset time.
pub const DEFAULT_COOLDOWN: Duration = Duration::minutes(30);

/// One router answer: backend ids in preference order and the router's explanation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BackendRoute {
  pub backends: Vec<String>,
  pub reason: String,
}

/// A configured router could not start or did not answer its contract.
#[derive(Debug)]
pub struct RouterError(String);

impl Display for RouterError {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(&self.0)
  }
}

impl std::error::Error for RouterError {}

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

  /// Select the first available member in a fixed router order without moving pool rotation.
  pub fn pick_ordered(
    &self,
    cfg: &CharlieBotConfig,
    caller: &str,
    backends: &[String],
    exclude: &[String],
  ) -> anyhow::Result<Option<BackendOption>> {
    let now = UtcDateTime::now().into_inner();
    let state = self.state.lock().expect("backend pool mutex is not poisoned");
    for member in backends {
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
      tracing::info!(target: "charliebot_agents", caller, member, source = "route", "backend_pool_selected");
      let option = cfg
        .get_backend_option(member)
        .with_context(|| format!("routed backend '{member}' is not in backends.options"))?;
      return Ok(Some(option.clone()));
    }
    tracing::warn!(target: "charliebot_agents", caller, backends = ?backends, excluded = ?exclude, "backend_pool_route_exhausted");
    Ok(None)
  }

  /// Ask `backends.routing.command` to order candidate backends for one role.
  pub async fn route(
    &self,
    cfg: &CharlieBotConfig,
    role: &str,
    text: &str,
    candidates: &[String],
  ) -> Result<BackendRoute, RouterError> {
    let routing =
      cfg.backends.routing.as_ref().ok_or_else(|| RouterError("backends.routing is not configured".to_string()))?;
    self.route_command(&routing.command, role, text, candidates, StdDuration::from_secs(ROUTER_TIMEOUT_SECONDS)).await
  }

  async fn route_command(
    &self,
    command: &[String],
    role: &str,
    text: &str,
    candidates: &[String],
    timeout: StdDuration,
  ) -> Result<BackendRoute, RouterError> {
    let program = command.first().ok_or_else(|| RouterError("backends.routing.command is empty".to_string()))?;
    let candidate_entries = self.route_candidates(candidates);
    let payload = json!({"role": role, "text": text, "candidates": candidate_entries});
    let input =
      serde_json::to_vec(&payload).map_err(|error| RouterError(format!("could not encode router input: {error}")))?;
    let mut process = Command::new(program);
    process
      .args(&command[1..])
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .process_group(0)
      .kill_on_drop(true);
    let mut child = process
      .spawn()
      .map_err(|error| RouterError(format!("backend router command {program} could not start: {error}")))?;
    let pid = child.id().expect("a spawned router has a process id");
    let _group_guard = RouterProcessGroupGuard { pid };
    let output = match tokio::time::timeout(timeout, communicate(&mut child, &input)).await {
      Ok(Ok(output)) => output,
      Ok(Err(error)) => {
        return Err(RouterError(format!("backend router command {program} communication failed: {error}")));
      }
      Err(_) => {
        kill_router_group(pid)
          .map_err(|error| RouterError(format!("could not stop backend router command {program}: {error}")))?;
        child.wait().await.map_err(|error| {
          RouterError(format!("could not reap timed out backend router command {program}: {error}"))
        })?;
        return Err(RouterError(format!("backend router command {program} timed out after {}s", timeout.as_secs())));
      }
    };
    kill_router_group(pid)
      .map_err(|error| RouterError(format!("could not clean up backend router command {program}: {error}")))?;
    parse_router_output(program, output, candidates)
  }

  fn route_candidates(&self, candidates: &[String]) -> Vec<Value> {
    let now = UtcDateTime::now().into_inner();
    let state = self.state.lock().expect("backend pool mutex is not poisoned");
    candidates
      .iter()
      .map(|candidate| {
        let cooling_until =
          state.cooling_until.get(candidate).filter(|until| **until > now).map(|until| format_isoformat(*until));
        json!({"id": candidate, "cooling_until": cooling_until})
      })
      .collect()
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

  /// Return the stored cooldown deadline, whether or not it has passed.
  pub fn cooling_until(&self, backend_id: &str) -> Option<OffsetDateTime> {
    self.state.lock().expect("backend pool mutex is not poisoned").cooling_until.get(backend_id).copied()
  }

  /// Return the stored cooldown deadline in Python's minute-resolution event format.
  pub fn cooling_until_minutes(&self, backend_id: &str) -> Option<String> {
    self.cooling_until(backend_id).map(format_minutes)
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

  /// The line a caller reports when `select` found no member.
  ///
  /// Subagent resolution, the improve loop's per-iteration pick and the quota failover all report it, and an
  /// operator reads the same wording whichever one refused.
  pub fn no_member_message(&self, cfg: &CharlieBotConfig) -> String {
    format!("no backends.pool member is available: {}", self.recovery_report(cfg))
  }
}

/// Return whether an error string contains a known quota error substring.
pub fn is_quota_error(text: &str) -> bool {
  let lowered = text.to_lowercase();
  QUOTA_ERROR_PATTERNS.iter().any(|pattern| lowered.contains(pattern))
}

/// Return whether an error string contains a temporary backend failure substring.
pub fn is_temporary_unavailable_error(text: &str) -> bool {
  let lowered = text.to_lowercase();
  BACKEND_TEMPORARY_UNAVAILABLE_PATTERNS.iter().any(|pattern| lowered.contains(pattern))
}

/// Return whether a Claude Code `result` event reports a temporary backend failure. A present status decides alone:
/// 429 and 5xx fail over, and any other integer or non-integer status does not, without reading the text. A null or
/// absent status falls back to the text of the result.
pub fn is_temporary_unavailable_result(event: &Value) -> bool {
  match event.get("api_error_status") {
    None | Some(Value::Null) => {
      is_temporary_unavailable_error(&event.get("result").map(text_repr::value_str).unwrap_or_default())
    }
    Some(status) => status.as_i64().is_some_and(|code| code == 429 || code >= 500),
  }
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
      owner: METADATA_OWNER,
      on: MetadataFile::Session,
      after: None,
      fields: vec![
        SlotField { name: "backend_from_pool", default: serde_json::json!(false) },
        SlotField { name: "route", default: Value::Null },
      ],
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
  if let Some(route) = values.get("route")
    && !route.is_null()
    && serde_json::from_value::<BackendRoute>(route.clone()).is_err()
  {
    return Some("route must be null or contain backends: list[str] and reason: str".to_string());
  }
  None
}

fn format_minutes(datetime: OffsetDateTime) -> String {
  const MINUTES: &[time::format_description::FormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day]T[hour]:[minute][offset_hour sign:mandatory]:[offset_minute]"
  );
  datetime.to_offset(UtcOffset::UTC).format(MINUTES).expect("a UTC timestamp formats")
}

fn format_isoformat(datetime: OffsetDateTime) -> String {
  charliebot_core::timefmt::isoformat(datetime.to_offset(UtcOffset::UTC))
}

struct RouterOutput {
  status: ExitStatus,
  stdout: Vec<u8>,
  stderr: Vec<u8>,
}

async fn communicate(child: &mut Child, input: &[u8]) -> std::io::Result<RouterOutput> {
  let mut stdin = child.stdin.take().expect("router stdin is piped");
  let mut stdout = child.stdout.take().expect("router stdout is piped");
  let mut stderr = child.stderr.take().expect("router stderr is piped");
  let write = async move {
    let result = stdin.write_all(input).await;
    drop(stdin);
    match result {
      Ok(()) => Ok(()),
      Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
      Err(error) => Err(error),
    }
  };
  let read_stdout = async {
    let mut output = Vec::new();
    stdout.read_to_end(&mut output).await.map(|_| output)
  };
  let read_stderr = async {
    let mut output = Vec::new();
    stderr.read_to_end(&mut output).await.map(|_| output)
  };
  let (write_result, stdout_result, stderr_result, status_result) =
    tokio::join!(write, read_stdout, read_stderr, child.wait());
  write_result?;
  Ok(RouterOutput { status: status_result?, stdout: stdout_result?, stderr: stderr_result? })
}

fn parse_router_output(
  program: &str,
  output: RouterOutput,
  candidates: &[String],
) -> Result<BackendRoute, RouterError> {
  let status_code = output.status.code().map_or_else(|| "signal".to_string(), |code| code.to_string());
  let stderr = stderr_tail(&output.stderr);
  if !output.status.success() {
    return Err(RouterError(format!("backend router command {program} exited {status_code}; stderr: {stderr}")));
  }
  let parsed: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
    RouterError(format!(
      "backend router command {program} exited {status_code} with stdout outside the router contract ({error}); stderr: {stderr}"
    ))
  })?;
  let object = parsed.as_object();
  let backends = object
    .and_then(|object| object.get("backends"))
    .and_then(Value::as_array)
    .filter(|backends| backends.iter().all(Value::is_string));
  let reason = object.and_then(|object| object.get("reason")).and_then(Value::as_str);
  let (Some(backends), Some(reason)) = (backends, reason) else {
    return Err(RouterError(format!(
      "backend router command {program} exited {status_code} with stdout outside the router contract \
       (backends: list[str], reason: str expected); stderr: {stderr}"
    )));
  };
  let backends = backends
    .iter()
    .map(|backend| backend.as_str().expect("backend element is a string").to_string())
    .collect::<Vec<_>>();
  let unknown = backends.iter().filter(|backend| !candidates.contains(backend)).collect::<Vec<_>>();
  if !unknown.is_empty() {
    return Err(RouterError(format!(
      "backend router command {program} returned ids outside the candidates: {unknown:?}; stderr: {stderr}"
    )));
  }
  Ok(BackendRoute { backends, reason: reason.to_string() })
}

fn stderr_tail(stderr: &[u8]) -> String {
  let decoded = String::from_utf8_lossy(stderr);
  let trimmed = decoded.trim();
  let mut chars = trimmed.chars().rev().take(500).collect::<Vec<_>>();
  chars.reverse();
  chars.into_iter().collect()
}

fn kill_router_group(pid: u32) -> nix::Result<()> {
  match signal::killpg(Pid::from_raw(i32::try_from(pid).expect("process id fits pid_t")), Signal::SIGKILL) {
    Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
    Err(error) => Err(error),
  }
}

struct RouterProcessGroupGuard {
  pid: u32,
}

impl Drop for RouterProcessGroupGuard {
  fn drop(&mut self) {
    if let Err(error) = kill_router_group(self.pid) {
      tracing::error!(target: "charliebot_agents", pid = self.pid, error = %error, "backend_router_group_cleanup_failed");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_core::config::{BackendsConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig};
  use serde_yaml::Mapping;
  use std::fs;
  use std::path::PathBuf;
  use std::time::Duration as StdDuration;

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
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic/worktrees".to_string(),
        scratch_dir: "/synthetic/scripts".to_string(),
      },
      backends: BackendsConfig {
        pool: pool.iter().map(|member| (*member).to_string()).collect(),
        routing: None,
        options,
      },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      sections: Sections::default(),
    }
  }

  fn select_id(pool: &BackendPool, cfg: &CharlieBotConfig, caller: &str, exclude: &[&str]) -> Option<String> {
    let excluded = exclude.iter().map(|member| (*member).to_string()).collect::<Vec<_>>();
    pool.select(cfg, caller, &excluded).unwrap().map(|option| option.id)
  }

  fn router_command(directory: &tempfile::TempDir, body: &str, arguments: &[&str]) -> Vec<String> {
    let path = directory.path().join("fake-router.sh");
    fs::write(&path, format!("set -eu\n{body}\n")).unwrap();
    let mut command = vec!["/bin/sh".to_string(), path.to_string_lossy().into_owned()];
    command.extend(arguments.iter().map(|argument| (*argument).to_string()));
    command
  }

  fn configured_router(cfg: &mut CharlieBotConfig, command: Vec<String>) {
    cfg.backends.routing = Some(charliebot_core::config::RoutingConfig { command });
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
  fn no_member_message_joins_the_refusal_and_the_recovery_report() {
    let cfg = config(&["a"]);
    let pool = BackendPool::default();
    pool.cool("a", Some(UtcDateTime::parse("2030-01-02T03:04:00Z").unwrap()));
    assert_eq!(
      pool.no_member_message(&cfg),
      "no backends.pool member is available: a cooling until 2030-01-02T03:04+00:00"
    );
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
  fn pick_ordered_takes_the_first_available_route_member() {
    // Python: tests/test_backend_router.py:154-157.
    let cfg = config(&["a", "b", "c"]);
    let selected = BackendPool::default().pick_ordered(&cfg, "test", &["b".to_string(), "a".to_string()], &[]).unwrap();
    assert_eq!(selected.map(|option| option.id), Some("b".to_string()));
  }

  #[test]
  fn pick_ordered_skips_cooling_and_excluded_members() {
    // Python: tests/test_backend_router.py:160-171.
    let cfg = config(&["a", "b", "c"]);
    let pool = BackendPool::default();
    pool.cool("b", None);
    assert_eq!(
      pool.pick_ordered(&cfg, "test", &["b".to_string(), "a".to_string()], &[]).unwrap().map(|option| option.id),
      Some("a".to_string())
    );
    assert_eq!(pool.pick_ordered(&cfg, "test", &["b".to_string(), "a".to_string()], &["a".to_string()]).unwrap(), None);
  }

  #[test]
  fn pick_ordered_shares_the_pool_cooldown_table() {
    // Python: tests/test_backend_router.py:174-179.
    let cfg = config(&["a", "b"]);
    let pool = BackendPool::default();
    pool.cool("a", None);
    assert_eq!(select_id(&pool, &cfg, "test", &[]), Some("b".to_string()));
    assert_eq!(
      pool.pick_ordered(&cfg, "test", &["a".to_string(), "b".to_string()], &[]).unwrap().map(|option| option.id),
      Some("b".to_string())
    );
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

  #[tokio::test]
  async fn router_returns_its_order_and_reason() {
    let directory = tempfile::tempdir().unwrap();
    let command = router_command(
      &directory,
      "cat >/dev/null\nprintf '%s' '{\"backends\":[\"b\",\"a\"],\"reason\":\"ranked\"}'",
      &[],
    );
    let mut cfg = config(&["a", "b"]);
    configured_router(&mut cfg, command);

    let route = BackendPool::default()
      .route(&cfg, "implement", "design task", &["a".to_string(), "b".to_string()])
      .await
      .unwrap();
    assert_eq!(route, BackendRoute { backends: vec!["b".to_string(), "a".to_string()], reason: "ranked".to_string() });
  }

  #[tokio::test]
  async fn router_receives_role_text_and_each_cooling_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let capture = directory.path().join("stdin.json");
    let capture_text = capture.to_string_lossy().into_owned();
    let command = router_command(
      &directory,
      "cat > \"$1\"\nprintf '%s' '{\"backends\":[],\"reason\":\"captured\"}'",
      &[&capture_text],
    );
    let mut cfg = config(&["a", "b"]);
    configured_router(&mut cfg, command);
    let pool = BackendPool::default();
    pool.cool("a", Some(UtcDateTime::parse("2030-01-02T03:04:00Z").unwrap()));
    pool.cool("b", Some(UtcDateTime::parse("2000-01-02T03:04:00Z").unwrap()));

    let route = pool.route(&cfg, "thread", "untrimmed thread", &["a".to_string(), "b".to_string()]).await.unwrap();
    assert_eq!(route.reason, "captured");
    let request: Value = serde_json::from_slice(&fs::read(capture).unwrap()).unwrap();
    assert_eq!(request["role"], "thread");
    assert_eq!(request["text"], "untrimmed thread");
    assert_eq!(
      request["candidates"],
      serde_json::json!([
        {"id": "a", "cooling_until": "2030-01-02T03:04:00+00:00"},
        {"id": "b", "cooling_until": null},
      ])
    );
  }

  #[tokio::test]
  async fn router_failures_surface_start_exit_contract_unknown_id_and_timeout() {
    let pool = BackendPool::default();
    let candidates = ["a".to_string(), "b".to_string()];

    let missing = pool
      .route_command(
        &["/synthetic/missing-router".to_string()],
        "implement",
        "task",
        &candidates,
        StdDuration::from_secs(1),
      )
      .await
      .unwrap_err();
    assert!(missing.to_string().contains("missing-router"));

    let directory = tempfile::tempdir().unwrap();
    let command = router_command(&directory, "cat >/dev/null\nprintf '%s' 'router boom' >&2\nexit 3", &[]);
    let exited =
      pool.route_command(&command, "implement", "task", &candidates, StdDuration::from_secs(1)).await.unwrap_err();
    assert!(exited.to_string().contains("exited 3") && exited.to_string().contains("router boom"));

    let command = router_command(&directory, "cat >/dev/null\nprintf '%s' 'not json'", &[]);
    let malformed =
      pool.route_command(&command, "implement", "task", &candidates, StdDuration::from_secs(1)).await.unwrap_err();
    assert!(malformed.to_string().contains("stdout outside the router contract"));

    let command = router_command(
      &directory,
      "cat >/dev/null\nprintf '%s' '{\"backends\":[\"ghost\"],\"reason\":\"unknown\"}'",
      &[],
    );
    let unknown =
      pool.route_command(&command, "implement", "task", &candidates, StdDuration::from_secs(1)).await.unwrap_err();
    assert!(unknown.to_string().contains("ghost"));

    let command = router_command(&directory, "cat >/dev/null\nsleep 5", &[]);
    let timeout =
      pool.route_command(&command, "implement", "task", &candidates, StdDuration::from_millis(100)).await.unwrap_err();
    assert!(timeout.to_string().contains("timed out"));
  }

  #[test]
  fn router_timeout_stays_below_the_delegate_http_timeout() {
    assert_eq!(ROUTER_TIMEOUT_SECONDS, 25);
    const { assert!(ROUTER_TIMEOUT_SECONDS < 30) };
  }
}
