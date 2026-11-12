//! Process-local backend rotation for unpinned work, reading "out until" from the shared availability owner.

use std::fmt::{Display, Formatter};
use std::io::ErrorKind;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use charliebot_core::config::{BackendOption, CallTarget, CharlieBotConfig, PoolConfig};
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
use time::{OffsetDateTime, UtcOffset};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::backend_availability::BackendAvailability;
use crate::model_pool::{self, RotationPositions};
use charliebot_store::metadata::UtcDateTime;

/// Error substrings that mark a backend as out of quota; the aigw gateway's `402 Insufficient Balance` text is one.
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
  "insufficient balance",
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

/// Error substrings that mark a member refusing access: Claude Code's authentication-failure marker and the
/// gateway's model-access refusal.
pub const ACCESS_REFUSED_PATTERNS: &[&str] = &["failed to authenticate", "not allowed to access model"];

/// How one member failure sorts: a spent quota, a refusal of authentication or model access, or anything else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberFailure {
  Quota,
  AccessRefused,
  Other,
}

impl MemberFailure {
  /// The availability reason this failure records, or `None` when it records the member nowhere.
  pub fn availability_reason(self) -> Option<&'static str> {
    match self {
      Self::Quota => Some(crate::backend_availability::REASON_QUOTA_ERROR),
      Self::AccessRefused => Some(crate::backend_availability::REASON_ACCESS_REFUSED),
      Self::Other => None,
    }
  }

  /// Whether this failure records the member out of every pool that lists it.
  pub fn records_out(self) -> bool {
    self.availability_reason().is_some()
  }
}

/// The session metadata slot that records the route a delegation keeps.
pub const METADATA_OWNER: &str = "backend_pool";

/// The route a request keeps on its session: the backend ids the router named, and the router's explanation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BackendRoute {
  pub backends: Vec<String>,
  pub reason: String,
}

/// One router answer: whether the router routed the request, the backend ids in preference order, and the reason.
#[derive(Clone, Debug, PartialEq)]
pub struct RouterAnswer {
  pub routed: bool,
  pub backends: Vec<String>,
  pub reason: String,
}

/// Where one routed request runs.
#[derive(Debug)]
pub enum RouteTarget {
  /// The router did not route the request, the router failed, or the one backend it named is out: the caller's default
  /// path runs it.
  NotRouted,
  /// The `backends.options` id or the `backends.pools` id the router named. A pool id runs on the member its rule
  /// picks.
  Backend(String),
}

/// One routed request: the route its session keeps, and where it runs.
#[derive(Debug)]
pub struct RoutedRequest {
  pub route: BackendRoute,
  pub target: RouteTarget,
}

impl RoutedRequest {
  /// The request after a router failure: it takes the caller's default path, and its route names the failure.
  pub fn failed(error: &RouterError) -> Self {
    Self {
      route: BackendRoute { backends: Vec::new(), reason: format!("router failed: {error}") },
      target: RouteTarget::NotRouted,
    }
  }
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

/// The state every unpinned backend caller shares: the availability owner, the registry whose member readings the
/// selection reads, and the rotation positions of the rotate pools.
pub struct BackendPool {
  availability: Arc<BackendAvailability>,
  registry: Arc<Registry>,
  positions: Arc<RotationPositions>,
}

impl BackendPool {
  /// Build the pool on top of the availability owner every caller reads "out until" from, the registry whose member
  /// readings the rotate rule consults, and the rotation positions every caller of a pool shares.
  pub fn new(
    availability: Arc<BackendAvailability>,
    registry: Arc<Registry>,
    positions: Arc<RotationPositions>,
  ) -> Self {
    Self { availability, registry, positions }
  }

  /// The availability owner this pool reads "out until" from, for callers that record a member out.
  pub fn availability(&self) -> Arc<BackendAvailability> {
    Arc::clone(&self.availability)
  }

  /// One selection over the process-wide owners this pool holds. A caller that also holds a store chains
  /// [`model_pool::Selector::with_store`], so a priority pool orders its members by session load.
  pub fn selector<'a>(&'a self, cfg: &'a CharlieBotConfig) -> model_pool::Selector<'a> {
    model_pool::Selector::new(cfg, &self.registry, Arc::clone(&self.availability), Arc::clone(&self.positions))
  }

  fn out_until(&self, member: &str) -> Option<OffsetDateTime> {
    self.availability.out_until(member)
  }

