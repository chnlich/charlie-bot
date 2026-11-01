//! Delayed and watched task triggers.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, anyhow};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, TaggedVariant};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_core::{event_types, ssh, text_repr};
use charliebot_registry::context::FeatureContext;
use charliebot_store::metadata::UtcDateTime;
use charliebot_store::trigger_files::{
  self, LOCAL_PID_KIND, PendingTrigger, REMOTE_PID_KIND, SLURM_JOB_KIND, TriggerStatus, WatchTarget, fresh_uuid,
};
use charliebot_store::{StoreError, layout};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use time::format_description::well_known::Rfc3339;
use tokio::io::{Interest, unix::AsyncFd};
use tokio::process::Command;
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, sleep_until, timeout};

/// The maximum number of pending triggers for one session.
pub const MAX_PENDING_TRIGGERS: usize = 5;

const REMOTE_PROBE_INTERVALS: [u64; 6] = [10, 20, 40, 80, 160, 320];
const REMOTE_PROBE_PLATEAU_SECONDS: u64 = 600;
const REMOTE_PROBE_NOISE_MAX_SECONDS: u64 = 10;
const SACCT_POLL_INTERVAL_SECONDS: u64 = 30;
const REMOTE_SACCT_UNREACHABLE_GRACE_SECONDS: u64 = 900;
const DORMANCY_CHECK_SECONDS: u64 = 60;
const SSH_OVERALL_TIMEOUT_SECONDS: u64 = 60;
const MAX_TRIGGER_MESSAGE_CHARS: usize = 200;

/// The answer one remote `kill -0` probe gives about a watched pid.
///
/// The probe script echoes [`PROBE_ALIVE_WORD`] or [`PROBE_DEAD_WORD`]. A probe that could not start, or whose output
/// names neither word, answers `Error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeStatus {
  Alive,
  Dead,
  Error,
}

/// The word the remote `kill -0` script echoes while the pid lives.
const PROBE_ALIVE_WORD: &str = "ALIVE";
/// The word the remote `kill -0` script echoes once the pid is gone.
const PROBE_DEAD_WORD: &str = "DEAD";
/// The report word for a probe that gave no answer.
const PROBE_ERROR_WORD: &str = "ERROR";

impl std::fmt::Display for ProbeStatus {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(match self {
      Self::Alive => PROBE_ALIVE_WORD,
      Self::Dead => PROBE_DEAD_WORD,
      Self::Error => PROBE_ERROR_WORD,
    })
  }
}

static PROGRAM_AVAILABILITY: OnceLock<StdMutex<HashMap<String, bool>>> = OnceLock::new();
#[cfg(test)]
static PROGRAM_SEARCHES: AtomicUsize = AtomicUsize::new(0);
const LOCAL_PID_FIELDS: &[Field] = &[
  Field::required("kind", FieldType::Literal(&[LOCAL_PID_KIND])),
  Field::required("pid", FieldType::Integer),
  Field::optional("exit_file", FieldType::NullableString),
  Field::optional("log", FieldType::NullableString),
];
const LOCAL_PID: BodyModel = BodyModel::optional(LOCAL_PID_FIELDS);
const REMOTE_PID_FIELDS: &[Field] = &[
  Field::required("kind", FieldType::Literal(&[REMOTE_PID_KIND])),
  Field::required("host", FieldType::String),
  Field::required("pid", FieldType::Integer),
  Field::optional("exit_file", FieldType::NullableString),
  Field::optional("log", FieldType::NullableString),
];
const REMOTE_PID: BodyModel = BodyModel::optional(REMOTE_PID_FIELDS);
const SLURM_JOB_FIELDS: &[Field] = &[
  Field::required("kind", FieldType::Literal(&[SLURM_JOB_KIND])),
  Field::optional("host", FieldType::NullableString),
  Field::required("job_id", FieldType::Integer),
];
const SLURM_JOB: BodyModel = BodyModel::optional(SLURM_JOB_FIELDS);
const WATCH_TARGET_VARIANTS: &[TaggedVariant] = &[
  TaggedVariant { tag: LOCAL_PID_KIND, model: &LOCAL_PID },
  TaggedVariant { tag: REMOTE_PID_KIND, model: &REMOTE_PID },
  TaggedVariant { tag: SLURM_JOB_KIND, model: &SLURM_JOB },
];
const SCHEDULE_TRIGGER_BODY_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required("delay_seconds", FieldType::Integer),
  Field::required("message", FieldType::String).max_length(MAX_TRIGGER_MESSAGE_CHARS),
  Field::optional(
    "watch_targets",
    FieldType::NullableTaggedUnionList { discriminator: "kind", variants: WATCH_TARGET_VARIANTS },
  ),
];
const SCHEDULE_TRIGGER_BODY: BodyModel = BodyModel::required(SCHEDULE_TRIGGER_BODY_FIELDS).forbid_extra();

const SLURM_ACTIVE_STATES: [&str; 7] =
  ["PENDING", "RUNNING", "CONFIGURING", "COMPLETING", "REQUEUED", "RESIZING", "SUSPENDED"];
const SLURM_TERMINAL_STATES: [&str; 11] = [
  "COMPLETED",
  "FAILED",
  "CANCELLED",
  "TIMEOUT",
  "OUT_OF_MEMORY",
  "NODE_FAIL",
  "BOOT_FAIL",
  "DEADLINE",
  "PREEMPTED",
  "REVOKED",
  "SPECIAL_EXIT",
];

/// The request body for `POST /api/internal/schedule-trigger`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleTriggerRequest {
  /// The session that receives the scheduled input.
  pub session_id: String,
  /// Seconds from registration until the deadline.
  pub delay_seconds: i64,
  /// The short wake label.
  pub message: String,
  /// Targets that must all finish before the trigger fires.
  #[serde(default)]
  pub watch_targets: Option<Vec<WatchTarget>>,
}

/// A trigger creation refusal, with the status the schedule route answers for it.
#[derive(Debug)]
pub enum TriggerCreateError {
  /// A remote PID or SLURM verify-on-create probe refused the request.
  RemoteVerify(String),
  /// The target task is dormant.
  Archived(String),
  /// The session already has the maximum number of pending triggers.
  PendingLimit(String),
  /// A runtime prerequisite such as pidfd or local `sacct` is unavailable.
  Runtime(String),
  /// A filesystem, store, or serialization operation failed.
  Failure(anyhow::Error),
}

impl std::fmt::Display for TriggerCreateError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::RemoteVerify(message) | Self::Archived(message) | Self::PendingLimit(message) | Self::Runtime(message) => {
        f.write_str(message)
      }
      Self::Failure(error) => write!(f, "{error:#}"),
    }
  }
}

impl std::error::Error for TriggerCreateError {}

/// The runtime that persists, recovers, waits for, and fires triggers.
pub struct TriggerManager {
  features: Arc<FeatureContext>,
  tasks: StdMutex<HashMap<String, tokio::task::JoinHandle<()>>>,
  create_locks: StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
  probe_programs: ProbePrograms,
  ssh_control_dir: Option<PathBuf>,
  timing: Timing,
}

#[derive(Clone)]
struct ProbePrograms {
  ssh: String,
  sacct: String,
}

#[derive(Clone)]
struct Timing {
  remote_intervals: Vec<Duration>,
  remote_plateau: Duration,
  sacct_interval: Duration,
  remote_sacct_grace: Duration,
  dormancy_interval: Duration,
}

impl TriggerManager {
  /// Construct the one manager held in startup state.
  pub fn new(features: Arc<FeatureContext>) -> TriggerManager {
    TriggerManager {
      features,
      tasks: StdMutex::new(HashMap::new()),
      create_locks: StdMutex::new(HashMap::new()),
      probe_programs: ProbePrograms { ssh: "ssh".to_string(), sacct: "sacct".to_string() },
      ssh_control_dir: None,
      timing: Timing {
        remote_intervals: REMOTE_PROBE_INTERVALS.into_iter().map(Duration::from_secs).collect(),
        remote_plateau: Duration::from_secs(REMOTE_PROBE_PLATEAU_SECONDS),
        sacct_interval: Duration::from_secs(SACCT_POLL_INTERVAL_SECONDS),
        remote_sacct_grace: Duration::from_secs(REMOTE_SACCT_UNREACHABLE_GRACE_SECONDS),
        dormancy_interval: Duration::from_secs(DORMANCY_CHECK_SECONDS),
      },
    }
  }

