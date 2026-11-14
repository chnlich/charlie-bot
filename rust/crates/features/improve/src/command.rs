//! The loop's durable state: `sessions/<id>/loops/` with the active lock, one directory per loop, its state file and its
//! live goal and plan.

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

/// The state of one loop. The field order is the serialization order.
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
        formatter.write_str("An improve loop is already running for this session. Use charliebot improve stop first.")
      }
      Some(loop_id) => {
        write!(formatter, "Loop {loop_id} is already running for this session. Use charliebot improve stop first.")
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
  active_lock_path_in(&cfg.charliebot_home, session_id)
}

/// The active-lock path of one session under a profile home, for a reader that has no loaded config.
pub fn active_lock_path_in(home: &Path, session_id: &str) -> PathBuf {
  loops_dir_in(home, session_id).join(ACTIVE_LOCK_FILE)
}

/// One loop's directory.
pub fn loop_dir(cfg: &CharlieBotConfig, session_id: &str, loop_id: i64) -> PathBuf {
  loop_dir_in(&cfg.charliebot_home, session_id, loop_id)
}

/// One loop's directory under a profile home, for a reader that has no loaded config.
pub fn loop_dir_in(home: &Path, session_id: &str, loop_id: i64) -> PathBuf {
  loops_dir_in(home, session_id).join(loop_id.to_string())
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

/// The iteration report file under a loop directory. The worker prompt's `iteration_reports` section names the same
/// file, and every reader of a report goes through this path.
pub fn iteration_report_path(loop_dir: &Path, iteration: i64) -> PathBuf {
  loop_dir.join(format!("iter_{iteration:04}.md"))
}

/// The live goal of a loop directory. A missing `goal.md` fails loudly: there is no fallback to the state file's
/// startup snapshot.
pub fn read_loop_goal(loop_dir: &Path) -> anyhow::Result<String> {
  let goal_path = loop_dir.join(GOAL_FILE);
  if !goal_path.exists() {
    anyhow::bail!("improve loop goal file missing: {}", goal_path.display());
  }
  std::fs::read_to_string(&goal_path).with_context(|| format!("read {}", goal_path.display()))
}

/// The optional live plan of a loop directory, `None` when the file is absent.
pub fn read_loop_plan(loop_dir: &Path) -> anyhow::Result<Option<String>> {
  let plan_path = loop_dir.join(PLAN_FILE);
  if !plan_path.exists() {
    return Ok(None);
  }
  Ok(Some(std::fs::read_to_string(&plan_path).with_context(|| format!("read {}", plan_path.display()))?))
}

/// The loop id the session's active lock names. The lock holds the running loop's id and a newline.
pub fn read_active_lock_loop_id(lock: &Path) -> anyhow::Result<i64> {
  let text = std::fs::read_to_string(lock).with_context(|| format!("read {}", lock.display()))?;
  text.trim().parse::<i64>().with_context(|| format!("{} does not name a loop id", lock.display()))
}

/// One loop's controller signals, as a liveness reader outside the crate reads them.
#[derive(Debug)]
pub struct LoopController {
  /// The loop's status, as its state file spells it.
  pub status: String,
  /// The pid of the server process whose controller runs the loop.
  pub server_pid: i64,
}

/// One loop directory's controller signals, `None` when the directory holds no state file. A liveness reader outside
/// the crate — the switch helper's scan — takes the controller's signals only through this reader, so their
/// definitions stay here.
pub fn read_loop_controller(loop_dir: &Path) -> anyhow::Result<Option<LoopController>> {
  let Some(state) = read_loop_state_value(loop_dir)? else {
    return Ok(None);
  };
  let Some(server_pid) = state.get("server_pid").and_then(Value::as_i64) else {
    anyhow::bail!("{} has no server_pid for the controller", loop_dir.join(STATE_FILE).display());
  };
  let status = status_in_state(&state).with_context(|| format!("read {}", loop_dir.join(STATE_FILE).display()))?;
  Ok(Some(LoopController { status: status_text(status), server_pid }))
}

/// The loop status one state value holds, as `ImproveState` reads it: a missing `status` is the `LoopStatus` default,
/// and an unknown one fails.
fn status_in_state(state: &Value) -> anyhow::Result<LoopStatus> {
  #[derive(Deserialize)]
  struct StatusOnly {
    #[serde(default)]
    status: LoopStatus,
  }
  let StatusOnly { status } = serde_json::from_value(state.clone())?;
  Ok(status)
}

/// The status text the state file spells: the serialization of the status enum.
fn status_text(status: LoopStatus) -> String {
  serde_json::to_value(status)
    .expect("a loop status serializes")
    .as_str()
    .expect("a loop status serializes to text")
    .to_string()
}

/// The decoded state file at `path`, through the one reader of `state.json` in this module. The caller checks that the
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
/// is not checked against `ImproveState`, so a state file that does not match it still reads. The readers that need
/// single fields take them from it.
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

/// The loop's state file, written as the atomic swap the readers need: the server, the CLI and the startup pass read
/// it with no coordination.
pub fn save_loop_state(cfg: &CharlieBotConfig, session_id: &str, state: &ImproveState) -> anyhow::Result<()> {
  let path = state_path(cfg, session_id, state.loop_id);
  let parent = path.parent().expect("a state path has a loop directory");
  std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
  let text = serde_json::to_string_pretty(state).context("serialize the loop state")?;
  atomic_write_text(&path, &text).with_context(|| format!("write {}", path.display()))
}

/// The session's active-loop lock, removed when present.
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

/// A unique loop directory for the session, as its id and path.
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

/// The running loop of a session, if one exists. Loops are read in id order. The first running one wins.
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

/// The stopping of the session's running loop. Returns `false` when no loop runs.
pub fn stop_improve_loop(cfg: &CharlieBotConfig, session_id: &str) -> anyhow::Result<bool> {
  let Some(mut state) = find_running_loop(cfg, session_id)? else {
    return Ok(false);
  };
  state.status = LoopStatus::Stopped;
  save_loop_state(cfg, session_id, &state)?;
  Ok(true)
}

/// The resolved path, non-strictly: symlinks in the existing prefix are followed and
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