  /// Ask `backends.routing.command` for one request's answer. `session_backend` is the delegating session's backend id,
  /// or None for a thread.
  pub async fn route(
    &self,
    cfg: &CharlieBotConfig,
    role: &str,
    text: &str,
    candidates: &[String],
    session_backend: Option<&str>,
  ) -> Result<RouterAnswer, RouterError> {
    let routing =
      cfg.backends.routing.as_ref().ok_or_else(|| RouterError("backends.routing is not configured".to_string()))?;
    let payload = json!({
      "role": role,
      "text": text,
      "candidates": self.route_candidates(cfg, candidates),
      "session_backend": session_backend,
    });
    self.route_command(&routing.command, payload, candidates, StdDuration::from_secs(ROUTER_TIMEOUT_SECONDS)).await
  }

  /// Turn one router answer into the route the request keeps and the place it runs. A routed answer names exactly one
  /// id. A pool id is the place the request runs: its delegation selects the member the rule picks for the work run. An
  /// options id runs it unless the member is out, in which case the caller's default path runs it.
  pub fn decide(&self, cfg: &CharlieBotConfig, caller: &str, answer: RouterAnswer) -> anyhow::Result<RoutedRequest> {
    let RouterAnswer { routed, backends, reason } = answer;
    if !routed {
      return Ok(RoutedRequest { route: BackendRoute { backends, reason }, target: RouteTarget::NotRouted });
    }
    let [backend] = backends.as_slice() else {
      anyhow::bail!("{caller}: a routed answer names exactly one backend id, got {}", backends.len());
    };
    let backend = backend.clone();
    if cfg.backends.pool(&backend).is_some() || self.decide_option_available(cfg, &backend)? {
      return Ok(RoutedRequest { route: BackendRoute { backends, reason }, target: RouteTarget::Backend(backend) });
    }
    let reason = format!("{reason}; {backend} cooling; ran on the default path");
    Ok(RoutedRequest { route: BackendRoute { backends, reason }, target: RouteTarget::NotRouted })
  }

  /// Whether the one `backends.options` id a routed answer names can run now.
  fn decide_option_available(&self, cfg: &CharlieBotConfig, backend_id: &str) -> anyhow::Result<bool> {
    if cfg.get_backend_option(backend_id).is_none() {
      return Ok(false);
    }
    self.selector(cfg).option_available(backend_id, &[])
  }

  /// Run the router command on `payload` and parse its answer against the candidate ids it was offered.
  async fn route_command(
    &self,
    command: &[String],
    payload: Value,
    candidates: &[String],
    timeout: StdDuration,
  ) -> Result<RouterAnswer, RouterError> {
    let program = command.first().ok_or_else(|| RouterError("backends.routing.command is empty".to_string()))?;
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

  /// The candidate entries the router reads. A pool id reports the moment its last selectable member recovers, and no
  /// moment while any member is selectable.
  fn route_candidates(&self, cfg: &CharlieBotConfig, candidates: &[String]) -> Vec<Value> {
    let now = UtcDateTime::now().into_inner();
    candidates
      .iter()
      .map(|candidate| {
        let cooling_until = match cfg.backends.pool(candidate) {
          Some(pool) => self.pool_cooling_until(pool, now),
          None => self.out_until(candidate).filter(|until| *until > now).map(format_isoformat),
        };
        json!({"id": candidate, "cooling_until": cooling_until})
      })
      .collect()
  }

  /// The moment a pool becomes selectable again: `None` while one member has no out record, else the earliest recovery.
  fn pool_cooling_until(&self, pool: &PoolConfig, now: OffsetDateTime) -> Option<String> {
    let recoveries = pool
      .members
      .iter()
      .map(|member| self.out_until(&member.backend).filter(|until| *until > now))
      .collect::<Option<Vec<_>>>()?;
    recoveries.into_iter().min().map(format_isoformat)
  }
}

/// The members one call of a `backends.calls` class tries, in order. A pool target selects its next member, excluding
/// the members already tried, so a caller that records a failed member out moves on to the next one. A model target
/// yields that model once. An unset class yields nothing; the caller decides what an unset class means.
pub fn one_shot_members<'a>(
  cfg: &'a CharlieBotConfig,
  pool: Arc<BackendPool>,
  caller: &'a str,
  target: Option<&'a CallTarget>,
) -> impl Iterator<Item = anyhow::Result<BackendOption>> + 'a {
  let mut tried: Vec<String> = Vec::new();
  let mut done = false;
  std::iter::from_fn(move || {
    if done {
      return None;
    }
    let next = match target {
      None => return None,
      Some(CallTarget::Session) => Err(anyhow::anyhow!("backends.calls.{caller} cannot name the session")),
      Some(CallTarget::Backend(id)) if cfg.backends.pool(id).is_some() => pool.selector(cfg).select(id, &tried),
      Some(CallTarget::Backend(id)) => cfg
        .get_backend_option(id)
        .cloned()
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("backends.calls.{caller} names '{id}', which is not a backends.options id")),
    };
    let is_pool = matches!(target, Some(CallTarget::Backend(id)) if cfg.backends.pool(id).is_some());
    if !is_pool {
      done = true;
    }
    match next {
      Ok(Some(option)) => {
        tried.push(option.id.clone());
        Some(Ok(option))
      }
      Ok(None) => {
        done = true;
        None
      }
      Err(error) => {
        done = true;
        Some(Err(error))
      }
    }
  })
}