  /// Create a trigger, verify remote targets, enforce the session limit, persist the record, and start its waiter.
  #[allow(clippy::too_many_arguments)]
  pub async fn create_trigger(
    self: &Arc<Self>,
    session_id: &str,
    delay_seconds: i64,
    message: &str,
    watch_targets: Option<Vec<WatchTarget>>,
    probe_out: Option<&mut Map<String, Value>>,
    created_at: Option<UtcDateTime>,
    enforce_pending_limit: bool,
  ) -> Result<PendingTrigger, TriggerCreateError> {
    let targets = watch_targets.unwrap_or_default();
    let dormancy = self.dormancy_reason(session_id).await.map_err(TriggerCreateError::Failure)?;
    if let Some(dormancy) = dormancy {
      return Err(TriggerCreateError::Archived(format!(
        "session {session_id} is archived ({dormancy}); trigger rejected"
      )));
    }

    if targets.iter().any(|target| matches!(target, WatchTarget::LocalPid { .. })) && !pidfd_available() {
      return Err(TriggerCreateError::Runtime(
        "pidfd_open unavailable: need Linux 5.3+ with kernel pidfd_open support".to_string(),
      ));
    }
    if targets.iter().any(|target| matches!(target, WatchTarget::SlurmJob { host: None, .. }))
      && !program_available(&self.probe_programs.sacct)
    {
      return Err(TriggerCreateError::Runtime(
        "sacct unavailable: cannot watch a SLURM job on a host without slurm".to_string(),
      ));
    }

    let remote_pids = targets.iter().filter_map(remote_pid).collect::<Vec<_>>();
    if !remote_pids.is_empty() {
      self.verify_remote_pids(&remote_pids).await.map_err(TriggerCreateError::RemoteVerify)?;
    }

    let remote_slurm = targets.iter().filter_map(slurm_job).filter(|target| target.host.is_some()).collect::<Vec<_>>();
    let observed = if remote_slurm.is_empty() {
      Map::new()
    } else {
      self.verify_remote_slurm(&remote_slurm).await.map_err(TriggerCreateError::RemoteVerify)?
    };
    if let Some(probe_out) = probe_out {
      probe_out.extend(observed);
    }

    let fire_at = deadline_after(delay_seconds).map_err(TriggerCreateError::Failure)?;
    let create_lock = {
      let mut locks = self.create_locks.lock().expect("trigger create lock map is not poisoned");
      Arc::clone(locks.entry(session_id.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))))
    };
    let _guard = create_lock.lock().await;

    if enforce_pending_limit {
      let pending_count = self
        .list_triggers(session_id)
        .await
        .map_err(TriggerCreateError::Failure)?
        .iter()
        .filter(|trigger| trigger.status == TriggerStatus::Pending)
        .count();
      if pending_count + 1 > MAX_PENDING_TRIGGERS {
        return Err(TriggerCreateError::PendingLimit(format!(
          "session {session_id} has {pending_count} pending triggers (limit {MAX_PENDING_TRIGGERS}); watch several targets with one trigger: --watch A --watch B"
        )));
      }
    }

    let trigger = PendingTrigger {
      id: fresh_uuid(),
      session_id: session_id.to_string(),
      fire_at,
      message: message.to_string(),
      created_at: created_at.unwrap_or_else(UtcDateTime::now),
      status: TriggerStatus::Pending,
      fired_at: None,
      watch_targets: targets,
      fire_reason: None,
    };
    self.save_trigger(&trigger).await.map_err(TriggerCreateError::Failure)?;
    self.start_task(trigger.clone());
    tracing::info!(
      target: "charliebot_triggers",
      trigger_id = %trigger.id,
      session = session_id,
      fire_at = %trigger.fire_at,
      "trigger_created"
    );
    Ok(trigger)
  }

  async fn verify_remote_pids(&self, targets: &[RemotePid]) -> Result<(), String> {
    let probes = targets.iter().map(|target| self.ssh_probe_pid(target));
    let results = join_all(probes).await;
    let mut bad = Vec::new();
    for (target, (status, raw)) in targets.iter().zip(results) {
      if status != ProbeStatus::Alive {
        bad.push(format!("{}:{} -> {} ({})", target.host, target.pid, status, text_repr::str_repr(raw.trim())));
      }
    }
    if bad.is_empty() {
      Ok(())
    } else {
      Err(format!("verify-on-create failed for remote watch target(s): {}", bad.join("; ")))
    }
  }

  async fn verify_remote_slurm(&self, targets: &[SlurmJob]) -> Result<Map<String, Value>, String> {
    let mut by_host: HashMap<String, Vec<i64>> = HashMap::new();
    for target in targets {
      by_host.entry(target.host.clone().expect("remote SLURM targets have hosts")).or_default().push(target.job_id);
    }
    let mut hosts = by_host.keys().cloned().collect::<Vec<_>>();
    hosts.sort();
    let probes = hosts.iter().map(|host| {
      let host = host.clone();
      let mut ids = by_host.get(&host).expect("the host was inserted").clone();
      ids.sort_unstable();
      async move { self.probe_sacct(&ids, "verify-on-create", Some(&host)).await }
    });
    let results = join_all(probes).await;
    let mut observed = Map::new();
    let mut bad = Vec::new();
    for (host, (states, error)) in hosts.iter().zip(results) {
      if let Some(error) = error {
        bad.push(format!("{host} -> {error}"));
        continue;
      }
      let mut ids = by_host.get(host).expect("the host was inserted").clone();
      ids.sort_unstable();
      for job_id in ids {
        let key = slurm_label(Some(host), job_id);
        let state = states.get(&job_id).map_or("not-yet-registered", |row| row.0.as_str());
        observed.insert(key, Value::String(state.to_string()));
      }
    }
    if bad.is_empty() {
      Ok(observed)
    } else {
      Err(format!("verify-on-create failed for remote SLURM host(s): {}", bad.join("; ")))
    }
  }

  /// Read one session's trigger files in newest-first creation order.
  pub async fn list_triggers(&self, session_id: &str) -> anyhow::Result<Vec<PendingTrigger>> {
    let directory = self.triggers_dir(session_id);
    match tokio::fs::metadata(&directory).await {
      Ok(_) => {}
      Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
      Err(error) => return Err(error).with_context(|| format!("stat {}", directory.display())),
    }
    let directory_for_walk = directory.clone();
    let files = tokio::task::spawn_blocking(move || trigger_files::iter_trigger_file_stats(directory_for_walk))
      .await
      .context("trigger-file stat walk task failed")??;
    let mut triggers = Vec::new();
    for (path, _stat) in files {
      match self.load_trigger_path(&path).await {
        Ok(trigger) => triggers.push(trigger),
        Err(error) => {
          tracing::warn!(target: "charliebot_triggers", path = %path.display(), error = %error, "trigger_load_failed")
        }
      }
    }
    triggers.sort_by_key(|trigger| std::cmp::Reverse(trigger.created_at));
    Ok(triggers)
  }

  /// Mark one pending trigger cancelled and stop its waiter.
  pub async fn cancel_trigger(&self, session_id: &str, trigger_id: &str) -> anyhow::Result<()> {
    let mut trigger = self.load_trigger(session_id, trigger_id).await?;
    if trigger.status != TriggerStatus::Pending {
      return Ok(());
    }
    trigger.status = TriggerStatus::Cancelled;
    self.save_trigger(&trigger).await?;
    if let Some(task) = self.tasks.lock().expect("trigger task map is not poisoned").remove(trigger_id) {
      task.abort();
    }
    tracing::info!(target: "charliebot_triggers", trigger_id, session = session_id, "trigger_cancelled");
    Ok(())
  }

  /// Restart every pending trigger found under the configured sessions directory.
  pub async fn recover_pending(self: &Arc<Self>) -> anyhow::Result<()> {
    let stop = self.features.startup.stop();
    let sessions_dir = self.features.store.config().sessions_dir();
    let mut sessions = match tokio::fs::read_dir(&sessions_dir).await {
      Ok(entries) => entries,
      Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
      Err(error) => return Err(error).with_context(|| format!("read {}", sessions_dir.display())),
    };
    while let Some(entry) = sessions.next_entry().await? {
      if stop.is_cancelled() {
        return Ok(());
      }
      if !entry.file_type().await?.is_dir() {
        continue;
      }
      let recovery_started = Instant::now();
      let mut recovered = 0u64;
      let directory = layout::triggers_dir(&entry.path());
      let directory_for_walk = directory.clone();
      let files = match tokio::task::spawn_blocking(move || trigger_files::iter_trigger_file_stats(directory_for_walk))
        .await
        .context("trigger recovery stat task failed")?
      {
        Ok(files) => files,
        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
        Err(error) => return Err(error).with_context(|| format!("scan {}", directory.display())),
      };
      for (path, _stat) in files {
        if stop.is_cancelled() {
          return Ok(());
        }
        let trigger = match self.load_trigger_path(&path).await {
          Ok(trigger) => trigger,
          Err(error) => {
            tracing::warn!(
              target: "charliebot_triggers",
              path = %path.display(),
              error = %error,
              "trigger_recovery_load_failed"
            );
            continue;
          }
        };
        if stop.is_cancelled() {
          return Ok(());
        }
        if trigger.status == TriggerStatus::Pending {
          self.start_task(trigger.clone());
          recovered += 1;
          tracing::info!(
            target: "charliebot_triggers",
            trigger_id = %trigger.id,
            session = %trigger.session_id,
            "trigger_recovered"
          );
        }
      }
      log_slow_trigger_recovery(&entry.path(), recovered, recovery_started);
    }
    Ok(())
  }

  fn start_task(self: &Arc<Self>, trigger: PendingTrigger) {
    let id = trigger.id.clone();
    let task_id = id.clone();
    let manager = Arc::clone(self);
    let (start, wait) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
      let _ = wait.await;
      if let Err(error) = manager.wait_and_fire(trigger).await {
        tracing::error!(target: "charliebot_triggers", trigger_id = %id, error = %error, "trigger_wait_failed");
      }
      manager.tasks.lock().expect("trigger task map is not poisoned").remove(&id);
    });
    self.tasks.lock().expect("trigger task map is not poisoned").insert(task_id, handle);
    start.send(()).expect("the newly spawned trigger waiter is alive");
  }

  async fn dormancy_reason(&self, session_id: &str) -> anyhow::Result<Option<String>> {
    if self.features.store.load_meta(session_id).await?.is_none() {
      return Ok(None);
    }
    if self.features.store.task_state(session_id)? != "open" {
      return Ok(Some("target task is archived".to_string()));
    }
    Ok(None)
  }

  fn triggers_dir(&self, session_id: &str) -> PathBuf {
    layout::triggers_dir(&self.features.store.session_dir(session_id))
  }

  fn trigger_path(&self, session_id: &str, trigger_id: &str) -> PathBuf {
    trigger_files::trigger_path(&self.features.store.session_dir(session_id), trigger_id)
  }

  async fn load_trigger(&self, session_id: &str, trigger_id: &str) -> anyhow::Result<PendingTrigger> {
    self.load_trigger_path(&self.trigger_path(session_id, trigger_id)).await
  }

  async fn load_trigger_path(&self, path: &Path) -> anyhow::Result<PendingTrigger> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || trigger_files::read_trigger(&path)).await.expect("trigger read task completes")
  }

  async fn save_trigger(&self, trigger: &PendingTrigger) -> anyhow::Result<()> {
    let session_dir = self.features.store.session_dir(&trigger.session_id);
    let trigger = trigger.clone();
    tokio::task::spawn_blocking(move || trigger_files::write_trigger(&session_dir, &trigger))
      .await
      .expect("trigger publish task completes")
  }

  async fn ssh_probe_pid(&self, target: &RemotePid) -> (ProbeStatus, String) {
    let remote = format!("kill -0 {} 2>&1 && echo {PROBE_ALIVE_WORD} || echo {PROBE_DEAD_WORD}", target.pid);
    let command = match self.ssh_argv(&target.host, &remote) {
      Ok(command) => command,
      Err(error) => return (ProbeStatus::Error, format!("spawn failed: {error}")),
    };
    let result = run_probe_command(command, Some(Duration::from_secs(SSH_OVERALL_TIMEOUT_SECONDS)), "ssh").await;
    let output = match result {
      Ok(output) => output,
      Err(error) => return (ProbeStatus::Error, error),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{stdout}{stderr}");
    let last_line = stdout.trim().lines().last().unwrap_or_default().trim();
    match last_line {
      PROBE_ALIVE_WORD => (ProbeStatus::Alive, raw),
      PROBE_DEAD_WORD => (ProbeStatus::Dead, raw),
      _ => {
        let combined = raw.trim();
        let error = if combined.is_empty() {
          format!("ssh exit {}", output.status.code().map_or_else(|| "None".to_string(), |code| code.to_string()))
        } else {
          combined.to_string()
        };
        (ProbeStatus::Error, error)
      }
    }
  }

  fn ssh_argv(&self, host: &str, remote: &str) -> io::Result<Vec<String>> {
    let remote_argv = [remote];
    let mut argv = match &self.ssh_control_dir {
      Some(directory) => {
        ssh::ensure_control_dir(directory)?;
        ssh::argv(host, &remote_argv, directory)
      }
      None => ssh::ssh_cmd(host, &remote_argv)?,
    };
    argv[0] = self.probe_programs.ssh.clone();
    Ok(argv)
  }

  async fn probe_sacct(
    &self,
    job_ids: &[i64],
    trigger_id: &str,
    host: Option<&str>,
  ) -> (HashMap<i64, (String, String)>, Option<String>) {
    let ids = job_ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let sacct_args = ["sacct", "-j", &ids, "-X", "-n", "-P", "--format=JobID,State,ExitCode"];
    let (command, deadline, executable) = match host {
      Some(host) => {
        let remote = sacct_args.join(" ");
        match self.ssh_argv(host, &remote) {
          Ok(command) => (command, Some(Duration::from_secs(SSH_OVERALL_TIMEOUT_SECONDS)), "ssh"),
          Err(error) => return (HashMap::new(), Some(format!("spawn failed: {error}"))),
        }
      }
      None => (
        std::iter::once(self.probe_programs.sacct.clone())
          .chain(sacct_args.iter().skip(1).map(|arg| arg.to_string()))
          .collect(),
        None,
        "sacct",
      ),
    };
    let output = match run_probe_command(command, deadline, executable).await {
      Ok(output) => output,
      Err(error) => return (HashMap::new(), Some(error)),
    };
    if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
      tracing::warn!(
        target: "charliebot_triggers",
        trigger_id,
        host = host.unwrap_or(""),
        returncode = output.status.code().unwrap_or(-1),
        stderr,
        "sacct_probe_nonzero"
      );
      return (
        HashMap::new(),
        Some(format!(
          "sacct exit {}: {stderr}",
          output.status.code().map_or_else(|| "None".to_string(), |code| code.to_string())
        )),
      );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut states = HashMap::new();
    for line in stdout.lines() {
      let fields = line.trim().split('|').collect::<Vec<_>>();
      if fields.len() < 3 {
        continue;
      }
      let Ok(job_id) = fields[0].parse::<i64>() else {
        continue;
      };
      states.insert(job_id, (fields[1].trim().to_string(), fields[2].trim().to_string()));
    }
    (states, None)
  }

  async fn remote_read_exit_code(&self, target: &RemotePid) -> Option<String> {
    let remote = match remote_exit_read_command(target.exit_file.as_deref()?) {
      Ok(remote) => remote,
      Err(error) => {
        tracing::debug!(target: "charliebot_triggers", host = %target.host, error = %error, "ssh_exit_read_failed");
        return None;
      }
    };
    let command = match self.ssh_argv(&target.host, &remote) {
      Ok(command) => command,
      Err(error) => {
        tracing::debug!(target: "charliebot_triggers", host = %target.host, error = %error, "ssh_exit_read_failed");
        return None;
      }
    };
    match run_probe_command(command, Some(Duration::from_secs(SSH_OVERALL_TIMEOUT_SECONDS)), "ssh").await {
      Ok(output) if output.status.success() => {
        let content = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!content.is_empty()).then_some(content)
      }
      Ok(output) => {
        tracing::debug!(
          target: "charliebot_triggers",
          host = %target.host,
          returncode = output.status.code().unwrap_or(-1),
          "ssh_exit_read_failed"
        );
        None
      }
      Err(error) => {
        tracing::debug!(target: "charliebot_triggers", host = %target.host, error, "ssh_exit_read_failed");
        None
      }
    }
  }

  async fn local_finished_label(&self, target: &LocalPid, gone_at_start: bool) -> String {
    if target.exit_file.is_none() {
      return if gone_at_start { format!("{} (gone at start)", target.pid) } else { target.pid.to_string() };
    }
    let exit_file = target.exit_file.as_deref().expect("an exit file was checked");
    let code = match tokio::fs::read_to_string(exit_file).await {
      Ok(content) => {
        let content = content.trim().to_string();
        (!content.is_empty()).then_some(content)
      }
      Err(error) => {
        tracing::debug!(target: "charliebot_triggers", exit_file, error = %error, "local_exit_read_failed");
        None
      }
    };
    exit_state_label(&target.pid.to_string(), code.as_deref(), target.log.as_deref())
  }

  async fn remote_finished_label(&self, target: &RemotePid) -> String {
    let base = format!("{}:{}", target.host, target.pid);
    if target.exit_file.is_none() {
      return base;
    }
    let code = self.remote_read_exit_code(target).await;
    exit_state_label(&base, code.as_deref(), target.log.as_deref())
  }

  async fn wait_for_trigger(&self, trigger: &PendingTrigger) -> anyhow::Result<WaitOutcome> {
    let groups = wait_groups(&trigger.watch_targets);
    if groups.is_empty() {
      sleep(remaining_until(trigger.fire_at)).await;
      return Ok(WaitOutcome { reason: WaitReason::Timeout, finished: Vec::new(), still_alive: Vec::new() });
    }
    let waits = groups.into_iter().map(|group| async move {
      match group {
        WaitGroup::Local(targets) => self.wait_with_pidfd(trigger, &targets).await,
        WaitGroup::Remote(targets) => self.wait_with_remote_probe(trigger, &targets).await,
        WaitGroup::Slurm(targets) => self.wait_with_sacct(trigger, &targets).await,
      }
    });
    let results = join_all(waits).await;
    let mut finished = Vec::new();
    let mut still_alive = Vec::new();
    for result in results {
      let (group_finished, group_alive) = result?;
      finished.extend(group_finished);
      still_alive.extend(group_alive);
    }
    let reason = if still_alive.is_empty() { WaitReason::Completed } else { WaitReason::Timeout };
    Ok(WaitOutcome { reason, finished, still_alive })
  }

  async fn watch_dormancy(&self, session_id: &str) -> anyhow::Result<String> {
    loop {
      sleep(self.timing.dormancy_interval).await;
      if let Some(reason) = self.dormancy_reason(session_id).await? {
        return Ok(reason);
      }
    }
  }

  async fn wait_and_fire(self: &Arc<Self>, trigger: PendingTrigger) -> anyhow::Result<()> {
    let outcome = tokio::select! {
      biased;
      dormant = self.watch_dormancy(&trigger.session_id) => {
        let fresh = self.reload_pending(&trigger).await?;
        let Some(mut fresh) = fresh else { return Ok(()) };
        self.cancel_undeliverable(&mut fresh, dormant?).await?;
        return Ok(());
      }
      outcome = self.wait_for_trigger(&trigger) => outcome?,
    };
    let Some(mut fresh) = self.reload_pending(&trigger).await? else { return Ok(()) };
    let message = if fresh.watch_targets.is_empty() {
      format!("[Scheduled trigger fired] {}", fresh.message)
    } else {
      format!(
        "[Scheduled trigger fired | {}] {}{}",
        outcome.reason,
        fresh.message,
        format_suffix(outcome.reason, &outcome.finished, &outcome.still_alive)
      )
    };
    if self.features.store.load_meta(&fresh.session_id).await?.is_none() {
      self.cancel_undeliverable(&mut fresh, "metadata_unavailable".to_string()).await?;
      return Ok(());
    }
    if let Some(reason) = self.dormancy_reason(&fresh.session_id).await? {
      self.cancel_undeliverable(&mut fresh, reason).await?;
      return Ok(());
    }
    self.fire_task_tree(&mut fresh, &message).await?;
    self.stamp_fired(&mut fresh, outcome.reason).await?;
    Ok(())
  }

  async fn reload_pending(&self, trigger: &PendingTrigger) -> anyhow::Result<Option<PendingTrigger>> {
    let path = self.trigger_path(&trigger.session_id, &trigger.id);
    let fresh = match self.load_trigger_path(&path).await {
      Ok(fresh) => fresh,
      Err(error)
        if error.downcast_ref::<io::Error>().is_some_and(|io_error| io_error.kind() == io::ErrorKind::NotFound) =>
      {
        tracing::warn!(target: "charliebot_triggers", trigger_id = %trigger.id, "trigger_file_missing_after_sleep");
        return Ok(None);
      }
      Err(error) => return Err(error),
    };
    Ok((fresh.status == TriggerStatus::Pending).then_some(fresh))
  }

  async fn cancel_undeliverable(&self, trigger: &mut PendingTrigger, reason: String) -> anyhow::Result<()> {
    trigger.status = TriggerStatus::Cancelled;
    trigger.fire_reason = Some(reason.clone());
    self.save_trigger(trigger).await?;
    self.tasks.lock().expect("trigger task map is not poisoned").remove(&trigger.id);
    tracing::info!(
      target: "charliebot_triggers",
      trigger_id = %trigger.id,
      session = %trigger.session_id,
      reason,
      "trigger_cancelled_archived_session"
    );
    Ok(())
  }

  async fn fire_task_tree(&self, trigger: &mut PendingTrigger, message: &str) -> anyhow::Result<()> {
    self.features.store.load_task_meta(&trigger.session_id).await?;
    let dispatcher = charliebot_tasks::dispatch::dispatcher(&self.features)?;
    let fields = json!({ "input_id": trigger.id });
    match dispatcher.admit_input(&trigger.session_id, event_types::SCHEDULED_TRIGGER, message, "system", fields).await {
      Ok(_) => {}
      Err(StoreError::Archived(_)) => {
        tracing::info!(
          target: "charliebot_triggers",
          trigger_id = %trigger.id,
          session = %trigger.session_id,
          "trigger_cancelled_archived_target"
        );
        self.cancel_undeliverable(trigger, "target task is archived".to_string()).await?;
        return Ok(());
      }
      Err(error) => return Err(error.into()),
    }
    dispatcher.dispatch_pending(&trigger.session_id).await?;
    tracing::info!(
      target: "charliebot_triggers",
      trigger_id = %trigger.id,
      session = %trigger.session_id,
      "trigger_delivered_to_task_tree"
    );
    Ok(())
  }

  async fn stamp_fired(&self, trigger: &mut PendingTrigger, reason: WaitReason) -> anyhow::Result<()> {
    trigger.status = TriggerStatus::Fired;
    trigger.fired_at = Some(UtcDateTime::now());
    trigger.fire_reason = Some(reason.to_string());
    self.save_trigger(trigger).await?;
    self.tasks.lock().expect("trigger task map is not poisoned").remove(&trigger.id);
    tracing::info!(
      target: "charliebot_triggers",
      trigger_id = %trigger.id,
      session = %trigger.session_id,
      reason = %reason,
      "trigger_fired"
    );
    Ok(())
  }

  async fn wait_with_pidfd(
    &self,
    trigger: &PendingTrigger,
    targets: &[LocalPid],
  ) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let mut finished = Vec::new();
    let mut waiting = Vec::new();
    let mut join_set = JoinSet::new();
    let targets_by_pid = targets.iter().map(|target| (target.pid, target.clone())).collect::<HashMap<_, _>>();
    for target in targets {
      match open_pidfd(target.pid) {
        Ok(fd) => {
          let index = waiting.len();
          waiting.push(Some(target.pid));
          join_set.spawn(async move {
            // SAFETY: `fd` owns one pidfd and remains inside `AsyncFd` until it is dropped.
            let async_fd = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE) }
              .map_err(|error| error.into_parts().1)?;
            let mut ready = async_fd.readable().await?;
            ready.clear_ready();
            Ok::<usize, io::Error>(index)
          });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) => {
          finished.push(self.local_finished_label(&targets_by_pid[&target.pid], true).await);
        }
        Err(error) => return Err(error).with_context(|| format!("pidfd_open {}", target.pid)),
      }
    }
    let deadline = Instant::now() + remaining_until(trigger.fire_at);
    while !join_set.is_empty() {
      tokio::select! {
        _ = sleep_until(deadline) => break,
        result = join_set.join_next() => {
          let Some(result) = result else { break };
          let index = result.context("pidfd waiter task failed")?.context("pidfd readiness failed")?;
          let pid = waiting[index].take().expect("each pidfd waiter completes once");
          let target = &targets_by_pid[&pid];
          finished.push(self.local_finished_label(target, false).await);
        }
      }
    }
    join_set.abort_all();
    while let Some(result) = join_set.join_next().await {
      if let Err(error) = result
        && !error.is_cancelled()
      {
        return Err(error).context("pidfd waiter cleanup failed");
      }
    }
    let still_alive = waiting
      .into_iter()
      .flatten()
      .map(|pid| {
        let target = &targets_by_pid[&pid];
        still_alive_label(&target.pid.to_string(), target.log.as_deref())
      })
      .collect();
    Ok((finished, still_alive))
  }

  async fn wait_with_remote_probe(
    &self,
    trigger: &PendingTrigger,
    targets: &[RemotePid],
  ) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let mut remaining: Vec<(String, i64, RemotePid)> = Vec::new();
    for target in targets {
      if let Some((_, _, previous)) =
        remaining.iter_mut().find(|(host, pid, _)| host == &target.host && pid == &target.pid)
      {
        *previous = target.clone();
      } else {
        remaining.push((target.host.clone(), target.pid, target.clone()));
      }
    }
    let mut finished = Vec::new();
    let mut step = 0;
    loop {
      let time_to_fire = remaining_until(trigger.fire_at);
      if time_to_fire.is_zero() {
        break;
      }
      sleep(self.remote_interval(step).min(time_to_fire)).await;
      let time_to_fire = remaining_until(trigger.fire_at);
      if time_to_fire.is_zero() {
        break;
      }
      let probes = remaining.iter().map(|(_, _, target)| self.ssh_probe_pid(target));
      let results = join_all(probes).await;
      let mut keep = Vec::new();
      for ((host, pid, target), (status, raw)) in remaining.into_iter().zip(results) {
        if status == ProbeStatus::Dead {
          finished.push(self.remote_finished_label(&target).await);
        } else {
          if status != ProbeStatus::Alive {
            tracing::debug!(
              target: "charliebot_triggers",
              trigger_id = %trigger.id,
              host,
              pid,
              raw = raw.trim(),
              "remote_probe_transient_error"
            );
          }
          keep.push((host, pid, target));
        }
      }
      remaining = keep;
      step += 1;
      if remaining.is_empty() {
        break;
      }
    }
    let still_alive = remaining
      .iter()
      .map(|(_, _, target)| still_alive_label(&format!("{}:{}", target.host, target.pid), target.log.as_deref()))
      .collect();
    Ok((finished, still_alive))
  }

  async fn wait_with_sacct(
    &self,
    trigger: &PendingTrigger,
    targets: &[SlurmJob],
  ) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    // One wait group per host, in first-seen order; a job id repeated under a host joins its group once.
    let mut groups: Vec<(Option<String>, Vec<i64>, HashSet<i64>)> = Vec::new();
    let mut group_indexes: HashMap<Option<String>, usize> = HashMap::new();
    for target in targets {
      if let Some(index) = group_indexes.get(&target.host).copied() {
        let (_, ids, seen) = &mut groups[index];
        if seen.insert(target.job_id) {
          ids.push(target.job_id);
        }
      } else {
        let index = groups.len();
        group_indexes.insert(target.host.clone(), index);
        groups.push((target.host.clone(), vec![target.job_id], HashSet::from([target.job_id])));
      }
    }
    let results =
      join_all(groups.iter().map(|(host, ids, _)| self.wait_sacct_group(trigger, host.as_deref(), ids.clone()))).await;
    let mut finished = Vec::new();
    let mut still_alive = Vec::new();
    for result in results {
      let (group_finished, group_alive) = result?;
      finished.extend(group_finished);
      still_alive.extend(group_alive);
    }
    Ok((finished, still_alive))
  }

  async fn wait_sacct_group(
    &self,
    trigger: &PendingTrigger,
    host: Option<&str>,
    mut remaining: Vec<i64>,
  ) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    remaining.sort_unstable();
    if host.is_none() && !program_available(&self.probe_programs.sacct) {
      tracing::warn!(target: "charliebot_triggers", trigger_id = %trigger.id, job_ids = ?remaining, "slurm_watch_no_sacct_skip");
      sleep(remaining_until(trigger.fire_at)).await;
      return Ok((Vec::new(), remaining.iter().map(|job_id| slurm_label(None, *job_id)).collect()));
    }
    let mut finished = Vec::new();
    let mut step = 0;
    let mut last_success = Instant::now();
    loop {
      let (states, error) = self.probe_sacct(&remaining, &trigger.id, host).await;
      if let Some(error) = error {
        tracing::debug!(target: "charliebot_triggers", trigger_id = %trigger.id, host = host.unwrap_or(""), %error, "sacct_probe_transient_error");
        let dark_for = last_success.elapsed();
        if host.is_some() && dark_for >= self.timing.remote_sacct_grace {
          tracing::warn!(
            target: "charliebot_triggers",
            trigger_id = %trigger.id,
            host = host.unwrap_or_default(),
            dark_seconds = dark_for.as_secs(),
            %error,
            "slurm_watch_host_unreachable"
          );
          let note = error.lines().next().unwrap_or_default().chars().take(120).collect::<String>();
          let alive = remaining
            .iter()
            .map(|job_id| {
              format!("{} (unreachable {}m: {})", slurm_label(host, *job_id), dark_for.as_secs() / 60, note)
            })
            .collect();
          return Ok((finished, alive));
        }
      } else {
        last_success = Instant::now();
      }
      let mut keep = Vec::new();
      for job_id in remaining {
        let Some((state, exit_code)) = states.get(&job_id) else {
          keep.push(job_id);
          continue;
        };
        let state_key = state.split_whitespace().next().unwrap_or_default();
        if SLURM_ACTIVE_STATES.contains(&state_key) {
          keep.push(job_id);
        } else if SLURM_TERMINAL_STATES.contains(&state_key) {
          finished.push(format!("{}: {state} {exit_code}", slurm_label(host, job_id)));
        } else {
          tracing::warn!(target: "charliebot_triggers", trigger_id = %trigger.id, job_id, %state, "slurm_unknown_state");
          keep.push(job_id);
        }
      }
      remaining = keep;
      if remaining.is_empty() || remaining_until(trigger.fire_at).is_zero() {
        break;
      }
      let sleep_for = if host.is_none() { self.timing.sacct_interval } else { self.remote_interval(step) };
      sleep(sleep_for.min(remaining_until(trigger.fire_at))).await;
      step += 1;
    }
    let still_alive = remaining.iter().map(|job_id| slurm_label(host, *job_id)).collect();
    Ok((finished, still_alive))
  }

  fn remote_interval(&self, step: usize) -> Duration {
    let base = self.timing.remote_intervals.get(step).copied().unwrap_or(self.timing.remote_plateau);
    let noise = if REMOTE_PROBE_NOISE_MAX_SECONDS == 0 {
      0
    } else {
      (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time is after the Unix epoch")
        .subsec_nanos() as u64)
        % (REMOTE_PROBE_NOISE_MAX_SECONDS + 1)
    };
    base + Duration::from_secs(noise)
  }
}

