//! The loop's durable state: `sessions/<id>/loops/` with the active lock, one directory per loop, its state file and its
//! live goal and plan (`src/features/improve/improve_command.py:33-242`, `:435-487`).

use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::fsutil::atomic_write_text;
use charliebot_core::timefmt::utc_now_iso;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The state file's name in a loop directory.
pub const STATE_FILE: &str = "state.json";
/// The live goal file: re-read at the start of every iteration.
pub const GOAL_FILE: &str = "goal.md";
/// The optional live plan file.
pub const PLAN_FILE: &str = "plan.md";
/// The session's active-loop lock, holding the running loop's id and a newline.
pub const ACTIVE_LOCK_FILE: &str = "active.lock";

/// The loop's lifecycle status, written to `state.json` under the name `status`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopStatus {
  #[default]
  Running,
  Stopped,
  Completed,
  Failed,
  Blocked,
  Interrupted,
}

/// The state of one loop (`improve_command.ImproveState`). The field order is the order `model_dump_json` writes.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ImproveState {
  pub loop_id: i64,
  pub goal: String,
  #[serde(default)]
  pub status: LoopStatus,
  pub work_branch: String,
  #[serde(default)]
  pub base_branch: Option<String>,
  pub repo_path: String,
  #[serde(default)]
  pub merge_back: bool,
  #[serde(default)]
  pub backend: Option<String>,
  #[serde(default)]
  pub model: Option<String>,
  /// True when the loop named no backend: each iteration takes a member from `backends.pool`.
  #[serde(default)]
  pub backend_from_pool: bool,
  pub created_at: String,
  /// The server process that owns this loop's controller.
  #[serde(default)]
  pub server_pid: Option<i64>,
  /// The error text the final report carries, written in the same save as the final status.
  #[serde(default)]
  pub end_error: Option<String>,
  /// The blocked loop's own iteration and reason, written in the same save as the `failed` status.
  #[serde(default)]
  pub blocked_iteration: Option<i64>,
  #[serde(default)]
  pub blocked_reason: Option<String>,
}

/// A second loop while one is running (`ImproveLoopAlreadyRunningError`).
#[derive(Debug)]
pub struct ImproveLoopAlreadyRunningError {
  pub loop_id: Option<i64>,
}

impl fmt::Display for ImproveLoopAlreadyRunningError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self.loop_id {
      None => {
        formatter.write_str("An improve loop is already running for this session. Use charliebot improve-stop first.")
      }
      Some(loop_id) => {
        write!(formatter, "Loop {loop_id} is already running for this session. Use charliebot improve-stop first.")
      }
    }
  }
}

impl std::error::Error for ImproveLoopAlreadyRunningError {}

/// The loops directory of one session.
pub fn loops_dir(cfg: &CharlieBotConfig, session_id: &str) -> PathBuf {
  loops_dir_in(&cfg.charliebot_home, session_id)
}

/// The loops directory of one session under a profile home, for a reader that has no loaded config.
pub fn loops_dir_in(home: &Path, session_id: &str) -> PathBuf {
  charliebot_core::config::sessions_dir_in(home).join(session_id).join("loops")
}

/// The active-lock path of one session.
pub fn active_lock_path(cfg: &CharlieBotConfig, session_id: &str) -> PathBuf {
  loops_dir(cfg, session_id).join(ACTIVE_LOCK_FILE)
}

/// One loop's directory.
pub fn loop_dir(cfg: &CharlieBotConfig, session_id: &str, loop_id: i64) -> PathBuf {
  loops_dir(cfg, session_id).join(loop_id.to_string())
}

fn state_path(cfg: &CharlieBotConfig, session_id: &str, loop_id: i64) -> PathBuf {
  loop_dir(cfg, session_id, loop_id).join(STATE_FILE)
}

/// The live goal file of a loop (the editable per-iteration goal).
pub fn loop_goal_path(cfg: &CharlieBotConfig, session_id: &str, loop_id: i64) -> PathBuf {
  loop_dir(cfg, session_id, loop_id).join(GOAL_FILE)
}

/// The optional live plan file of a loop.
pub fn loop_plan_path(cfg: &CharlieBotConfig, session_id: &str, loop_id: i64) -> PathBuf {
  loop_dir(cfg, session_id, loop_id).join(PLAN_FILE)
}

/// Read the live goal of a loop directory. A missing `goal.md` fails loudly: there is no fallback to the state file's
/// startup snapshot.
pub fn read_loop_goal(loop_dir: &Path) -> anyhow::Result<String> {
  let goal_path = loop_dir.join(GOAL_FILE);
  if !goal_path.exists() {
    anyhow::bail!("improve loop goal file missing: {}", goal_path.display());
  }
  std::fs::read_to_string(&goal_path).with_context(|| format!("read {}", goal_path.display()))
}