/// Return whether an error string contains a known quota error substring. A bare status code matches only as a whole
/// token: a request id carries the same digits, and a request error that names one must fail fast.
pub fn is_quota_error(text: &str) -> bool {
  let lowered = text.to_lowercase();
  QUOTA_ERROR_PATTERNS.iter().any(|pattern| {
    if pattern.bytes().all(|byte| byte.is_ascii_digit()) {
      contains_token(&lowered, pattern)
    } else {
      lowered.contains(pattern)
    }
  })
}

/// Whether `token` occurs in `text` with no letter, digit or underscore on either side.
fn contains_token(text: &str, token: &str) -> bool {
  let is_word = |character: char| character.is_alphanumeric() || character == '_';
  text.match_indices(token).any(|(start, _)| {
    !text[..start].chars().next_back().is_some_and(is_word)
      && !text[start + token.len()..].chars().next().is_some_and(is_word)
  })
}

/// Return whether an error text marks a spent quota rather than a temporary failure. This is the one verdict that
/// manager turns and worker runs share: a quota substring matches and no temporary-failure substring does.
pub fn is_spent_quota_error(text: &str) -> bool {
  is_quota_error(text) && !is_temporary_unavailable_error(text)
}

/// Sort one member failure from a launch verdict and its text. A stated quota verdict decides first; then a spent
/// quota text; then a refusal of authentication or model access; anything else is other. Every record site uses this
/// one sort, so a refusal no longer lands in the quota reason.
pub fn classify_member_failure(text: &str, quota_exhausted: Option<bool>) -> MemberFailure {
  if quota_exhausted == Some(true) || is_spent_quota_error(text) {
    MemberFailure::Quota
  } else if is_access_refused_error(text) {
    MemberFailure::AccessRefused
  } else {
    MemberFailure::Other
  }
}

/// Return whether an error text marks an authentication or model-access refusal. A `401` or `403` status counts only
/// inside an `API Error` line and as a whole token, so `400 Bad Request` and the digits in a request id never match.
pub fn is_access_refused_error(text: &str) -> bool {
  let lowered = text.to_lowercase();
  ACCESS_REFUSED_PATTERNS.iter().any(|pattern| lowered.contains(pattern))
    || lowered
      .lines()
      .any(|line| line.contains("api error") && (contains_token(line, "401") || contains_token(line, "403")))
}

/// Return whether a Claude Code `result` event's text marks a spent quota. The caller has checked `is_error`.
pub fn is_spent_quota_result(event: &Value) -> bool {
  event.get("result").map(text_repr::value_str).is_some_and(|text| is_spent_quota_error(&text))
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

/// Insert the process-wide availability owner and the pool that reads it before recovery starts. A state file that
/// exists but cannot be read fails the step, and the startup stops with the error that names the file.
pub fn pre_recovery_step(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    let cfg = features.config.get()?;
    features.startup.insert(BackendAvailability::load(&cfg.charliebot_home)?)?;
    let availability = features.startup.require::<BackendAvailability>()?;
    features.startup.insert(RotationPositions::default())?;
    let positions = features.startup.require::<RotationPositions>()?;
    features.startup.insert(BackendPool::new(availability, Arc::clone(&features.registry), positions))?;
    Ok(())
  })
}

/// Register the session metadata slot owned by `features.backend_pool`.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: METADATA_OWNER,
      on: MetadataFile::Session,
      after: None,
      fields: vec![SlotField { name: "route", default: Value::Null }],
    },
    validate_backend_pool_fields,
  )
}