/// Why a trigger's wait ended: every watched target finished, or the deadline came first.
///
/// `Display` writes the word that `fire_reason` carries in the trigger file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitReason {
  Completed,
  Timeout,
}

impl std::fmt::Display for WaitReason {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(match self {
      Self::Completed => "completed",
      Self::Timeout => "timeout",
    })
  }
}

struct WaitOutcome {
  reason: WaitReason,
  finished: Vec<String>,
  still_alive: Vec<String>,
}

enum WaitGroup {
  Local(Vec<LocalPid>),
  Remote(Vec<RemotePid>),
  Slurm(Vec<SlurmJob>),
}

/// One watched process on the server host.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LocalPid {
  /// The discriminator value.
  pub kind: String,
  /// The process id.
  pub pid: i64,
  /// The wrapper's exit-code file, when the process came from `--cmd`.
  pub exit_file: Option<String>,
  /// The wrapper's log path, when the process came from `--cmd`.
  pub log: Option<String>,
}

/// One watched process on a remote host.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RemotePid {
  /// The discriminator value.
  pub kind: String,
  /// The SSH host.
  pub host: String,
  /// The process id.
  pub pid: i64,
  /// The wrapper's remote exit-code file, when the process came from `--cmd`.
  pub exit_file: Option<String>,
  /// The wrapper's log path, when the process came from `--cmd`.
  pub log: Option<String>,
}