/// Read the optional live plan of a loop directory, `None` when the file is absent.
pub fn read_loop_plan(loop_dir: &Path) -> anyhow::Result<Option<String>> {
  let plan_path = loop_dir.join(PLAN_FILE);
  if !plan_path.exists() {
    return Ok(None);
  }
  Ok(Some(std::fs::read_to_string(&plan_path).with_context(|| format!("read {}", plan_path.display()))?))
}

/// Read and decode the state file at `path`, the one reader of `state.json` in this module. The caller checks that the
/// file exists, and the read and parse errors name the path.
fn read_state_file<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
  let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// The loop's state file, or `None` when it is missing.
pub fn load_loop_state(cfg: &CharlieBotConfig, session_id: &str, loop_id: i64) -> anyhow::Result<Option<ImproveState>> {
  let path = state_path(cfg, session_id, loop_id);
  if !path.exists() {
    return Ok(None);
  }
  Ok(Some(read_state_file(&path)?))
}

/// The state file of one loop directory as its JSON value, or `None` when the directory holds no state file. The value
/// is not checked against `ImproveState`, so a state file that does not match it still reads: the switch helper's scan
/// takes only the status and the controller's PID from it.
pub fn read_loop_state_value(loop_dir: &Path) -> anyhow::Result<Option<Value>> {
  let path = loop_dir.join(STATE_FILE);
  if !path.is_file() {
    return Ok(None);
  }
  Ok(Some(read_state_file(&path)?))
}

/// The loop's state file, which must exist.
pub fn require_loop_state(cfg: &CharlieBotConfig, session_id: &str, loop_id: i64) -> anyhow::Result<ImproveState> {
  load_loop_state(cfg, session_id, loop_id)?
    .ok_or_else(|| anyhow::anyhow!("missing loop state for session {session_id} loop {loop_id}"))
}

/// Write the loop's state file as the atomic swap the readers need: the server, the CLI and the startup pass read it
/// with no coordination.
pub fn save_loop_state(cfg: &CharlieBotConfig, session_id: &str, state: &ImproveState) -> anyhow::Result<()> {
  let path = state_path(cfg, session_id, state.loop_id);
  let parent = path.parent().expect("a state path has a loop directory");
  std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
  let text = serde_json::to_string_pretty(state).context("serialize the loop state")?;
  atomic_write_text(&path, &text).with_context(|| format!("write {}", path.display()))
}

/// Remove the session's active-loop lock, if present.
pub fn clear_active_loop_lock(cfg: &CharlieBotConfig, session_id: &str) -> anyhow::Result<()> {
  let active = active_lock_path(cfg, session_id);
  match std::fs::remove_file(&active) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(error).with_context(|| format!("remove {}", active.display())),
  }
}

/// The numeric loop directories under `loops_dir`, as `(path, loop id)`. Zero-padded names parse to their number, and
/// the path is the directory itself, so "007" stays at its own path.
fn numeric_loop_dirs(loops_dir: &Path) -> anyhow::Result<Vec<(PathBuf, i64)>> {
  let mut found = Vec::new();
  for entry in std::fs::read_dir(loops_dir).with_context(|| format!("read {}", loops_dir.display()))? {
    let child = entry?.path();
    if !child.is_dir() {
      continue;
    }
    let Some(name) = child.file_name().and_then(|name| name.to_str()) else { continue };
    if let Ok(loop_id) = name.parse::<i64>() {
      found.push((child, loop_id));
    }
  }
  Ok(found)
}

/// The loop ids whose directory holds a state file, in directory order.
fn state_loop_ids(loops_dir: &Path) -> anyhow::Result<Vec<i64>> {
  if !loops_dir.exists() {
    return Ok(Vec::new());
  }
  Ok(
    numeric_loop_dirs(loops_dir)?
      .into_iter()
      .filter(|(child, _)| child.join(STATE_FILE).exists())
      .map(|(_, loop_id)| loop_id)
      .collect(),
  )
}

/// The next loop id of a session: one past the largest numeric directory, or 1.
fn next_loop_id(loops_dir: &Path) -> anyhow::Result<i64> {
  if !loops_dir.exists() {
    return Ok(1);
  }
  let max_loop_id = numeric_loop_dirs(loops_dir)?.into_iter().map(|(_, loop_id)| loop_id).max().unwrap_or(0);
  Ok(if max_loop_id > 0 { max_loop_id + 1 } else { 1 })
}

/// Create a unique loop directory for the session and return its id and path.
fn reserve_loop_dir(cfg: &CharlieBotConfig, session_id: &str) -> anyhow::Result<(i64, PathBuf)> {
  let loops = loops_dir(cfg, session_id);
  std::fs::create_dir_all(&loops).with_context(|| format!("create {}", loops.display()))?;
  loop {
    let loop_id = next_loop_id(&loops)?;
    let path = loops.join(loop_id.to_string());
    match std::fs::create_dir(&path) {
      Ok(()) => return Ok((loop_id, path)),
      Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
      Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
    }
  }
}