fn validate_backend_pool_fields(values: &mut Map<String, Value>) -> Option<String> {
  if let Some(route) = values.get("route")
    && !route.is_null()
    && serde_json::from_value::<BackendRoute>(route.clone()).is_err()
  {
    return Some("route must be null or contain backends: list[str] and reason: str".to_string());
  }
  None
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
) -> Result<RouterAnswer, RouterError> {
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
  let routed = object.and_then(|object| object.get("routed")).and_then(Value::as_bool);
  let backends = object
    .and_then(|object| object.get("backends"))
    .and_then(Value::as_array)
    .filter(|backends| backends.iter().all(Value::is_string));
  let reason = object.and_then(|object| object.get("reason")).and_then(Value::as_str);
  let (Some(routed), Some(backends), Some(reason)) = (routed, backends, reason) else {
    return Err(RouterError(format!(
      "backend router command {program} exited {status_code} with stdout outside the router contract \
       (routed: bool, backends: list[str], reason: str expected); stderr: {stderr}"
    )));
  };
  let backends = backends
    .iter()
    .map(|backend| backend.as_str().expect("backend element is a string").to_string())
    .collect::<Vec<_>>();
  if !routed && !backends.is_empty() {
    return Err(RouterError(format!(
      "backend router command {program} answered routed false with backends {backends:?}; stderr: {stderr}"
    )));
  }
  let unknown = backends.iter().filter(|backend| !candidates.contains(backend)).collect::<Vec<_>>();
  if !unknown.is_empty() {
    return Err(RouterError(format!(
      "backend router command {program} returned ids outside the candidates: {unknown:?}; stderr: {stderr}"
    )));
  }
  Ok(RouterAnswer { routed, backends, reason: reason.to_string() })
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
  use crate::backend_availability;
  use charliebot_core::config::{
    BackendsConfig, CallsConfig, PathsConfig, PoolConfig, PoolMember, PoolRule, Sections, ServerConfig, TelegramConfig,
    UiConfig,
  };
  use charliebot_registry::backend::MemberReading;
  use serde_yaml::Mapping;
  use std::fs;
  use std::path::{Path, PathBuf};
  use std::time::Duration as StdDuration;
  use time::Duration;

  /// The rotate pool that every selection test names.
  const POOL: &str = "light";

  /// Options `a`, `b` and `c`, and the rotate pool [`POOL`] over `members` in order.
  fn config(members: &[&str]) -> CharlieBotConfig {
    let options = ["a", "b", "c"]
      .into_iter()
      .map(|id| BackendOption {
        id: id.to_string(),
        label: id.to_string(),
        model: "synthetic-model".to_string(),
        prompt_overlay: None,
        billing: None,
        backend_type: "codex".to_string(),
        type_fields: Mapping::new(),
        visible: true,
      })
      .collect();
    let pools = vec![PoolConfig {
      id: POOL.to_string(),
      label: "Light".to_string(),
      rule: PoolRule::Rotate,
      members: members.iter().map(|member| PoolMember { backend: (*member).to_string() }).collect(),
      visible: true,
    }];
    CharlieBotConfig {
      charliebot_home: PathBuf::from("/synthetic/home"),
      headless_chrome_bin: "/synthetic/bin/chrome".to_string(),
      server: ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18650,
        subprocess_buffer_limit_mb: 1,
        session_memory_max_mb: 0,
        session_swap_max_mb: 0,
        public_url: None,
        session_tasks_max: 0,
        min_free_disk_gib: 0,
        enable_session_cpu_weight: true,
        enable_session_io_weight: true,
        enable_local_work_guard: false,
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic/worktrees".to_string(),
        scratch_dir: "/synthetic/scripts".to_string(),
      },
      backends: BackendsConfig { routing: None, options, pools, calls: CallsConfig::default() },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      remote_run: charliebot_core::config::RemoteRunConfig::default(),
      github: Default::default(),
      sections: Sections::default(),
    }
  }

  fn select_id(pool: &BackendPool, cfg: &CharlieBotConfig, _caller: &str, exclude: &[&str]) -> Option<String> {
    let excluded = exclude.iter().map(|member| (*member).to_string()).collect::<Vec<_>>();
    pool.selector(cfg).select(POOL, &excluded).unwrap().map(|option| option.id)
  }

  /// A pool whose owner holds no record and never persists one: the selection tests never record.
  fn pool() -> BackendPool {
    let home = tempfile::tempdir().unwrap();
    pool_at(home.path())
  }

  /// A pool on a live home, for the tests that record a member out through the owner.
  fn pool_at(home: &Path) -> BackendPool {
    pool_with(home, Arc::new(Registry::new()))
  }

  fn pool_with(home: &Path, registry: Arc<Registry>) -> BackendPool {
    BackendPool::new(
      Arc::new(BackendAvailability::load(home).unwrap()),
      registry,
      Arc::new(RotationPositions::default()),
    )
  }

  /// A registry whose `codex` reading says member `b` is exhausted and says nothing about the other members.
  fn exhausted_b_registry() -> Arc<Registry> {
    fn register(registry: &mut Registry) -> Result<(), RegistryError> {
      registry.member_reading(
        "codex",
        Arc::new(|_cfg, option, _now| {
          Ok((option.id == "b").then(|| MemberReading {
            windows: Vec::new(),
            weekly_resets_at: None,
            near_exhaustion: false,
            exhausted: true,
          }))
        }),
      )
    }
    let mut registry = Registry::new();
    registry.register_package("test.backend_pool", register).unwrap();
    Arc::new(registry)
  }

  fn record(pool: &BackendPool, member: &str, reset: Option<UtcDateTime>) {
    pool
      .availability()
      .record(member, reset, backend_availability::REASON_QUOTA_ERROR, "synthetic quota error")
      .unwrap();
  }

  /// The payload of a task-role routing request, as `route` builds it for a task.
  fn router_payload() -> Value {
    json!({"role": "task", "text": "implement", "candidates": [], "session_backend": null})
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
    let pool = pool();
    assert_eq!((0..4).map(|_| select_id(&pool, &cfg, "test", &[]).unwrap()).collect::<Vec<_>>(), ["a", "b", "c", "a"]);
  }

  #[test]
  fn selection_skips_a_member_whose_reading_says_it_is_exhausted() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(&["a", "b", "c"]);
    let pool = pool_with(home.path(), exhausted_b_registry());
    assert_eq!(select_id(&pool, &cfg, "test", &[]), Some("a".to_string()));
    assert_eq!(select_id(&pool, &cfg, "test", &[]), Some("c".to_string()), "b's reading says it is exhausted");
  }

  #[test]
  fn selection_skips_out_members_and_callers_exclusions() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(&["a", "b", "c"]);
    let pool = pool_at(home.path());
    record(&pool, "a", None);
    assert_eq!(select_id(&pool, &cfg, "test", &["c"]), Some("b".to_string()));
    assert_eq!(select_id(&pool, &cfg, "test", &["b", "c"]), None);
  }

  #[test]
  fn a_member_the_owner_recorded_out_stays_out_after_a_rebuild_from_the_same_home() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(&["a", "b", "c"]);
    let pool = pool_at(home.path());
    record(&pool, "a", None);

    let reloaded = pool_at(home.path());
    assert_eq!(select_id(&reloaded, &cfg, "test", &[]), Some("b".to_string()));
  }

  #[test]
  fn cooling_ends_at_the_reported_reset() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(&["a"]);
    let pool = pool_at(home.path());
    let reset = UtcDateTime::parse("2030-01-02T03:04:00Z").unwrap();
    record(&pool, "a", Some(reset));
    assert_eq!(select_id(&pool, &cfg, "test", &[]), None);
    let expired = UtcDateTime::parse("2000-01-02T03:04:00Z").unwrap();
    record(&pool, "a", Some(expired));
    assert_eq!(select_id(&pool, &cfg, "test", &[]), Some("a".to_string()));
  }

  #[test]
  fn unknown_reset_uses_the_default_cooldown() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(&["a"]);
    let pool = pool_at(home.path());
    let until =
      pool.availability().record("a", None, backend_availability::REASON_QUOTA_ERROR, "synthetic quota error").unwrap();
    let after = OffsetDateTime::now_utc();
    let remaining = until - after;
    assert!(remaining >= backend_availability::DEFAULT_COOLDOWN - Duration::seconds(5));
    assert!(remaining <= backend_availability::DEFAULT_COOLDOWN + Duration::seconds(5));
    assert_eq!(select_id(&pool, &cfg, "test", &[]), None);
  }

  #[test]
  fn a_refused_member_is_skipped_by_select_and_peek_and_listed_with_its_reason() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(&["a", "b"]);
    let pool = pool_at(home.path());
    pool
      .availability()
      .record("a", None, backend_availability::REASON_ACCESS_REFUSED, "Failed to authenticate")
      .unwrap();
    assert_eq!(select_id(&pool, &cfg, "test", &[]), Some("b".to_string()));
    assert_eq!(pool.selector(&cfg).peek(POOL, &[]).unwrap().map(|option| option.id), Some("b".to_string()));

    pool
      .availability()
      .record("b", None, backend_availability::REASON_ACCESS_REFUSED, "Failed to authenticate")
      .unwrap();
    let message = pool.selector(&cfg).exhausted_message(POOL).unwrap();
    assert!(message.contains("- a: access_refused, recovers at "), "{message}");
    assert!(message.contains("- b: access_refused, recovers at "), "{message}");
  }

  #[test]
  fn exhausted_message_lists_each_member_and_why_it_is_out() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(&["a", "b"]);
    let pool = pool_at(home.path());
    record(&pool, "a", Some(UtcDateTime::parse("2030-01-02T03:04:00Z").unwrap()));
    let message = pool.selector(&cfg).exhausted_message(POOL).unwrap();
    assert!(message.starts_with("pool 'light' has no selectable member:\n"), "{message}");
    assert!(message.contains("\n- a: ") && message.contains("recovers at 2030-01-02T03:04"), "{message}");
    assert!(message.contains("(synthetic quota error)"), "{message}");
    assert!(message.ends_with("\n- b: not selectable"), "{message}");
  }

  #[test]
  fn callers_share_the_out_records_and_rotation_position() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(&["a", "b", "c"]);
    let pool = pool_at(home.path());
    assert_eq!(select_id(&pool, &cfg, "caller-a", &[]), Some("a".to_string()));
    assert_eq!(select_id(&pool, &cfg, "caller-b", &[]), Some("b".to_string()));
    record(&pool, "a", Some(UtcDateTime::parse("2030-01-02T03:04:00Z").unwrap()));
    assert_eq!(select_id(&pool, &cfg, "caller-b", &[]), Some("c".to_string()));
    assert_eq!(select_id(&pool, &cfg, "caller-a", &[]), Some("b".to_string()));
  }

  #[test]
  fn peek_names_the_member_select_would_pick_without_moving_the_rotation() {
    let cfg = config(&["a", "b", "c"]);
    let pool = pool();
    assert_eq!(pool.selector(&cfg).peek(POOL, &[]).unwrap().map(|option| option.id), Some("a".to_string()));
    assert_eq!(pool.selector(&cfg).peek(POOL, &[]).unwrap().map(|option| option.id), Some("a".to_string()));
    assert_eq!(select_id(&pool, &cfg, "test", &[]), Some("a".to_string()));
    assert_eq!(pool.selector(&cfg).peek(POOL, &[]).unwrap().map(|option| option.id), Some("b".to_string()));
  }

  #[test]
  fn a_route_order_takes_the_first_available_member() {
    let cfg = config(&["a", "b", "c"]);
    let selected = pool().selector(&cfg).ordered(&["b".to_string(), "a".to_string()], &[]).unwrap();
    assert_eq!(selected.map(|option| option.id), Some("b".to_string()));
  }

  #[test]
  fn a_route_order_skips_out_and_excluded_members() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(&["a", "b", "c"]);
    let pool = pool_at(home.path());
    record(&pool, "b", None);
    assert_eq!(
      pool.selector(&cfg).ordered(&["b".to_string(), "a".to_string()], &[]).unwrap().map(|option| option.id),
      Some("a".to_string())
    );
    assert_eq!(pool.selector(&cfg).ordered(&["b".to_string(), "a".to_string()], &["a".to_string()]).unwrap(), None);
  }

  #[test]
  fn a_route_order_shares_the_owner_state_with_selection() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(&["a", "b"]);
    let pool = pool_at(home.path());
    record(&pool, "a", None);
    assert_eq!(select_id(&pool, &cfg, "test", &[]), Some("b".to_string()));
    assert_eq!(
      pool.selector(&cfg).ordered(&["a".to_string(), "b".to_string()], &[]).unwrap().map(|option| option.id),
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
      "API Error: 402 Insufficient Balance",
    ] {
      assert!(is_quota_error(text), "{text}");
    }
    for text in [
      "syntax error in the diff",
      "connection reset by peer",
      // A request id whose hex digits contain 429 is not a status code.
      "Error code: 400 - invalid request (request_id: 0b429c7d-1e2f-4a5b-8c9d-0e1f2a3b4c5d)",
      "Error code: 400 - invalid request (request id req_429ab)",
    ] {
      assert!(!is_quota_error(text), "{text}");
    }
  }

  #[test]
  fn a_spent_quota_text_is_one_verdict_and_a_temporary_text_is_not() {
    let aigw_402 = "API Error: 402 litellm.BadRequestError: OpenAIException - Insufficient Balance";
    assert!(is_spent_quota_error(aigw_402));
    assert!(!is_spent_quota_error("500 Internal Server Error: quota exhausted"));
    assert!(!is_spent_quota_error("Error code: 400 - invalid request"));
    let result = json!({"type": "result", "is_error": true, "api_error_status": 402, "result": aigw_402});
    assert!(is_spent_quota_result(&result));
    assert!(!is_spent_quota_result(&json!({"type": "result", "is_error": true, "result": null})));
  }

  #[test]
  fn a_refusal_text_is_its_own_member_failure_and_a_request_error_is_not() {
    // The text of the review run that ended on the aigw gateway's model-access refusal.
    let refused = "Failed to authenticate. API Error: 401 Not allowed to access model due to tags configuration. \
                   Passed model=deepseek-flash and tags=['deepseek']";
    assert_eq!(classify_member_failure(refused, None), MemberFailure::AccessRefused);
    assert_eq!(
      classify_member_failure("API Error: 403 Forbidden: model access denied", None),
      MemberFailure::AccessRefused
    );
    assert_eq!(classify_member_failure("Failed to authenticate", None), MemberFailure::AccessRefused);

    for text in [
      "400 Bad Request",
      "API Error: 400 Bad Request",
      "Error code: 400 - invalid request (request_id: 0b401c7d-1e2f-4a5b-8c9d-0e1f2a3b4c5d)",
      "Error code: 400 - invalid request (request id req_401ab)",
      "Error code: 403 - forbidden by the proxy",
    ] {
      assert_eq!(classify_member_failure(text, None), MemberFailure::Other, "{text}");
    }
  }

  #[test]
  fn the_shared_classification_keeps_each_member_failure_it_met_before() {
    assert_eq!(classify_member_failure("API Error: 402 Insufficient Balance", None), MemberFailure::Quota);
    assert_eq!(
      classify_member_failure("Claude account pool has no available account", Some(true)),
      MemberFailure::Quota
    );
    assert_eq!(classify_member_failure("500 Internal Server Error: quota exhausted", None), MemberFailure::Other);
    assert_eq!(classify_member_failure("at capacity", Some(false)), MemberFailure::Other);
    assert_eq!(classify_member_failure("the build broke", None), MemberFailure::Other);
    assert_eq!(MemberFailure::Quota.availability_reason(), Some(backend_availability::REASON_QUOTA_ERROR));
    assert_eq!(MemberFailure::AccessRefused.availability_reason(), Some(backend_availability::REASON_ACCESS_REFUSED));
    assert_eq!(MemberFailure::Other.availability_reason(), None);
  }

  #[test]
  fn selecting_an_unknown_pool_id_fails() {
    let cfg = config(&["a"]);
    assert!(pool().selector(&cfg).select("missing", &[]).is_err());
  }

  #[test]
  fn a_routed_answer_names_exactly_one_id() {
    let cfg = config(&["a", "b"]);
    let answer =
      RouterAnswer { routed: true, backends: vec!["a".to_string(), "b".to_string()], reason: "two ids".to_string() };
    let error = pool().decide(&cfg, "implement", answer).unwrap_err();
    assert!(error.to_string().contains("exactly one backend id, got 2"), "{error}");
  }

  #[test]
  fn a_routed_pool_id_runs_through_the_pool_and_a_cooling_option_takes_the_default_path() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(&["a", "b"]);
    let pool = pool_at(home.path());
    let routed = pool
      .decide(
        &cfg,
        "implement",
        RouterAnswer { routed: true, backends: vec![POOL.to_string()], reason: "pool".to_string() },
      )
      .unwrap();
    assert!(matches!(routed.target, RouteTarget::Backend(ref id) if id == POOL), "{:?}", routed.target);

    record(&pool, "b", Some(UtcDateTime::parse("2030-01-02T03:04:00Z").unwrap()));
    let cooling = pool
      .decide(
        &cfg,
        "implement",
        RouterAnswer { routed: true, backends: vec!["b".to_string()], reason: "option".to_string() },
      )
      .unwrap();
    assert!(matches!(cooling.target, RouteTarget::NotRouted), "{:?}", cooling.target);
    assert!(cooling.route.reason.ends_with("b cooling; ran on the default path"), "{}", cooling.route.reason);
  }

  #[test]
  fn three_workers_routed_to_a_rotate_pool_take_its_three_members_in_turn() {
    let home = tempfile::tempdir().unwrap();
    let cfg = config(&["a", "b", "c"]);
    let pool = pool_at(home.path());
    let picked = (0..3)
      .map(|_| {
        let routed = pool
          .decide(
            &cfg,
            "implement",
            RouterAnswer { routed: true, backends: vec![POOL.to_string()], reason: "routine".to_string() },
          )
          .unwrap();
        assert!(matches!(routed.target, RouteTarget::Backend(ref id) if id == POOL), "{:?}", routed.target);
        select_id(&pool, &cfg, "worker", &[]).unwrap()
      })
      .collect::<Vec<_>>();
    assert_eq!(picked, ["a", "b", "c"]);
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn router_returns_its_order_and_reason() {
    let directory = tempfile::tempdir().unwrap();
    let command = router_command(
      &directory,
      "cat >/dev/null\nprintf '%s' '{\"routed\":true,\"backends\":[\"b\",\"a\"],\"reason\":\"ranked\"}'",
      &[],
    );
    let mut cfg = config(&["a", "b"]);
    configured_router(&mut cfg, command);

    let answer =
      pool().route(&cfg, "implement", "design task", &["a".to_string(), "b".to_string()], Some("a")).await.unwrap();
    assert_eq!(
      answer,
      RouterAnswer { routed: true, backends: vec!["b".to_string(), "a".to_string()], reason: "ranked".to_string() }
    );
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn router_receives_role_text_session_backend_and_each_cooling_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let capture = directory.path().join("stdin.json");
    let capture_text = capture.to_string_lossy().into_owned();
    let command = router_command(
      &directory,
      "cat > \"$1\"\nprintf '%s' '{\"routed\":true,\"backends\":[],\"reason\":\"captured\"}'",
      &[&capture_text],
    );
    let mut cfg = config(&["a", "b"]);
    configured_router(&mut cfg, command);
    let home = tempfile::tempdir().unwrap();
    let pool = pool_at(home.path());
    record(&pool, "a", Some(UtcDateTime::parse("2030-01-02T03:04:00Z").unwrap()));
    record(&pool, "b", Some(UtcDateTime::parse("2000-01-02T03:04:00Z").unwrap()));

    let answer =
      pool.route(&cfg, "thread", "untrimmed thread", &["a".to_string(), "b".to_string()], Some("b")).await.unwrap();
    assert_eq!(answer.reason, "captured");
    let request: Value = serde_json::from_slice(&fs::read(capture).unwrap()).unwrap();
    assert_eq!(request["role"], "thread");
    assert_eq!(request["text"], "untrimmed thread");
    assert_eq!(request["session_backend"], "b");
    assert_eq!(
      request["candidates"],
      serde_json::json!([
        {"id": "a", "cooling_until": "2030-01-02T03:04:00+00:00"},
        {"id": "b", "cooling_until": null},
      ])
    );
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn router_failures_surface_start_exit_contract_unknown_id_and_timeout() {
    let pool = pool();
    let candidates = ["a".to_string(), "b".to_string()];

    let missing = pool
      .route_command(
        &["/synthetic/missing-router".to_string()],
        router_payload(),
        &candidates,
        StdDuration::from_secs(1),
      )
      .await
      .unwrap_err();
    assert!(missing.to_string().contains("missing-router"));

    let directory = tempfile::tempdir().unwrap();
    let command = router_command(&directory, "cat >/dev/null\nprintf '%s' 'router boom' >&2\nexit 3", &[]);
    let exited =
      pool.route_command(&command, router_payload(), &candidates, StdDuration::from_secs(1)).await.unwrap_err();
    assert!(exited.to_string().contains("exited 3") && exited.to_string().contains("router boom"));

    let command = router_command(&directory, "cat >/dev/null\nprintf '%s' 'not json'", &[]);
    let malformed =
      pool.route_command(&command, router_payload(), &candidates, StdDuration::from_secs(1)).await.unwrap_err();
    assert!(malformed.to_string().contains("stdout outside the router contract"));

    // An answer without the routed flag is the old form, and the contract names the flag.
    let command =
      router_command(&directory, "cat >/dev/null\nprintf '%s' '{\"backends\":[\"a\"],\"reason\":\"old form\"}'", &[]);
    let without_routed =
      pool.route_command(&command, router_payload(), &candidates, StdDuration::from_secs(1)).await.unwrap_err();
    assert!(without_routed.to_string().contains("routed: bool"));

    let command = router_command(
      &directory,
      "cat >/dev/null\nprintf '%s' '{\"routed\":false,\"backends\":[\"a\"],\"reason\":\"contradiction\"}'",
      &[],
    );
    let unrouted_with_backends =
      pool.route_command(&command, router_payload(), &candidates, StdDuration::from_secs(1)).await.unwrap_err();
    assert!(unrouted_with_backends.to_string().contains("routed false"));

    let command = router_command(
      &directory,
      "cat >/dev/null\nprintf '%s' '{\"routed\":true,\"backends\":[\"ghost\"],\"reason\":\"unknown\"}'",
      &[],
    );
    let unknown =
      pool.route_command(&command, router_payload(), &candidates, StdDuration::from_secs(1)).await.unwrap_err();
    assert!(unknown.to_string().contains("ghost"));

    let command = router_command(&directory, "cat >/dev/null\nsleep 5", &[]);
    let timeout =
      pool.route_command(&command, router_payload(), &candidates, StdDuration::from_millis(100)).await.unwrap_err();
    assert!(timeout.to_string().contains("timed out"));
  }

  #[test]
  fn router_timeout_stays_below_the_delegate_http_timeout() {
    assert_eq!(ROUTER_TIMEOUT_SECONDS, 25);
    const { assert!(ROUTER_TIMEOUT_SECONDS < 30) };
  }
}