/// One watched SLURM job.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SlurmJob {
  /// The discriminator value.
  pub kind: String,
  /// The SSH host, or `None` for the server host.
  pub host: Option<String>,
  /// The SLURM job id.
  pub job_id: i64,
}

fn local_pid(target: &WatchTarget) -> Option<LocalPid> {
  match target {
    WatchTarget::LocalPid { pid, exit_file, log } => {
      Some(LocalPid { kind: LOCAL_PID_KIND.to_string(), pid: *pid, exit_file: exit_file.clone(), log: log.clone() })
    }
    WatchTarget::RemotePid { .. } | WatchTarget::SlurmJob { .. } => None,
  }
}

fn remote_pid(target: &WatchTarget) -> Option<RemotePid> {
  match target {
    WatchTarget::RemotePid { host, pid, exit_file, log } => Some(RemotePid {
      kind: REMOTE_PID_KIND.to_string(),
      host: host.clone(),
      pid: *pid,
      exit_file: exit_file.clone(),
      log: log.clone(),
    }),
    WatchTarget::LocalPid { .. } | WatchTarget::SlurmJob { .. } => None,
  }
}

fn slurm_job(target: &WatchTarget) -> Option<SlurmJob> {
  match target {
    WatchTarget::SlurmJob { host, job_id } => {
      Some(SlurmJob { kind: SLURM_JOB_KIND.to_string(), host: host.clone(), job_id: *job_id })
    }
    WatchTarget::LocalPid { .. } | WatchTarget::RemotePid { .. } => None,
  }
}

fn wait_groups(targets: &[WatchTarget]) -> Vec<WaitGroup> {
  let mut groups = Vec::new();
  for target in targets {
    let existing = groups.iter().position(|group: &WaitGroup| group.accepts(target));
    if let Some(index) = existing {
      groups[index].push(target);
      continue;
    }
    match target {
      WatchTarget::LocalPid { .. } => {
        groups.push(WaitGroup::Local(vec![local_pid(target).expect("local target variant")]))
      }
      WatchTarget::RemotePid { .. } => {
        groups.push(WaitGroup::Remote(vec![remote_pid(target).expect("remote target variant")]))
      }
      WatchTarget::SlurmJob { .. } => {
        groups.push(WaitGroup::Slurm(vec![slurm_job(target).expect("SLURM target variant")]))
      }
    }
  }
  groups
}

impl WaitGroup {
  fn accepts(&self, target: &WatchTarget) -> bool {
    matches!((self, target), (Self::Local(_), WatchTarget::LocalPid { .. }))
      || matches!((self, target), (Self::Remote(_), WatchTarget::RemotePid { .. }))
      || matches!((self, target), (Self::Slurm(_), WatchTarget::SlurmJob { .. }))
  }

  fn push(&mut self, target: &WatchTarget) {
    match (self, target) {
      (Self::Local(targets), WatchTarget::LocalPid { .. }) => {
        targets.push(local_pid(target).expect("local target variant"))
      }
      (Self::Remote(targets), WatchTarget::RemotePid { .. }) => {
        targets.push(remote_pid(target).expect("remote target variant"))
      }
      (Self::Slurm(targets), WatchTarget::SlurmJob { .. }) => {
        targets.push(slurm_job(target).expect("SLURM target variant"))
      }
      _ => unreachable!("target was matched to its wait group"),
    }
  }
}

#[derive(Debug)]
struct ProbeOutput {
  stdout: Vec<u8>,
  stderr: Vec<u8>,
  status: std::process::ExitStatus,
}

async fn run_probe_command(argv: Vec<String>, deadline: Option<Duration>, kind: &str) -> Result<ProbeOutput, String> {
  let (program, args) = argv.split_first().expect("probe argv has an executable");
  let mut command = Command::new(program);
  command.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
  let output = match deadline {
    Some(deadline) => match timeout(deadline, command.output()).await {
      Ok(result) => result.map_err(|error| format!("spawn failed: {error}"))?,
      Err(_) => return Err(format!("{kind} timeout after {}s", deadline.as_secs())),
    },
    None => command.output().await.map_err(|error| format!("spawn failed: {error}"))?,
  };
  Ok(ProbeOutput { stdout: output.stdout, stderr: output.stderr, status: output.status })
}

fn program_available(program: &str) -> bool {
  let executable = |path: &Path| {
    path.metadata().is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
  };
  let path = Path::new(program);
  if path.components().count() > 1 {
    return executable(path);
  }
  program_available_on_path(program, std::env::var_os("PATH").as_deref())
}

fn program_available_on_path(program: &str, paths: Option<&OsStr>) -> bool {
  // The availability answer is a process-wide memo: the first lookup decides it for every later caller.
  let key = program.to_string();
  let cache = PROGRAM_AVAILABILITY.get_or_init(|| StdMutex::new(HashMap::new()));
  let mut cached = cache.lock().expect("program availability cache mutex is not poisoned");
  if let Some(available) = cached.get(&key) {
    return *available;
  }
  #[cfg(test)]
  PROGRAM_SEARCHES.fetch_add(1, Ordering::Relaxed);
  let available = paths.is_some_and(|paths| {
    std::env::split_paths(paths).any(|directory| {
      let executable = directory.join(program);
      executable.metadata().is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
  });
  cached.insert(key, available);
  available
}

fn pidfd_available() -> bool {
  open_pidfd(std::process::id() as i64).is_ok()
}

fn open_pidfd(pid: i64) -> io::Result<OwnedFd> {
  #[cfg(target_os = "linux")]
  {
    let raw = libc::pid_t::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    if raw < 1 {
      // The kernel answers EINVAL for a pid below 1. rustix's `Pid` asserts on a negative pid, so the check comes first.
      return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let pid = rustix::process::Pid::from_raw(raw).expect("a positive pid is a valid Pid");
    rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).map_err(io::Error::from)
  }
  #[cfg(not(target_os = "linux"))]
  {
    let _ = pid;
    Err(io::Error::from(io::ErrorKind::Unsupported))
  }
}

fn exit_state_label(pid_label: &str, code: Option<&str>, log: Option<&str>) -> String {
  let state = code.map_or_else(|| "exit file missing".to_string(), |code| format!("exit {code}"));
  format!("{pid_label}: {state}{}", log_suffix(log))
}

fn log_suffix(log: Option<&str>) -> String {
  log.map_or_else(String::new, |log| format!(", log {log}"))
}

fn still_alive_label(pid_label: &str, log: Option<&str>) -> String {
  format!("{pid_label}{}", log_suffix(log))
}

fn slurm_label(host: Option<&str>, job_id: i64) -> String {
  host.map_or_else(|| format!("slurm:{job_id}"), |host| format!("{host}:slurm:{job_id}"))
}

/// The per-session trigger-recovery time over which the boot logs, so a slow boot names the session whose trigger
/// files it waited on without growing the log on a healthy one.
const SLOW_TRIGGER_RECOVERY_MS: u64 = 1000;

/// Logs one session's trigger-recovery step that took [`SLOW_TRIGGER_RECOVERY_MS`] or more. One line per slow step:
/// the step covers the session's trigger-file stat walk, the trigger loads and the [`TriggerManager::start_task`]
/// calls, the work a slow boot's trigger phase waits in.
fn log_slow_trigger_recovery(session_dir: &Path, recovered: u64, started: tokio::time::Instant) {
  let elapsed_ms = started.elapsed().as_millis() as u64;
  if elapsed_ms < SLOW_TRIGGER_RECOVERY_MS {
    return;
  }
  let session = session_dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
  tracing::info!(target: "charliebot_triggers", session, triggers = recovered, elapsed_ms, "trigger_recovery_session_slow");
}

fn format_suffix(reason: WaitReason, finished: &[String], still_alive: &[String]) -> String {
  let finished_part = finished.join(", ");
  match reason {
    WaitReason::Completed => format!(" (finished: {finished_part})"),
    WaitReason::Timeout => {
      let alive_part = still_alive.join(", ");
      if finished_part.is_empty() {
        format!(" (still alive: {alive_part})")
      } else {
        format!(" (finished: {finished_part}; still alive: {alive_part})")
      }
    }
  }
}

/// The remote command that prints an exit file, with the path quoted for the remote shell. A path with a NUL byte has
/// no shell quoting.
fn remote_exit_read_command(exit_file: &str) -> Result<String, shlex::QuoteError> {
  Ok(format!("cat {} 2>/dev/null", shlex::try_quote(exit_file)?))
}

/// The route path that the `schedule-trigger` command posts to. The router and `ROUTE_PATTERNS` use it, and so does the
/// CLI.
pub const SCHEDULE_TRIGGER_PATH: &str = "/api/internal/schedule-trigger";

/// Register delayed trigger routes under the internal API paths.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route(SCHEDULE_TRIGGER_PATH, post_only(schedule_trigger))
    .route("/api/internal/triggers/{session_id}/{trigger_id}/cancel", post_only(cancel_trigger_route))
    .with_state(Arc::clone(features))
}

/// The route patterns `routes` mounts. The registry's trailing-slash redirect matches a path against them.
pub const ROUTE_PATTERNS: &[&str] = &[SCHEDULE_TRIGGER_PATH, "/api/internal/triggers/{session_id}/{trigger_id}/cancel"];

/// Build one manager after recovery, then recover its durable pending triggers.
pub fn trigger_step(features: Arc<FeatureContext>) -> futures_util::future::BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    features.startup.insert(Arc::clone(&manager))?;
    manager.recover_pending().await
  })
}

/// Build the schedule request from the fields the body model validated, in the model's declaration order.
fn schedule_request(payload: &Value) -> anyhow::Result<ScheduleTriggerRequest> {
  Ok(ScheduleTriggerRequest {
    session_id: payload["session_id"].as_str().expect("validated string field").to_string(),
    delay_seconds: integer_field(payload, "delay_seconds")?,
    message: payload["message"].as_str().expect("validated string field").to_string(),
    watch_targets: match payload.get("watch_targets") {
      None | Some(Value::Null) => None,
      Some(Value::Array(items)) => Some(items.iter().map(watch_target).collect::<anyhow::Result<_>>()?),
      other => unreachable!("validated watch targets are null or a list, not {other:?}"),
    },
  })
}

/// Build one watched target from a validated member of the tagged union.
fn watch_target(value: &Value) -> anyhow::Result<WatchTarget> {
  Ok(match value["kind"].as_str().expect("validated tag") {
    LOCAL_PID_KIND => WatchTarget::LocalPid {
      pid: integer_field(value, "pid")?,
      exit_file: optional_string(value, "exit_file"),
      log: optional_string(value, "log"),
    },
    REMOTE_PID_KIND => WatchTarget::RemotePid {
      host: value["host"].as_str().expect("validated string field").to_string(),
      pid: integer_field(value, "pid")?,
      exit_file: optional_string(value, "exit_file"),
      log: optional_string(value, "log"),
    },
    SLURM_JOB_KIND => {
      WatchTarget::SlurmJob { host: optional_string(value, "host"), job_id: integer_field(value, "job_id")? }
    }
    other => unreachable!("validated watch target kind {other}"),
  })
}

/// A validated integer field as the 64-bit field it is stored in. The validator reads any integer up to `u64::MAX`, as
/// pydantic reads any `int`; a value past `i64::MAX` has no stored form.
fn integer_field(value: &Value, field: &str) -> anyhow::Result<i64> {
  value[field].as_i64().ok_or_else(|| anyhow!("{field} {} is past the 64-bit integer range", value[field]))
}

/// An optional validated string field: null and absent are both `None`.
fn optional_string(value: &Value, field: &str) -> Option<String> {
  match &value[field] {
    Value::Null => None,
    Value::String(text) => Some(text.clone()),
    other => unreachable!("validated string field {field} is null or a string, not {other:?}"),
  }
}

async fn schedule_trigger(State(features): State<Arc<FeatureContext>>, request: axum::http::Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let payload = match body.validate(&SCHEDULE_TRIGGER_BODY) {
    Ok(Some(payload)) => payload,
    Ok(None) => unreachable!("a required request body validates to an object"),
    Err(response) => return *response,
  };
  let session_id = payload["session_id"].as_str().expect("validated string field");
  match features.store.get_session(session_id).await {
    Ok(Some(_)) => {}
    Ok(None) => return responses::json(StatusCode::NOT_FOUND, &json!({ "detail": "Session not found" })),
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  }
  let request = match schedule_request(&payload) {
    Ok(request) => request,
    Err(error) => return responses::internal_error("OverflowError", error.to_string()),
  };
  if let Some(targets) = &request.watch_targets {
    if targets.is_empty() {
      return responses::json(
        StatusCode::BAD_REQUEST,
        &json!({ "detail": "watch_targets must be non-empty when provided" }),
      );
    }
    for target in targets {
      match target {
        WatchTarget::SlurmJob { job_id, .. } if *job_id <= 0 => {
          return responses::json(
            StatusCode::BAD_REQUEST,
            &json!({ "detail": "watch_targets slurm job_id must be a positive integer" }),
          );
        }
        WatchTarget::LocalPid { pid, .. } | WatchTarget::RemotePid { pid, .. } if *pid <= 0 => {
          return responses::json(
            StatusCode::BAD_REQUEST,
            &json!({ "detail": "watch_targets pids must be positive integers" }),
          );
        }
        _ => {}
      }
    }
  }

  let manager = match features.startup.require::<Arc<TriggerManager>>() {
    Ok(manager) => manager,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let mut watch_probe = Map::new();
  let trigger = match manager
    .create_trigger(
      &request.session_id,
      request.delay_seconds,
      &request.message,
      request.watch_targets.clone(),
      Some(&mut watch_probe),
      None,
      true,
    )
    .await
  {
    Ok(trigger) => trigger,
    Err(
      TriggerCreateError::RemoteVerify(detail)
      | TriggerCreateError::Archived(detail)
      | TriggerCreateError::PendingLimit(detail),
    ) => {
      return responses::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({ "detail": detail }));
    }
    Err(TriggerCreateError::Runtime(detail)) => {
      return responses::json(StatusCode::BAD_REQUEST, &json!({ "detail": detail }));
    }
    Err(TriggerCreateError::Failure(error)) => {
      return responses::internal_error("RuntimeError", error.to_string());
    }
  };
  tracing::info!(
    target: "charliebot_triggers",
    session = %request.session_id,
    trigger_id = %trigger.id,
    watch_targets = %serde_json::to_string(&trigger.watch_targets).expect("watch targets serialize"),
    "trigger_scheduled"
  );
  let mut body = json!({ "trigger_id": trigger.id, "fire_at": trigger.fire_at.to_isoformat_text() });
  if !watch_probe.is_empty() {
    body["watch_probe"] = Value::Object(watch_probe);
  }
  responses::json(StatusCode::OK, &body)
}