/// The running loop of a session, if one exists. Loops are read in id order; the first running one wins.
pub fn find_running_loop(cfg: &CharlieBotConfig, session_id: &str) -> anyhow::Result<Option<ImproveState>> {
  let mut ids = state_loop_ids(&loops_dir(cfg, session_id))?;
  ids.sort_unstable();
  for loop_id in ids {
    if let Some(state) = load_loop_state(cfg, session_id, loop_id)?
      && state.status == LoopStatus::Running
    {
      return Ok(Some(state));
    }
  }
  Ok(None)
}

/// Mark the session's running loop stopped. Returns `false` when no loop runs.
pub fn stop_improve_loop(cfg: &CharlieBotConfig, session_id: &str) -> anyhow::Result<bool> {
  let Some(mut state) = find_running_loop(cfg, session_id)? else {
    return Ok(false);
  };
  state.status = LoopStatus::Stopped;
  save_loop_state(cfg, session_id, &state)?;
  Ok(true)
}

/// Resolve a path the way Python's non-strict `Path.resolve()` does: symlinks in the existing prefix are followed and
/// the missing tail is kept as written.
pub fn resolve_path(path: &Path) -> anyhow::Result<PathBuf> {
  let absolute = std::path::absolute(path).with_context(|| format!("absolute path of {}", path.display()))?;
  let mut existing = absolute.clone();
  let mut tail = Vec::new();
  while !existing.exists() {
    let Some(name) = existing.file_name().map(|name| name.to_os_string()) else { break };
    tail.push(name);
    existing.pop();
  }
  let mut resolved = existing.canonicalize().with_context(|| format!("resolve {}", existing.display()))?;
  for name in tail.iter().rev() {
    match Path::new(name).components().next() {
      Some(Component::Normal(part)) => resolved.push(part),
      _ => anyhow::bail!("cannot resolve {}", path.display()),
    }
  }
  Ok(resolved)
}

/// The inputs of one reservation (`reserve_loop_state`'s parameters).
pub struct Reservation<'a> {
  pub goal: &'a str,
  pub work_branch: &'a str,
  pub repo_path: &'a str,
  pub plan: Option<&'a str>,
  pub base_branch: Option<&'a str>,
  pub merge_back: bool,
  pub resolved_backend: &'a str,
  pub resolved_model: &'a str,
  pub backend_from_pool: bool,
}

/// Reserve a loop id and persist its running state before any background work starts. The active lock is created
/// exclusively, so a second reservation fails with the running loop's id.
pub fn reserve_loop_state(
  cfg: &CharlieBotConfig,
  session_id: &str,
  reservation: &Reservation<'_>,
) -> anyhow::Result<ImproveState> {
  let loops = loops_dir(cfg, session_id);
  std::fs::create_dir_all(&loops).with_context(|| format!("create {}", loops.display()))?;
  let active = active_lock_path(cfg, session_id);
  if let Err(error) = std::fs::OpenOptions::new().write(true).create_new(true).open(&active) {
    if error.kind() != io::ErrorKind::AlreadyExists {
      return Err(error).with_context(|| format!("create {}", active.display()));
    }
    let loop_id = find_running_loop(cfg, session_id)?.map(|state| state.loop_id);
    return Err(ImproveLoopAlreadyRunningError { loop_id }.into());
  }
  let reserved = reserve_loop_dir(cfg, session_id).and_then(|(loop_id, loop_dir)| {
    std::fs::write(loop_dir.join(GOAL_FILE), reservation.goal)
      .with_context(|| format!("write {}", loop_dir.join(GOAL_FILE).display()))?;
    if let Some(plan) = reservation.plan {
      std::fs::write(loop_dir.join(PLAN_FILE), plan)
        .with_context(|| format!("write {}", loop_dir.join(PLAN_FILE).display()))?;
    }
    let state = ImproveState {
      loop_id,
      goal: reservation.goal.to_string(),
      status: LoopStatus::Running,
      work_branch: reservation.work_branch.to_string(),
      base_branch: reservation.base_branch.map(str::to_string),
      repo_path: resolve_path(Path::new(reservation.repo_path))?.to_string_lossy().into_owned(),
      merge_back: reservation.merge_back,
      backend: non_empty(reservation.resolved_backend),
      model: non_empty(reservation.resolved_model),
      backend_from_pool: reservation.backend_from_pool,
      created_at: utc_now_iso(),
      server_pid: Some(i64::from(std::process::id())),
      end_error: None,
      blocked_iteration: None,
      blocked_reason: None,
    };
    save_loop_state(cfg, session_id, &state)?;
    std::fs::write(&active, format!("{loop_id}\n")).with_context(|| format!("write {}", active.display()))?;
    Ok(state)
  });
  if reserved.is_err() {
    clear_active_loop_lock(cfg, session_id)?;
  }
  reserved
}

fn non_empty(text: &str) -> Option<String> {
  (!text.is_empty()).then(|| text.to_string())
}