async fn cancel_trigger_route(
  State(features): State<Arc<FeatureContext>>,
  AxumPath((session_id, trigger_id)): AxumPath<(String, String)>,
) -> Response {
  let manager = match features.startup.require::<Arc<TriggerManager>>() {
    Ok(manager) => manager,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  match manager.cancel_trigger(&session_id, &trigger_id).await {
    Ok(()) => responses::json(StatusCode::OK, &json!({ "ok": true })),
    Err(error)
      if error.downcast_ref::<io::Error>().is_some_and(|io_error| io_error.kind() == io::ErrorKind::NotFound) =>
    {
      responses::json(StatusCode::NOT_FOUND, &json!({ "detail": "Trigger not found" }))
    }
    Err(error) => responses::internal_error("RuntimeError", error.to_string()),
  }
}

#[cfg(test)]
impl TriggerManager {
  fn test_probe_setup(mut self, ssh: &Path, sacct: &Path, control_dir: &Path, interval: Duration) -> Self {
    self.probe_programs.ssh = ssh.display().to_string();
    self.probe_programs.sacct = sacct.display().to_string();
    self.ssh_control_dir = Some(control_dir.to_path_buf());
    self.timing.remote_intervals = vec![interval];
    self.timing.remote_plateau = interval;
    self.timing.sacct_interval = interval;
    self.timing.remote_sacct_grace = Duration::from_millis(100);
    self.timing.dormancy_interval = Duration::from_millis(20);
    self
  }
}

fn deadline_after(delay_seconds: i64) -> anyhow::Result<UtcDateTime> {
  let duration = time::Duration::seconds(delay_seconds);
  let deadline =
    UtcDateTime::now().into_inner().checked_add(duration).ok_or_else(|| anyhow!("delay_seconds is out of range"))?;
  UtcDateTime::parse(&deadline.format(&Rfc3339)?).context("format trigger deadline")
}

fn remaining_until(deadline: UtcDateTime) -> Duration {
  let remaining = deadline.into_inner() - time::OffsetDateTime::now_utc();
  let nanos = remaining.whole_nanoseconds().max(0).min(u64::MAX as i128) as u64;
  Duration::from_nanos(nanos)
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use axum::body::{Body, to_bytes};
  use axum::http::{Method, Request, StatusCode, header};
  use charliebot_core::config::CREDENTIALS_FILENAME;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::context::StartupState;
  use charliebot_registry::route_claims::RouteClaims;
  use charliebot_registry::sequence::{RestoreOutcome, SequenceCompletion};
  use charliebot_registry::store_hooks;
  use charliebot_registry::{FeatureContext, Registry};
  use charliebot_store::compact::CompactValue;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec};
  use charliebot_store::{ControlGuard, CreateTask, Store, StoreError, StoreResult, TaskCaller};
  use charliebot_tasks::completion::ArchiveAuthority;
  use charliebot_tasks::completion::TaskCompletion;
  use futures_util::future::BoxFuture;
  use serde_json::Value;
  use tower::ServiceExt;

  use super::*;

  #[test]
  fn matches_claims_the_trigger_routes_and_refuses_empty_or_extra_segments() {
    let claimed = ["/api/internal/schedule-trigger", "/api/internal/triggers/s/t/cancel"];
    let refused = [
      "/api/internal/schedule-trigger/",
      "/api/internal/triggers//t/cancel",
      "/api/internal/triggers/s//cancel",
      "/api/internal/triggers/s/t",
      "/api/internal/triggers/s/t/cancel/",
      "/api/internal/triggers/s/t/cancelx",
    ];
    let route_claims = RouteClaims::new(ROUTE_PATTERNS).expect("the route patterns are distinct");
    for path in claimed {
      assert!(route_claims.claims(path), "{path} should be claimed");
    }
    for path in refused {
      assert!(!route_claims.claims(path), "{path} should not be claimed");
    }
  }

  const SESSION_ID: &str = "synthetic-trigger-session";

  #[test]
  fn sacct_availability_searches_once_per_program_for_the_process() {
    let home = tempfile::tempdir().unwrap();
    let path_value = home.path().as_os_str();
    let before = PROGRAM_SEARCHES.load(Ordering::Relaxed);
    assert!(!program_available_on_path("synthetic-sacct-missing", Some(path_value)));
    let executable = home.path().join("synthetic-sacct");
    std::fs::write(&executable, "").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!program_available_on_path("synthetic-sacct-missing", Some(path_value)));
    assert_eq!(PROGRAM_SEARCHES.load(Ordering::Relaxed), before + 1);
    let other = tempfile::tempdir().unwrap();
    let other_executable = other.path().join("synthetic-sacct");
    std::fs::write(&other_executable, "").unwrap();
    std::fs::set_permissions(&other_executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(program_available_on_path("synthetic-sacct", Some(other.path().as_os_str())));
    assert_eq!(PROGRAM_SEARCHES.load(Ordering::Relaxed), before + 2);
  }

  fn feature_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
    let home = tempfile::tempdir().expect("temporary home");
    std::fs::write(home.path().join(CREDENTIALS_FILENAME), "charliebot: {}\n").expect("credentials file");
    let registry = Arc::new(Registry::new());
    let config_value = registry.load_config(home.path()).expect("test config");
    let store = Arc::new(
      Store::new(config_value.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry)))
        .expect("test store"),
    );
    let config_registry = Arc::clone(&registry);
    let config_home = home.path().to_path_buf();
    let config = Arc::new(HotReload::new(
      config_value.config_file(),
      "trigger_test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let credential_home = home.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.path().join(CREDENTIALS_FILENAME),
      "trigger_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let features =
      Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(StartupState::default()) });
    features.startup.insert(TaskCompletion::new(Arc::clone(&features))).expect("one completion enters startup state");
    (home, features)
  }

  /// Archive one task through the completion owner, as the operator's archive route does.
  async fn archive_task(features: &Arc<FeatureContext>, session_id: &str) {
    if features.startup.get::<charliebot_tasks::dispatch::Dispatcher>().is_none() {
      install_dispatcher(features);
    }
    let completion = features.startup.require::<TaskCompletion>().expect("the completion is installed by the fixture");
    completion
      .archive_subtree(
        session_id,
        Some("synthetic-archive"),
        "archived by the user",
        &[],
        &[],
        &ArchiveAuthority::Operator,
      )
      .await
      .expect("archive task subtree");
  }

  fn add_session(features: &FeatureContext, session_id: &str) {
    let metadata = SessionMetadata::new(session_id, "Synthetic trigger session", TaskProfile::Manager);
    let directory = features.store.session_dir(session_id);
    std::fs::create_dir_all(directory.join("data")).expect("session data directory");
    std::fs::write(directory.join("metadata.json"), metadata.to_json_string(&features.registry.metadata_slots(), true))
      .expect("session metadata");
    std::fs::write(directory.join("data").join("chat_events.jsonl"), "").expect("empty event stream");
    features.store.invalidate_cache(session_id);
  }

  async fn add_task_node(features: &FeatureContext, session_id: &str) {
    features
      .store
      .create_task(
        CreateTask {
          request_id: format!("create-{session_id}"),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: Some(TaskSpec { goal: "Synthetic trigger test task".to_string(), ..TaskSpec::default() }),
          name: Some("Synthetic trigger task".to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: Some(session_id.to_string()),
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await
      .expect("create task-tree node");
  }

  fn probe_behavior(stdout: &str, stderr: &str, exit: i64) -> Value {
    json!({ "stdout": stdout, "stderr": stderr, "exit": exit })
  }

  fn fake_probe_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary path");
    exe
      .ancestors()
      .skip(1)
      .map(|directory| directory.join("fake_probe"))
      .find(|candidate| candidate.is_file())
      .expect("cargo builds the fake-probe helper into the target directory that holds the test binary")
  }

  fn fake_manager(
    features: Arc<FeatureContext>,
    root: &Path,
    ssh_behavior: Value,
    sacct_behavior: Value,
  ) -> Arc<TriggerManager> {
    let bin = root.join("fake-bin");
    std::fs::create_dir_all(&bin).expect("fake bin directory");
    let helper = fake_probe_bin();
    for role in ["fake-ssh", "fake-sacct"] {
      std::os::unix::fs::symlink(&helper, bin.join(role))
        .unwrap_or_else(|error| panic!("link fake program {role}: {error}"));
    }
    let behavior = json!({ "fake-ssh": ssh_behavior, "fake-sacct": sacct_behavior });
    std::fs::write(bin.join("probe.json"), serde_json::to_vec(&behavior).expect("serialize probe behavior"))
      .expect("write probe behavior");
    let manager = TriggerManager::new(features).test_probe_setup(
      &bin.join("fake-ssh"),
      &bin.join("fake-sacct"),
      &root.join("synthetic-home/.ssh/controlmasters"),
      Duration::from_millis(2),
    );
    Arc::new(manager)
  }

  async fn send(router: &Router, method: Method, path: &str, body: &str) -> Response {
    send_body(router, method, path, body.as_bytes().to_vec(), Some("application/json")).await
  }

  async fn send_body(
    router: &Router,
    method: Method,
    path: &str,
    body: Vec<u8>,
    content_type: Option<&str>,
  ) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(content_type) = content_type {
      request = request.header(header::CONTENT_TYPE, content_type);
    }
    router
      .clone()
      .oneshot(request.body(Body::from(body)).expect("request builds"))
      .await
      .expect("router handles request")
  }

  async fn response_json(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.expect("response body"))
      .expect("JSON body")
  }

  async fn assert_validation(response: Response, error_type: &str, location: Value) {
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response_json(response).await;
    assert_eq!(body["detail"][0]["type"], error_type);
    assert_eq!(body["detail"][0]["loc"], location);
  }

  fn utf16le_json(value: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in value.encode_utf16() {
      bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
  }

  fn near_future(seconds: i64) -> UtcDateTime {
    deadline_after(seconds).expect("deadline is representable")
  }

  fn pending_trigger(id: &str, session_id: &str, fire_at: UtcDateTime, targets: Vec<WatchTarget>) -> PendingTrigger {
    PendingTrigger {
      id: id.to_string(),
      session_id: session_id.to_string(),
      fire_at,
      message: "synthetic wake".to_string(),
      created_at: UtcDateTime::now(),
      status: TriggerStatus::Pending,
      fired_at: None,
      watch_targets: targets,
      fire_reason: None,
    }
  }

  #[tokio::test]
  async fn saved_trigger_file_holds_pretty_json_without_a_trailing_newline() {
    let (_home, features) = feature_context();
    let manager = Arc::new(TriggerManager::new(features));
    let trigger = pending_trigger("synthetic-bytes-trigger", SESSION_ID, near_future(3600), Vec::new());
    manager.save_trigger(&trigger).await.expect("save trigger");
    let bytes = std::fs::read(manager.trigger_path(SESSION_ID, &trigger.id)).expect("read trigger file");
    assert_eq!(bytes, serde_json::to_string_pretty(&trigger).expect("serialize trigger").into_bytes());
    let loaded = manager.load_trigger(SESSION_ID, &trigger.id).await.expect("load trigger file");
    assert_eq!(serde_json::to_value(loaded).unwrap(), serde_json::to_value(&trigger).unwrap());
  }

  #[test]
  fn remote_exit_read_command_hands_the_remote_shell_the_same_path() {
    let dir = tempfile::tempdir().expect("temporary directory");
    for name in ["exit code.txt", "it's done", "quote\"d", "dollar$HOME", "semi;colon"] {
      let path = dir.path().join(name);
      std::fs::write(&path, format!("{name}-status")).expect("write exit file");
      let command = remote_exit_read_command(path.to_str().expect("UTF-8 path")).expect("path has no NUL byte");
      let output = std::process::Command::new("sh").arg("-c").arg(&command).output().expect("sh runs");
      assert_eq!(String::from_utf8_lossy(&output.stdout), format!("{name}-status"), "command: {command}");
    }
    assert!(remote_exit_read_command("bad\0path").is_err());
  }

  #[test]
  fn open_pidfd_opens_a_live_process_and_reports_a_reaped_one_as_esrch() {
    assert!(open_pidfd(i64::from(std::process::id())).is_ok());
    let mut child = std::process::Command::new("true").spawn().expect("true starts");
    let pid = i64::from(child.id());
    child.wait().expect("true exits");
    let error = open_pidfd(pid).expect_err("a reaped process has no pidfd");
    assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
  }

  #[tokio::test]
  async fn sixth_registration_returns_limit_detail() {
    let (_home, features) = feature_context();
    let manager = Arc::new(TriggerManager::new(features));
    for index in 0..MAX_PENDING_TRIGGERS {
      manager
        .create_trigger(SESSION_ID, 3600, &format!("pending {index}"), None, None, None, true)
        .await
        .expect("pending trigger fits");
    }
    let error = manager
      .create_trigger(SESSION_ID, 3600, "one too many", None, None, None, true)
      .await
      .expect_err("the sixth trigger is refused");
    assert_eq!(
      error.to_string(),
      format!(
        "session {SESSION_ID} has 5 pending triggers (limit 5); watch several targets with one trigger: --watch A --watch B"
      )
    );
    assert_eq!(manager.list_triggers(SESSION_ID).await.expect("trigger list").len(), MAX_PENDING_TRIGGERS);
  }

  #[tokio::test]
  async fn concurrent_registrations_hold_one_create_slot() {
    let (_home, features) = feature_context();
    let manager = Arc::new(TriggerManager::new(features));
    for index in 0..MAX_PENDING_TRIGGERS - 1 {
      manager
        .create_trigger(SESSION_ID, 3600, &format!("pending {index}"), None, None, None, true)
        .await
        .expect("four pending triggers fit");
    }
    let (left, right) = tokio::join!(
      manager.create_trigger(SESSION_ID, 3600, "concurrent left", None, None, None, true),
      manager.create_trigger(SESSION_ID, 3600, "concurrent right", None, None, None, true),
    );
    assert_ne!(left.is_ok(), right.is_ok());
    assert_eq!(manager.list_triggers(SESSION_ID).await.expect("trigger list").len(), MAX_PENDING_TRIGGERS);
  }

  #[tokio::test]
  async fn replacement_trigger_can_bypass_the_pending_limit() {
    let (_home, features) = feature_context();
    let manager = Arc::new(TriggerManager::new(features));
    for index in 0..MAX_PENDING_TRIGGERS {
      manager
        .create_trigger(SESSION_ID, 3600, &format!("pending {index}"), None, None, None, true)
        .await
        .expect("pending trigger fits");
    }
    manager
      .create_trigger(SESSION_ID, 3600, "follow re-arm", None, None, None, false)
      .await
      .expect("follow re-arm skips the registration limit");
    assert_eq!(manager.list_triggers(SESSION_ID).await.expect("trigger list").len(), MAX_PENDING_TRIGGERS + 1);
  }

  #[tokio::test]
  async fn fired_and_cancelled_records_do_not_count_toward_the_limit() {
    let (_home, features) = feature_context();
    let manager = Arc::new(TriggerManager::new(features));
    for (id, status) in [("fired", TriggerStatus::Fired), ("cancelled", TriggerStatus::Cancelled)] {
      let mut trigger = pending_trigger(id, SESSION_ID, near_future(3600), Vec::new());
      trigger.status = status;
      manager.save_trigger(&trigger).await.expect("save terminal trigger");
    }
    for index in 0..MAX_PENDING_TRIGGERS {
      manager
        .create_trigger(SESSION_ID, 3600, &format!("pending {index}"), None, None, None, true)
        .await
        .expect("terminal records do not count");
    }
    assert_eq!(manager.list_triggers(SESSION_ID).await.expect("trigger list").len(), MAX_PENDING_TRIGGERS + 2);
  }

  #[tokio::test]
  async fn recovery_starts_only_pending_trigger_waiters() {
    let (_home, features) = feature_context();
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    for (id, status) in [
      ("synthetic-recovery-pending", TriggerStatus::Pending),
      ("synthetic-recovery-fired", TriggerStatus::Fired),
      ("synthetic-recovery-cancelled", TriggerStatus::Cancelled),
    ] {
      let mut trigger = pending_trigger(id, SESSION_ID, near_future(3600), Vec::new());
      trigger.status = status;
      manager.save_trigger(&trigger).await.expect("save recovery trigger");
    }
    manager.recover_pending().await.expect("recover pending trigger");
    {
      let tasks = manager.tasks.lock().expect("trigger task map is not poisoned");
      assert!(tasks.contains_key("synthetic-recovery-pending"));
      assert!(!tasks.contains_key("synthetic-recovery-fired"));
      assert!(!tasks.contains_key("synthetic-recovery-cancelled"));
    }
    manager.cancel_trigger(SESSION_ID, "synthetic-recovery-pending").await.expect("cancel recovered waiter");
  }

  #[tokio::test]
  async fn missing_local_pid_gets_the_gone_at_start_label() {
    let (_home, features) = feature_context();
    let manager = TriggerManager::new(features);
    let trigger = pending_trigger("synthetic-gone", SESSION_ID, near_future(10), Vec::new());
    let target = LocalPid { kind: "local_pid".to_string(), pid: 1_000_000_000, exit_file: None, log: None };
    let (finished, still_alive) = manager.wait_with_pidfd(&trigger, &[target]).await.expect("pidfd wait");
    assert_eq!(finished, ["1000000000 (gone at start)"]);
    assert!(still_alive.is_empty());
  }

  #[tokio::test]
  async fn local_pid_timeout_keeps_the_live_process() {
    let (_home, features) = feature_context();
    let manager = TriggerManager::new(features);
    let trigger = pending_trigger("synthetic-live", SESSION_ID, near_future(0), Vec::new());
    let target = LocalPid { kind: "local_pid".to_string(), pid: std::process::id() as i64, exit_file: None, log: None };
    let (finished, still_alive) =
      manager.wait_with_pidfd(&trigger, std::slice::from_ref(&target)).await.expect("pidfd wait");
    assert!(finished.is_empty());
    assert_eq!(still_alive, [target.pid.to_string()]);
  }

  #[tokio::test]
  async fn local_finished_label_reads_the_exit_file() {
    let (home, features) = feature_context();
    let manager = TriggerManager::new(features);
    let exit_file = home.path().join("launch-exit");
    std::fs::write(&exit_file, "0\n").expect("exit code");
    let target = LocalPid {
      kind: "local_pid".to_string(),
      pid: 42,
      exit_file: Some(exit_file.display().to_string()),
      log: Some("/synthetic/log".to_string()),
    };
    assert_eq!(manager.local_finished_label(&target, false).await, "42: exit 0, log /synthetic/log");
  }

  #[tokio::test]
  async fn missing_local_exit_file_states_no_exit_code() {
    let (_home, features) = feature_context();
    let manager = TriggerManager::new(features);
    let target = LocalPid {
      kind: "local_pid".to_string(),
      pid: 42,
      exit_file: Some("/synthetic/missing-exit".to_string()),
      log: Some("/synthetic/log".to_string()),
    };
    assert_eq!(manager.local_finished_label(&target, false).await, "42: exit file missing, log /synthetic/log");
  }

  #[tokio::test]
  async fn local_finished_label_without_exit_metadata_is_bare() {
    let (_home, features) = feature_context();
    let manager = TriggerManager::new(features);
    let target = LocalPid { kind: "local_pid".to_string(), pid: 42, exit_file: None, log: None };
    assert_eq!(manager.local_finished_label(&target, false).await, "42");
  }

  #[test]
  fn still_alive_label_keeps_the_log_path() {
    assert_eq!(still_alive_label("42", Some("/synthetic/log")), "42, log /synthetic/log");
  }

  #[test]
  fn legacy_local_watch_target_loads_without_exit_fields() {
    let target: WatchTarget = serde_json::from_value(json!({ "kind": "local_pid", "pid": 42 })).expect("target");
    assert_eq!(target, WatchTarget::LocalPid { pid: 42, exit_file: None, log: None });
  }

  #[tokio::test]
  async fn schedule_route_accepts_mixed_watch_target_kinds() {
    let (home, features) = feature_context();
    add_session(&features, SESSION_ID);
    let manager =
      fake_manager(Arc::clone(&features), home.path(), probe_behavior("ALIVE\n", "", 0), probe_behavior("", "", 0));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let body = json!({
      "session_id": SESSION_ID,
      "delay_seconds": 30,
      "message": "mixed targets",
      "watch_targets": [
        { "kind": "local_pid", "pid": 123 },
        { "kind": "remote_pid", "host": "synthetic-host", "pid": 456 },
        { "kind": "slurm_job", "host": "synthetic-cluster", "job_id": 789 },
      ],
    })
    .to_string();
    let response = send(&routes(&features), Method::POST, "/api/internal/schedule-trigger", &body).await;
    assert_eq!(response.status(), StatusCode::OK, "{}", response_json(response).await);
    let stored = manager.list_triggers(SESSION_ID).await.expect("trigger list");
    assert_eq!(stored.len(), 1);
    assert_eq!(
      stored[0].watch_targets,
      vec![
        WatchTarget::LocalPid { pid: 123, exit_file: None, log: None },
        WatchTarget::RemotePid { host: "synthetic-host".to_string(), pid: 456, exit_file: None, log: None },
        WatchTarget::SlurmJob { host: Some("synthetic-cluster".to_string()), job_id: 789 },
      ]
    );
  }

  #[tokio::test]
  async fn remote_pid_is_verified_before_its_trigger_is_saved() {
    let (home, features) = feature_context();
    let manager =
      fake_manager(Arc::clone(&features), home.path(), probe_behavior("ALIVE\n", "", 0), probe_behavior("", "", 0));
    let target = WatchTarget::RemotePid { host: "synthetic-host".to_string(), pid: 41, exit_file: None, log: None };
    let trigger = manager
      .create_trigger(SESSION_ID, 3600, "remote wake", Some(vec![target.clone()]), None, None, true)
      .await
      .expect("ALIVE remote PID is accepted");
    assert_eq!(trigger.watch_targets, [target]);
    assert_eq!(manager.list_triggers(SESSION_ID).await.expect("trigger list"), [trigger]);
  }

  #[tokio::test]
  async fn remote_pid_that_fails_verification_is_refused() {
    let (home, features) = feature_context();
    let manager =
      fake_manager(Arc::clone(&features), home.path(), probe_behavior("DEAD\n", "", 0), probe_behavior("", "", 0));
    let error = manager
      .create_trigger(
        SESSION_ID,
        3600,
        "remote wake",
        Some(vec![WatchTarget::RemotePid { host: "synthetic-host".to_string(), pid: 41, exit_file: None, log: None }]),
        None,
        None,
        true,
      )
      .await
      .expect_err("DEAD remote PID is refused");
    assert_eq!(
      error.to_string(),
      "verify-on-create failed for remote watch target(s): synthetic-host:41 -> DEAD ('DEAD')"
    );
    assert!(manager.list_triggers(SESSION_ID).await.expect("trigger list").is_empty());
  }

  #[tokio::test]
  async fn remote_finished_label_reads_the_exit_file_over_ssh() {
    let (home, features) = feature_context();
    let manager =
      fake_manager(Arc::clone(&features), home.path(), probe_behavior("7\n", "", 0), probe_behavior("", "", 0));
    let target = RemotePid {
      kind: "remote_pid".to_string(),
      host: "synthetic-host".to_string(),
      pid: 41,
      exit_file: Some("/synthetic/exit file".to_string()),
      log: Some("/synthetic/remote log".to_string()),
    };
    assert_eq!(manager.remote_finished_label(&target).await, "synthetic-host:41: exit 7, log /synthetic/remote log");
  }

  #[tokio::test]
  async fn remote_finished_label_names_a_missing_exit_file() {
    let (home, features) = feature_context();
    let manager =
      fake_manager(Arc::clone(&features), home.path(), probe_behavior("", "", 1), probe_behavior("", "", 0));
    let target = RemotePid {
      kind: "remote_pid".to_string(),
      host: "synthetic-host".to_string(),
      pid: 41,
      exit_file: Some("/synthetic/missing-exit".to_string()),
      log: None,
    };
    assert_eq!(manager.remote_finished_label(&target).await, "synthetic-host:41: exit file missing");
  }

  #[tokio::test]
  async fn remote_finished_label_without_exit_metadata_is_bare() {
    let (_home, features) = feature_context();
    let manager = TriggerManager::new(features);
    let target = RemotePid {
      kind: "remote_pid".to_string(),
      host: "synthetic-host".to_string(),
      pid: 41,
      exit_file: None,
      log: None,
    };
    assert_eq!(manager.remote_finished_label(&target).await, "synthetic-host:41");
  }

  #[tokio::test]
  async fn remote_pid_timeout_keeps_its_log_path() {
    let (home, features) = feature_context();
    let manager =
      fake_manager(Arc::clone(&features), home.path(), probe_behavior("ALIVE\n", "", 0), probe_behavior("", "", 0));
    let trigger = pending_trigger("synthetic-remote-live", SESSION_ID, near_future(0), Vec::new());
    let target = RemotePid {
      kind: "remote_pid".to_string(),
      host: "synthetic-host".to_string(),
      pid: 41,
      exit_file: None,
      log: Some("/synthetic/remote log".to_string()),
    };
    let (finished, still_alive) = manager.wait_with_remote_probe(&trigger, &[target]).await.expect("remote wait");
    assert!(finished.is_empty());
    assert_eq!(still_alive, ["synthetic-host:41, log /synthetic/remote log"]);
  }

  #[tokio::test]
  async fn remote_slurm_terminal_state_has_the_label() {
    let (home, features) = feature_context();
    let manager = fake_manager(
      Arc::clone(&features),
      home.path(),
      probe_behavior("123|COMPLETED|0:0\n", "", 0),
      probe_behavior("", "", 0),
    );
    let trigger = pending_trigger("synthetic-slurm", SESSION_ID, near_future(10), Vec::new());
    let target = SlurmJob { kind: "slurm_job".to_string(), host: Some("synthetic-host".to_string()), job_id: 123 };
    let (finished, still_alive) = manager.wait_with_sacct(&trigger, &[target]).await.expect("remote SLURM wait");
    assert_eq!(finished, ["synthetic-host:slurm:123: COMPLETED 0:0"]);
    assert!(still_alive.is_empty());
  }

  #[tokio::test]
  async fn unreachable_remote_slurm_group_reports_its_probe_error() {
    let (home, features) = feature_context();
    let manager = fake_manager(
      Arc::clone(&features),
      home.path(),
      probe_behavior("", "synthetic ssh refusal\n", 255),
      probe_behavior("", "", 0),
    );
    let trigger = pending_trigger("synthetic-slurm-dark", SESSION_ID, near_future(10), Vec::new());
    let target = SlurmJob { kind: "slurm_job".to_string(), host: Some("synthetic-host".to_string()), job_id: 123 };
    let (finished, still_alive) = manager.wait_with_sacct(&trigger, &[target]).await.expect("remote SLURM wait");
    assert!(finished.is_empty());
    assert_eq!(still_alive, ["synthetic-host:slurm:123 (unreachable 0m: sacct exit 255: synthetic ssh refusal)"]);
  }

  #[tokio::test]
  async fn local_slurm_terminal_state_has_the_label() {
    let (home, features) = feature_context();
    let manager = fake_manager(
      Arc::clone(&features),
      home.path(),
      probe_behavior("", "", 0),
      probe_behavior("123|COMPLETED|0:0\n", "", 0),
    );
    let trigger = pending_trigger("synthetic-local-slurm", SESSION_ID, near_future(10), Vec::new());
    let target = SlurmJob { kind: "slurm_job".to_string(), host: None, job_id: 123 };
    let (finished, still_alive) = manager.wait_with_sacct(&trigger, &[target]).await.expect("local SLURM wait");
    assert_eq!(finished, ["slurm:123: COMPLETED 0:0"]);
    assert!(still_alive.is_empty());
  }

  #[test]
  fn mixed_watch_groups_keep_the_first_seen_kind_order() {
    let targets = [
      WatchTarget::SlurmJob { host: None, job_id: 789 },
      WatchTarget::RemotePid { host: "synthetic-host".to_string(), pid: 456, exit_file: None, log: None },
      WatchTarget::LocalPid { pid: 1_000_000_000, exit_file: None, log: None },
    ];
    let groups = wait_groups(&targets);
    assert!(matches!(
      groups.as_slice(),
      [WaitGroup::Slurm(slurm), WaitGroup::Remote(remote), WaitGroup::Local(local)]
        if slurm.len() == 1 && remote.len() == 1 && local.len() == 1
    ));
  }

  #[tokio::test]
  async fn local_slurm_creation_fails_when_sacct_is_missing() {
    let (home, features) = feature_context();
    let bin = home.path().join("fake-bin");
    std::fs::create_dir_all(&bin).expect("fake bin directory");
    std::os::unix::fs::symlink(fake_probe_bin(), bin.join("fake-ssh")).expect("link fake ssh");
    let behavior = json!({ "fake-ssh": probe_behavior("", "", 0) });
    std::fs::write(bin.join("probe.json"), serde_json::to_vec(&behavior).expect("serialize probe behavior"))
      .expect("write probe behavior");
    let manager = Arc::new(TriggerManager::new(features).test_probe_setup(
      &bin.join("fake-ssh"),
      &home.path().join("missing-sacct"),
      &home.path().join("synthetic-home/.ssh/controlmasters"),
      Duration::from_millis(2),
    ));
    let error = manager
      .create_trigger(
        SESSION_ID,
        60,
        "local slurm",
        Some(vec![WatchTarget::SlurmJob { host: None, job_id: 123 }]),
        None,
        None,
        true,
      )
      .await
      .expect_err("local sacct is required");
    assert_eq!(error.to_string(), "sacct unavailable: cannot watch a SLURM job on a host without slurm");
  }

  #[test]
  fn fire_suffix_matches_outcomes() {
    assert_eq!(
      format_suffix(WaitReason::Completed, &["slurm:123: COMPLETED 0:0".to_string()], &[]),
      " (finished: slurm:123: COMPLETED 0:0)"
    );
    assert_eq!(
      format_suffix(WaitReason::Timeout, &["42".to_string()], &["synthetic-host:43".to_string()]),
      " (finished: 42; still alive: synthetic-host:43)"
    );
    assert_eq!(format_suffix(WaitReason::Timeout, &[], &["42".to_string()]), " (still alive: 42)");
  }

  struct NoopCompletion;

  impl SequenceCompletion for NoopCompletion {
    fn evaluate_automatic_completion<'a>(
      &'a self,
      _session_id: &'a str,
      _run_id: &'a str,
      _summary: Option<&'a str>,
      _result_refs: Option<&'a [String]>,
      _request_id: Option<&'a str>,
    ) -> BoxFuture<'a, StoreResult<(usize, Value)>> {
      Box::pin(async { Err(StoreError::new("unused sequence completion")) })
    }

    fn close_sequence_child<'a>(
      &'a self,
      _session_id: &'a str,
      _request_id: &'a str,
      _outcome: &'a str,
      _evidence: &'a Value,
    ) -> BoxFuture<'a, StoreResult<(usize, Value)>> {
      Box::pin(async { Err(StoreError::new("unused sequence completion")) })
    }

    fn restore_chain<'a>(
      &'a self,
      _session_id: &'a str,
      _request_id: &'a str,
      _reason: &'a str,
    ) -> BoxFuture<'a, StoreResult<Vec<String>>> {
      Box::pin(async { Err(StoreError::new("unused sequence completion")) })
    }

    fn restore_chain_locked<'a>(
      &'a self,
      _guard: &'a ControlGuard,
      _session_id: &'a str,
      _request_id: &'a str,
      _reason: &'a str,
    ) -> BoxFuture<'a, StoreResult<RestoreOutcome>> {
      Box::pin(async { Err(StoreError::new("unused sequence completion")) })
    }

    fn archive_unsuccessful_worker<'a>(
      &'a self,
      _session_id: &'a str,
      _run_id: &'a str,
      _outcome: &'a str,
    ) -> BoxFuture<'a, StoreResult<bool>> {
      Box::pin(async { Err(StoreError::new("unused sequence completion")) })
    }
  }

  impl charliebot_tasks::dispatch::RunFinishedCompletion for NoopCompletion {
    fn after_run_finished<'a>(&'a self, _session_id: &'a str, _run_id: &'a str) -> BoxFuture<'a, StoreResult<()>> {
      Box::pin(async { Err(StoreError::new("unused run completion")) })
    }
  }

  fn install_dispatcher(features: &Arc<FeatureContext>) -> Arc<charliebot_tasks::dispatch::Dispatcher> {
    let completion: Arc<dyn SequenceCompletion> = Arc::new(NoopCompletion);
    let run_completion: Arc<dyn charliebot_tasks::dispatch::RunFinishedCompletion> = Arc::new(NoopCompletion);
    features
      .startup
      .insert(charliebot_tasks::dispatch::Dispatcher::new(
        Arc::clone(&features.store),
        Arc::clone(features),
        completion,
        run_completion,
      ))
      .expect("insert dispatcher");
    charliebot_tasks::dispatch::dispatcher(features).expect("dispatcher is in startup state")
  }

  #[tokio::test]
  async fn fire_admits_one_durable_scheduled_input_before_stamping_fired() {
    let (_home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    install_dispatcher(&features);
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    let trigger = pending_trigger("synthetic-durable-trigger", SESSION_ID, near_future(-1), Vec::new());
    manager.save_trigger(&trigger).await.expect("save trigger");
    manager.wait_and_fire(trigger.clone()).await.expect("fire trigger");
    let fresh = manager.load_trigger(SESSION_ID, &trigger.id).await.expect("read fired trigger");
    assert_eq!(fresh.status, TriggerStatus::Fired);
    assert_eq!(fresh.fire_reason.as_deref(), Some("timeout"));
    let events = features.store.load_all_facts(SESSION_ID).expect("read task events");
    let scheduled = events
      .iter()
      .filter(|event| event.get("type").and_then(CompactValue::as_str) == Some(event_types::SCHEDULED_TRIGGER))
      .collect::<Vec<_>>();
    assert_eq!(scheduled.len(), 1);
    assert_eq!(scheduled[0].get("id").and_then(CompactValue::as_str), Some(trigger.id.as_str()));
    assert!(
      scheduled[0]
        .get("content")
        .and_then(CompactValue::as_str)
        .is_some_and(|content| content.contains("synthetic wake"))
    );
  }

  #[tokio::test]
  async fn master_wake_reuses_an_existing_stable_input() {
    let (_home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    let dispatcher = install_dispatcher(&features);
    dispatcher
      .admit_input(
        SESSION_ID,
        event_types::AGENT_MESSAGE,
        "summon prompt",
        "agent",
        json!({ "input_id": "synthetic-existing-wake", "from_session": SESSION_ID, "from_session_name": "Synthetic" }),
      )
      .await
      .expect("persist existing wake");
    let mut request = charliebot_tasks::master_trigger::TriggerMasterRequest::new(SESSION_ID, "summon prompt");
    request.input_id = Some("synthetic-existing-wake");
    request.from_session = Some(SESSION_ID);
    request.from_session_name = Some("Synthetic");
    charliebot_tasks::master_trigger::trigger_master(&features, request).await.expect("dispatch existing wake");
    let events = features.store.load_all_facts(SESSION_ID).expect("read task events");
    assert_eq!(
      events
        .iter()
        .filter(|event| event.get("id").and_then(CompactValue::as_str) == Some("synthetic-existing-wake"))
        .count(),
      1
    );
  }

  #[tokio::test]
  async fn archived_session_refuses_trigger_creation_without_writing_a_record() {
    let (_home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    archive_task(&features, SESSION_ID).await;
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    let error = manager
      .create_trigger(SESSION_ID, 60, "wake", None, None, None, true)
      .await
      .expect_err("archived session rejects wake");
    assert!(error.to_string().contains("target task is archived"));
    assert!(manager.list_triggers(SESSION_ID).await.expect("trigger list").is_empty());
  }

  #[tokio::test]
  async fn archived_task_with_a_session_successor_still_refuses_trigger_creation() {
    let (_home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    add_task_node(&features, "synthetic-trigger-successor").await;
    archive_task(&features, SESSION_ID).await;
    let mut metadata =
      features.store.get_session(SESSION_ID).await.expect("read archived target").expect("target exists");
    metadata.successor_session_id = Some("synthetic-trigger-successor".to_string());
    let path = features.store.session_dir(SESSION_ID).join("metadata.json");
    std::fs::write(&path, metadata.to_json_string(&features.registry.metadata_slots(), true))
      .expect("write successor link");
    features.store.invalidate_cache(SESSION_ID);
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    let error = manager
      .create_trigger(SESSION_ID, 60, "wake", None, None, None, true)
      .await
      .expect_err("archived source refuses its own trigger despite a successor");
    assert!(error.to_string().contains("target task is archived"));
    assert!(manager.list_triggers("synthetic-trigger-successor").await.expect("successor trigger list").is_empty());
  }

  #[tokio::test]
  async fn fire_time_backstop_cancels_an_archived_target() {
    let (_home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    let trigger = pending_trigger("synthetic-archived-trigger", SESSION_ID, near_future(0), Vec::new());
    manager.save_trigger(&trigger).await.expect("save trigger");
    archive_task(&features, SESSION_ID).await;
    manager.wait_and_fire(trigger.clone()).await.expect("dormant trigger retires");
    let fresh = manager.load_trigger(SESSION_ID, &trigger.id).await.expect("read cancelled trigger");
    assert_eq!(fresh.status, TriggerStatus::Cancelled);
    assert_eq!(fresh.fire_reason.as_deref(), Some("target task is archived"));
    let events = features.store.load_all_facts(SESSION_ID).expect("read task events");
    assert!(
      !events
        .iter()
        .any(|event| event.get("type").and_then(CompactValue::as_str) == Some(event_types::SCHEDULED_TRIGGER))
    );
  }

  #[tokio::test]
  async fn dormancy_watchdog_carries_its_reason_to_the_cancel_record() {
    let (_home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    archive_task(&features, SESSION_ID).await;
    let mut manager_value = TriggerManager::new(Arc::clone(&features));
    manager_value.timing.dormancy_interval = Duration::from_millis(2);
    let manager = Arc::new(manager_value);
    let trigger = pending_trigger("synthetic-watchdog-trigger", SESSION_ID, near_future(3600), Vec::new());
    manager.save_trigger(&trigger).await.expect("save trigger");
    manager.wait_and_fire(trigger.clone()).await.expect("watchdog cancels trigger");
    let fresh = manager.load_trigger(SESSION_ID, &trigger.id).await.expect("read cancelled trigger");
    assert_eq!(fresh.status, TriggerStatus::Cancelled);
    assert_eq!(fresh.fire_reason.as_deref(), Some("target task is archived"));
  }

  #[tokio::test]
  async fn schedule_route_maps_the_pending_limit_to_422() {
    let (_home, features) = feature_context();
    add_session(&features, SESSION_ID);
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    for index in 0..MAX_PENDING_TRIGGERS {
      manager
        .create_trigger(SESSION_ID, 3600, &format!("pending {index}"), None, None, None, true)
        .await
        .expect("pending trigger fits");
    }
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let response = send(
      &routes(&features),
      Method::POST,
      "/api/internal/schedule-trigger",
      &format!(r#"{{"session_id":"{SESSION_ID}","delay_seconds":60,"message":"one too many"}}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
      response_json(response).await["detail"],
      format!(
        "session {SESSION_ID} has 5 pending triggers (limit 5); watch several targets with one trigger: --watch A --watch B"
      )
    );
  }

  #[tokio::test]
  async fn schedule_route_maps_archived_target_to_422() {
    let (_home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    archive_task(&features, SESSION_ID).await;
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let response = send(
      &routes(&features),
      Method::POST,
      "/api/internal/schedule-trigger",
      &format!(r#"{{"session_id":"{SESSION_ID}","delay_seconds":60,"message":"wake"}}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
      response_json(response).await["detail"],
      format!("session {SESSION_ID} is archived (target task is archived); trigger rejected")
    );
  }

  #[tokio::test]
  async fn schedule_route_maps_remote_verification_refusal_to_422() {
    let (home, features) = feature_context();
    add_session(&features, SESSION_ID);
    let manager =
      fake_manager(Arc::clone(&features), home.path(), probe_behavior("DEAD\n", "", 0), probe_behavior("", "", 0));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let response = send(
      &routes(&features),
      Method::POST,
      "/api/internal/schedule-trigger",
      &format!(
        r#"{{"session_id":"{SESSION_ID}","delay_seconds":60,"message":"wake","watch_targets":[{{"kind":"remote_pid","host":"synthetic-host","pid":123}}]}}"#
      ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
      response_json(response).await["detail"].as_str().expect("refusal detail").contains("synthetic-host:123 -> DEAD")
    );
  }

  #[tokio::test]
  async fn schedule_route_returns_trigger_id_and_fire_at() {
    let (_home, features) = feature_context();
    add_session(&features, SESSION_ID);
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let response = send(
      &routes(&features),
      Method::POST,
      "/api/internal/schedule-trigger",
      &format!(r#"{{"session_id":"{SESSION_ID}","delay_seconds":3600,"message":"scheduled wake"}}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body.as_object().expect("object").len(), 2);
    let id = body["trigger_id"].as_str().expect("trigger id");
    assert!(body["fire_at"].as_str().is_some());
    assert_eq!(manager.load_trigger(SESSION_ID, id).await.expect("saved trigger").message, "scheduled wake");
  }

  #[tokio::test]
  async fn schedule_route_returns_404_for_a_missing_session() {
    let (_home, features) = feature_context();
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let response = send(
      &routes(&features),
      Method::POST,
      "/api/internal/schedule-trigger",
      r#"{"session_id":"synthetic-missing-session","delay_seconds":60,"message":"wake"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response_json(response).await["detail"], "Session not found");
  }

  #[tokio::test]
  async fn schedule_route_returns_400_for_empty_targets_and_nonpositive_ids() {
    let (_home, features) = feature_context();
    add_session(&features, SESSION_ID);
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let router = routes(&features);
    let cases = [
      (json!([]), "watch_targets must be non-empty when provided"),
      (json!([{ "kind": "local_pid", "pid": 0 }]), "watch_targets pids must be positive integers"),
      (
        json!([{ "kind": "remote_pid", "host": "synthetic-host", "pid": -1 }]),
        "watch_targets pids must be positive integers",
      ),
      (json!([{ "kind": "slurm_job", "job_id": 0 }]), "watch_targets slurm job_id must be a positive integer"),
    ];
    for (targets, detail) in cases {
      let body = json!({ "session_id": SESSION_ID, "delay_seconds": 60, "message": "wake", "watch_targets": targets })
        .to_string();
      let response = send(&router, Method::POST, "/api/internal/schedule-trigger", &body).await;
      assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
      assert_eq!(response_json(response).await["detail"], detail, "{body}");
    }
  }

  #[tokio::test]
  async fn schedule_route_keeps_422_validation_type_and_location() {
    let (_home, features) = feature_context();
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let response = send(
      &routes(&features),
      Method::POST,
      "/api/internal/schedule-trigger",
      r#"{"session_id":"synthetic-trigger-session","delay_seconds":"tomorrow","message":"wake"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response_json(response).await;
    assert_eq!(body["detail"][0]["type"], "int_parsing");
    assert_eq!(body["detail"][0]["loc"], json!(["body", "delay_seconds"]));
  }

  /// Assert every `detail` entry's `type` and `loc`, in pydantic's order.
  async fn assert_validation_detail(response: Response, expected: &[(&str, Value)]) {
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response_json(response).await;
    let detail = body["detail"].as_array().expect("detail list");
    assert_eq!(detail.len(), expected.len(), "{detail:?}");
    for (error, (error_type, location)) in detail.iter().zip(expected) {
      assert_eq!(error["type"], *error_type);
      assert_eq!(error["loc"], *location);
    }
  }

  #[tokio::test]
  async fn schedule_route_reports_member_and_field_errors_in_pydantic_order() {
    let (_home, features) = feature_context();
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let response = send(
      &routes(&features),
      Method::POST,
      "/api/internal/schedule-trigger",
      r#"{"session_id":"synthetic-trigger-session","delay_seconds":60,"message":123,"watch_targets":[{}]}"#,
    )
    .await;
    assert_validation_detail(
      response,
      &[("string_type", json!(["body", "message"])), ("union_tag_not_found", json!(["body", "watch_targets", 0]))],
    )
    .await;
  }

  #[tokio::test]
  async fn schedule_route_names_the_variant_of_a_bad_member_field() {
    let (_home, features) = feature_context();
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let cases = [
      (r#"{"kind":"local_pid","pid":"x"}"#, [("int_parsing", json!(["body", "watch_targets", 0, "local_pid", "pid"]))]),
      (r#"{"kind":"synthetic-unknown"}"#, [("union_tag_invalid", json!(["body", "watch_targets", 0]))]),
      (r#"42"#, [("model_attributes_type", json!(["body", "watch_targets", 0]))]),
    ];
    for (member, expected) in cases {
      let body = format!(
        r#"{{"session_id":"synthetic-trigger-session","delay_seconds":60,"message":"wake","watch_targets":[{member}]}}"#
      );
      let response = send(&routes(&features), Method::POST, "/api/internal/schedule-trigger", &body).await;
      let expected = expected.iter().map(|(kind, loc)| (*kind, loc.clone())).collect::<Vec<_>>();
      assert_validation_detail(response, &expected).await;
    }
    let response = send(
      &routes(&features),
      Method::POST,
      "/api/internal/schedule-trigger",
      r#"{"session_id":"synthetic-trigger-session","delay_seconds":60,"message":"wake","watch_targets":"nope"}"#,
    )
    .await;
    assert_validation_detail(response, &[("list_type", json!(["body", "watch_targets"]))]).await;
  }

  #[tokio::test]
  async fn schedule_route_drops_an_undeclared_member_key_from_the_trigger_file() {
    let (_home, features) = feature_context();
    add_session(&features, SESSION_ID);
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let response = send(
      &routes(&features),
      Method::POST,
      "/api/internal/schedule-trigger",
      r#"{"session_id":"synthetic-trigger-session","delay_seconds":60,"message":"wake","watch_targets":[{"kind":"local_pid","pid":7,"synthetic_extra":1}]}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "{}", response_json(response).await);
    let trigger_id = response_json(response).await["trigger_id"].as_str().expect("trigger id").to_string();
    let file = features.store.session_dir(SESSION_ID).join("triggers").join(format!("{trigger_id}.json"));
    let record: Value =
      serde_json::from_slice(&std::fs::read(&file).expect("trigger file")).expect("JSON trigger file");
    assert_eq!(
      record["watch_targets"],
      json!([{ "kind": "local_pid", "pid": 7, "exit_file": Value::Null, "log": Value::Null }])
    );
  }

  #[tokio::test]
  async fn schedule_route_answers_500_for_an_integer_past_the_64_bit_range() {
    let (_home, features) = feature_context();
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let missing = r#"{"session_id":"synthetic-missing-session","delay_seconds":9223372036854775808,"message":"wake"}"#;
    let response = send(&routes(&features), Method::POST, "/api/internal/schedule-trigger", missing).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    add_session(&features, SESSION_ID);
    let bodies = [
      r#"{"session_id":"synthetic-trigger-session","delay_seconds":9223372036854775808,"message":"wake"}"#,
      r#"{"session_id":"synthetic-trigger-session","delay_seconds":60,"message":"wake","watch_targets":[{"kind":"local_pid","pid":9223372036854775808}]}"#,
    ];
    for body in bodies {
      let response = send(&routes(&features), Method::POST, "/api/internal/schedule-trigger", body).await;
      assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
  }

  #[tokio::test]
  async fn cancel_route_returns_not_found_and_success_answers() {
    let (_home, features) = feature_context();
    add_session(&features, SESSION_ID);
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let router = routes(&features);
    let missing = send(
      &router,
      Method::POST,
      &format!("/api/internal/triggers/{SESSION_ID}/synthetic-missing-trigger/cancel"),
      "{}",
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(response_json(missing).await["detail"], "Trigger not found");
    let created =
      manager.create_trigger(SESSION_ID, 3600, "cancel me", None, None, None, true).await.expect("create trigger");
    let cancelled =
      send(&router, Method::POST, &format!("/api/internal/triggers/{SESSION_ID}/{}/cancel", created.id), "{}").await;
    assert_eq!(cancelled.status(), StatusCode::OK);
    assert_eq!(response_json(cancelled).await, json!({ "ok": true }));
    assert_eq!(
      manager.load_trigger(SESSION_ID, &created.id).await.expect("cancelled record").status,
      TriggerStatus::Cancelled
    );
  }

  #[tokio::test]
  async fn schedule_route_returns_observed_remote_slurm_state() {
    let (home, features) = feature_context();
    add_session(&features, SESSION_ID);
    let manager = fake_manager(
      Arc::clone(&features),
      home.path(),
      probe_behavior("123|RUNNING|0:0\n", "", 0),
      probe_behavior("", "", 0),
    );
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let response = send(
      &routes(&features),
      Method::POST,
      "/api/internal/schedule-trigger",
      &format!(r#"{{"session_id":"{SESSION_ID}","delay_seconds":3600,"message":"wake","watch_targets":[{{"kind":"slurm_job","host":"synthetic-host","job_id":123}}]}}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["watch_probe"]["synthetic-host:slurm:123"], "RUNNING");
  }

  #[tokio::test]
  async fn schedule_trigger_model_accepts_utf16_and_forbids_extra_fields() {
    let (_home, features) = feature_context();
    add_session(&features, SESSION_ID);
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    features.startup.insert(Arc::clone(&manager)).expect("insert trigger manager");
    let router = routes(&features);
    let path = "/api/internal/schedule-trigger";
    let valid = format!(r#"{{"session_id":"{SESSION_ID}","delay_seconds":3600,"message":"synthetic wake"}}"#);
    assert_eq!(send(&router, Method::POST, path, &valid).await.status(), StatusCode::OK);
    assert_eq!(
      send_body(&router, Method::POST, path, utf16le_json(&valid), Some("application/json")).await.status(),
      StatusCode::OK
    );
    assert_validation(
      send(
        &router,
        Method::POST,
        path,
        &format!(r#"{{"session_id":"{SESSION_ID}","delay_seconds":60,"message":"wake","extra":true}}"#),
      )
      .await,
      "extra_forbidden",
      json!(["body", "extra"]),
    )
    .await;
  }

  #[tokio::test]
  async fn schedule_trigger_body_errors_precede_session_lookup() {
    let (_home, features) = feature_context();
    let router = routes(&features);
    let path = "/api/internal/schedule-trigger";
    assert_validation(
      send_body(&router, Method::POST, path, Vec::new(), Some("application/json")).await,
      "missing",
      json!(["body"]),
    )
    .await;
    assert_validation(
      send_body(
        &router,
        Method::POST,
        path,
        br#"{"session_id":"synthetic-session","delay_seconds":60,"message":"wake"}"#.to_vec(),
        Some("text/plain"),
      )
      .await,
      "model_attributes_type",
      json!(["body"]),
    )
    .await;
    assert_validation(
      send_body(&router, Method::POST, path, b"{".to_vec(), Some("application/json")).await,
      "json_invalid",
      json!(["body", 1]),
    )
    .await;
    assert_validation(
      send_body(&router, Method::POST, path, b"[]".to_vec(), Some("application/json")).await,
      "model_attributes_type",
      json!(["body"]),
    )
    .await;

    let wrong_types = [
      (r#"{"session_id":1,"delay_seconds":60,"message":"wake"}"#, "string_type", json!(["body", "session_id"])),
      (
        r#"{"session_id":"synthetic-session","delay_seconds":"soon","message":"wake"}"#,
        "int_parsing",
        json!(["body", "delay_seconds"]),
      ),
      (
        r#"{"session_id":"synthetic-session","delay_seconds":60,"message":1}"#,
        "string_type",
        json!(["body", "message"]),
      ),
      (
        r#"{"session_id":"synthetic-session","delay_seconds":60,"message":"wake","watch_targets":"wrong"}"#,
        "list_type",
        json!(["body", "watch_targets"]),
      ),
      (
        r#"{"session_id":"synthetic-session","delay_seconds":60,"message":"wake","watch_targets":[{"kind":"local_pid","pid":"bad"}]}"#,
        "int_parsing",
        json!(["body", "watch_targets", 0, "local_pid", "pid"]),
      ),
    ];
    for (body, error_type, location) in wrong_types {
      assert_validation(send(&router, Method::POST, path, body).await, error_type, location).await;
    }
    for (body, location) in [
      (r#"{"delay_seconds":60,"message":"wake"}"#, json!(["body", "session_id"])),
      (r#"{"session_id":"synthetic-session","message":"wake"}"#, json!(["body", "delay_seconds"])),
      (r#"{"session_id":"synthetic-session","delay_seconds":60}"#, json!(["body", "message"])),
    ] {
      assert_validation(send(&router, Method::POST, path, body).await, "missing", location).await;
    }

    let malformed_missing_session = r#"{"session_id":"synthetic-missing-session""#;
    assert_validation(
      send(&router, Method::POST, path, malformed_missing_session).await,
      "json_invalid",
      json!(["body", malformed_missing_session.len()]),
    )
    .await;
  }
}
