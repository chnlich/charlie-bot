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
use std::time::{Duration, SystemTime};

use anyhow::{Context, anyhow};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, TaggedVariant};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_core::{event_types, parallel_walk, ssh, text_repr};
use charliebot_registry::context::{FeatureContext, StartupStop};
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

use crate::end_record::{self, EndSource, RemoteTargetEnd, SLURM_TERMINAL_STATES};
use crate::held_session::{self, HeldEnd, HeldSessions, HeldTarget, PRODUCTION_TIMING, SshArgv};
use crate::trigger_index::{self, TriggerFileIdentity, TriggerIndex, TriggerIndexRow};

/// The maximum number of pending triggers for one session.
pub const MAX_PENDING_TRIGGERS: usize = 5;

const REMOTE_PROBE_INTERVALS: [u64; 6] = [10, 20, 40, 80, 160, 320];
const REMOTE_PROBE_PLATEAU_SECONDS: u64 = 600;
const REMOTE_PROBE_NOISE_MAX_SECONDS: u64 = 10;
const SACCT_POLL_INTERVAL_SECONDS: u64 = 30;
const REMOTE_SACCT_UNREACHABLE_GRACE_SECONDS: u64 = 900;
const DORMANCY_CHECK_SECONDS: u64 = 60;
const SSH_OVERALL_TIMEOUT_SECONDS: u64 = 60;

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

/// Why the trigger code starts one remote ssh process. The `remote_ssh_started` log line carries it, so counting a
/// wake's ssh processes across restarts reads one row per process.
#[derive(Clone, Copy, Debug)]
enum SshPurpose {
  /// The verify-on-create probe that guards a registration.
  Verify,
  /// One interval probe of a pending watch.
  Probe,
  /// The ssh that reads a finished `--cmd` target's exit file back.
  Readback,
}

impl std::fmt::Display for SshPurpose {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(match self {
      Self::Verify => "verify",
      Self::Probe => "probe",
      Self::Readback => "readback",
    })
  }
}

/// The end recorder one verify-on-create ssh starts for a pid-only target, with the trigger's max wait as its
/// bound.
struct RecorderStart<'a> {
  trigger_id: &'a str,
  max_wait_seconds: i64,
}

/// What one remote pid probe answered, with the end-file and date lines the end readback reads.
struct PidProbeOutcome {
  status: ProbeStatus,
  raw: String,
  read: end_record::PidProbeRead,
  /// The probe ssh's local midpoint, in server-clock epoch ns.
  local_mid_ns: Option<i128>,
}

impl PidProbeOutcome {
  fn error(raw: String) -> Self {
    PidProbeOutcome {
      status: ProbeStatus::Error,
      raw,
      read: end_record::PidProbeRead { now_ns: None, end_ns: None },
      local_mid_ns: None,
    }
  }
}

/// One remote ssh process the trigger code ran, with the local midpoint its offset math needs.
struct SshRun {
  result: Result<ProbeOutput, String>,
  local_mid_ns: Option<i128>,
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
/// The session trigger body field that lists the targets a trigger watches. The body model reads it and so does
/// the `session trigger` command, so neither crate spells the name on its own.
pub const WATCH_TARGETS_FIELD: &str = "watch_targets";

/// The session trigger body field that holds the wait before the wake fires. The body model reads it and so does
/// the `session trigger` command, so neither crate spells the name on its own.
pub const DELAY_SECONDS_FIELD: &str = "delay_seconds";

/// The longest `message` a scheduled-trigger registration accepts, counted in characters. The `session trigger`
/// command checks `--message` against the same limit, so neither crate spells it on its own.
pub const MAX_TRIGGER_MESSAGE_CHARS: usize = 200;

const SCHEDULE_TRIGGER_BODY_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required(DELAY_SECONDS_FIELD, FieldType::Integer),
  Field::required("message", FieldType::String).max_length(MAX_TRIGGER_MESSAGE_CHARS),
  Field::optional(
    WATCH_TARGETS_FIELD,
    FieldType::NullableTaggedUnionList { discriminator: "kind", variants: WATCH_TARGET_VARIANTS },
  ),
];
const SCHEDULE_TRIGGER_BODY: BodyModel = BodyModel::required(SCHEDULE_TRIGGER_BODY_FIELDS).forbid_extra();

const SLURM_ACTIVE_STATES: [&str; 7] =
  ["PENDING", "RUNNING", "CONFIGURING", "COMPLETING", "REQUEUED", "RESIZING", "SUSPENDED"];

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
  held: HeldSessions,
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
  noise_max_seconds: u64,
}

impl TriggerManager {
  /// Construct the one manager held in startup state.
  pub fn new(features: Arc<FeatureContext>) -> TriggerManager {
    let probe_programs = ProbePrograms { ssh: "ssh".to_string(), sacct: "sacct".to_string() };
    TriggerManager {
      features,
      tasks: StdMutex::new(HashMap::new()),
      create_locks: StdMutex::new(HashMap::new()),
      held: HeldSessions::new(
        held_session::enabled_from_env(),
        ssh_argv_builder(probe_programs.ssh.clone(), None),
        PRODUCTION_TIMING,
        &SLURM_TERMINAL_STATES,
      ),
      probe_programs,
      ssh_control_dir: None,
      timing: Timing {
        remote_intervals: REMOTE_PROBE_INTERVALS.into_iter().map(Duration::from_secs).collect(),
        remote_plateau: Duration::from_secs(REMOTE_PROBE_PLATEAU_SECONDS),
        sacct_interval: Duration::from_secs(SACCT_POLL_INTERVAL_SECONDS),
        remote_sacct_grace: Duration::from_secs(REMOTE_SACCT_UNREACHABLE_GRACE_SECONDS),
        dormancy_interval: Duration::from_secs(DORMANCY_CHECK_SECONDS),
        noise_max_seconds: REMOTE_PROBE_NOISE_MAX_SECONDS,
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
    let remote_slurm = targets.iter().filter_map(slurm_job).filter(|target| target.host.is_some()).collect::<Vec<_>>();
    // The id exists before the verifications because the verify ssh starts a pid-only target's end recorder under
    // it, and every verify's remote_ssh_started row names it. A refused registration leaves a recorder that its own
    // timeout bounds.
    let trigger_id = fresh_uuid();
    if !remote_pids.is_empty() {
      self
        .verify_remote_pids(&remote_pids, &trigger_id, delay_seconds)
        .await
        .map_err(TriggerCreateError::RemoteVerify)?;
    }

    let observed = if remote_slurm.is_empty() {
      Map::new()
    } else {
      self.verify_remote_slurm(&remote_slurm, &trigger_id).await.map_err(TriggerCreateError::RemoteVerify)?
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
      id: trigger_id,
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
    self.start_task(trigger.clone(), false);
    tracing::info!(
      target: "charliebot_triggers",
      trigger_id = %trigger.id,
      session = session_id,
      fire_at = %trigger.fire_at,
      "trigger_created"
    );
    Ok(trigger)
  }

  async fn verify_remote_pids(
    &self,
    targets: &[RemotePid],
    trigger_id: &str,
    max_wait_seconds: i64,
  ) -> Result<(), String> {
    let probes = targets.iter().map(|target| {
      // A pid-only target gets its end recorder inside this verify ssh; a `--cmd` target reads its end from the
      // exit file's mtime instead.
      let recorder =
        (target.exit_file.is_none() && max_wait_seconds > 0).then_some(RecorderStart { trigger_id, max_wait_seconds });
      let remote = pid_probe_command(target, recorder.as_ref(), None);
      async move { self.ssh_probe_pid(target, trigger_id, SshPurpose::Verify, &remote).await }
    });
    let results = join_all(probes).await;
    let mut bad = Vec::new();
    for (target, outcome) in targets.iter().zip(results) {
      if outcome.status != ProbeStatus::Alive {
        bad.push(format!(
          "{}:{} -> {} ({})",
          target.host,
          target.pid,
          outcome.status,
          text_repr::str_repr(outcome.raw.trim())
        ));
      }
    }
    if bad.is_empty() {
      Ok(())
    } else {
      Err(format!("verify-on-create failed for remote watch target(s): {}", bad.join("; ")))
    }
  }

  async fn verify_remote_slurm(&self, targets: &[SlurmJob], trigger_id: &str) -> Result<Map<String, Value>, String> {
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
      async move { self.probe_sacct(&ids, trigger_id, Some(&host), SshPurpose::Verify).await }
    });
    let results = join_all(probes).await;
    let mut observed = Map::new();
    let mut bad = Vec::new();
    for (host, reading) in hosts.iter().zip(results) {
      if let Some(error) = &reading.error {
        bad.push(format!("{host} -> {error}"));
        continue;
      }
      let mut ids = by_host.get(host).expect("the host was inserted").clone();
      ids.sort_unstable();
      for job_id in ids {
        let key = slurm_label(Some(host), job_id);
        let state = reading.states.get(&job_id).map_or("not-yet-registered", |row| row.state.as_str());
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
  ///
  /// One blocking walk reads every session's trigger files in parallel, and the pending triggers then start in directory
  /// order. A trigger task writes only its own file, so reading every file before the first start reads what the
  /// walk would read with the starts in between.
  pub async fn recover_pending(self: &Arc<Self>) -> anyhow::Result<()> {
    let stop = self.features.startup.stop();
    let sessions_dir = self.features.store.config().sessions_dir();
    let home = self.features.store.config().charliebot_home.clone();
    // A file this index proves not pending needs no read: its row is served only while the file keeps the identity the
    // row recorded, and a file whose status changed was rewritten since.
    let index = TriggerIndex::read(&home);
    let walk_stop = stop.clone();
    let walk = tokio::task::spawn_blocking(move || read_session_triggers(&sessions_dir, &walk_stop, &index))
      .await
      .context("trigger recovery walk task failed")?;
    for session in walk.sessions {
      let mut recovered = 0u64;
      for (path, loaded) in session.triggers {
        if stop.is_cancelled() {
          return Ok(());
        }
        let trigger = match loaded {
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
        if trigger.status == TriggerStatus::Pending {
          self.start_task(trigger.clone(), true);
          recovered += 1;
          tracing::info!(
            target: "charliebot_triggers",
            trigger_id = %trigger.id,
            session = %trigger.session_id,
            "trigger_recovered"
          );
        }
      }
      log_slow_trigger_recovery(&session.dir, recovered, session.read_time);
    }
    walk.end?;
    // The rows this boot proved: the files it read as not pending, and the files the index served. The write keeps the
    // next boot's walk to the files that are pending or new.
    if let Err(error) = trigger_index::write(&home, &walk.index_rows) {
      tracing::warn!(target: "charliebot_triggers", error = %error, "trigger_index_write_failed");
    }
    Ok(())
  }

  fn start_task(self: &Arc<Self>, trigger: PendingTrigger, recovered: bool) {
    let id = trigger.id.clone();
    let task_id = id.clone();
    let manager = Arc::clone(self);
    let (start, wait) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
      let _ = wait.await;
      if let Err(error) = manager.wait_and_fire(trigger, recovered).await {
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

  async fn ssh_probe_pid(
    &self,
    target: &RemotePid,
    trigger_id: &str,
    purpose: SshPurpose,
    remote: &str,
  ) -> PidProbeOutcome {
    self.ssh_probe_pid_with(target, trigger_id, purpose, remote, end_record::pid_probe_read).await
  }

  /// Run one pid ssh and read the clock and end lines its `read` parser names.
  async fn ssh_probe_pid_with(
    &self,
    target: &RemotePid,
    trigger_id: &str,
    purpose: SshPurpose,
    remote: &str,
    read: fn(&str) -> end_record::PidProbeRead,
  ) -> PidProbeOutcome {
    let run = self.run_remote_ssh(&target.host, remote, trigger_id, purpose).await;
    let output = match run.result {
      Ok(output) => output,
      Err(error) => return PidProbeOutcome::error(error),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{stdout}{stderr}");
    let read = read(&stdout);
    let last_line = stdout.trim().lines().last().unwrap_or_default().trim();
    let status = match last_line {
      PROBE_ALIVE_WORD => ProbeStatus::Alive,
      PROBE_DEAD_WORD => ProbeStatus::Dead,
      _ => {
        let combined = raw.trim();
        let error = if combined.is_empty() {
          format!("ssh exit {}", output.status.code().map_or_else(|| "None".to_string(), |code| code.to_string()))
        } else {
          combined.to_string()
        };
        return PidProbeOutcome { status: ProbeStatus::Error, raw: error, read, local_mid_ns: run.local_mid_ns };
      }
    };
    PidProbeOutcome { status, raw, read, local_mid_ns: run.local_mid_ns }
  }

  /// Confirm one pid that the held session reported gone, with the same verdict and end-file read the fire path
  /// takes from an interval probe. The ssh is a readback: the held report is only the trigger for it.
  async fn held_readback_pid(&self, target: &RemotePid, trigger_id: &str) -> PidProbeOutcome {
    // Only a pid-only target has a recorder end file; a `--cmd` target's end is the exit file `remote_finish` reads,
    // so its readback waits for no recorder and stays short.
    let end_file = target.exit_file.is_none().then(|| end_record::end_file_word(trigger_id, target.pid));
    let remote = pid_readback_command(target.pid, end_file.as_deref());
    self.ssh_probe_pid_with(target, trigger_id, SshPurpose::Readback, &remote, end_record::pid_readback_read).await
  }

  /// Run one remote ssh process, logging the `remote_ssh_started` row that counts ssh processes per wake across
  /// restarts. The local midpoint brackets the whole run, so a readback's offset math measures the same ssh it
  /// reads the remote time from.
  async fn run_remote_ssh(&self, host: &str, remote: &str, trigger_id: &str, purpose: SshPurpose) -> SshRun {
    let command = match self.ssh_argv(host, remote) {
      Ok(command) => command,
      Err(error) => return SshRun { result: Err(format!("spawn failed: {error}")), local_mid_ns: None },
    };
    tracing::info!(
      target: "charliebot_triggers",
      trigger_ids = %trigger_id,
      host = %host,
      purpose = %purpose,
      "remote_ssh_started"
    );
    let started = SystemTime::now();
    let result = run_probe_command(command, Some(Duration::from_secs(SSH_OVERALL_TIMEOUT_SECONDS)), "ssh").await;
    let local_mid_ns = end_record::local_midpoint_ns(started, SystemTime::now());
    SshRun { result, local_mid_ns }
  }

  fn ssh_argv(&self, host: &str, remote: &str) -> io::Result<Vec<String>> {
    remote_ssh_argv(&self.probe_programs.ssh, self.ssh_control_dir.as_deref(), host, remote)
  }

  /// Run one sacct query, locally or over ssh, and read the states it answers. The remote ssh also prints the
  /// clock and zone lines that convert a terminal row's `End` column, so a remote end readback measures its offset
  /// in the ssh that read the end back.
  async fn probe_sacct(
    &self,
    job_ids: &[i64],
    trigger_id: &str,
    host: Option<&str>,
    purpose: SshPurpose,
  ) -> SacctReading {
    let ids = job_ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let format = format!("--format={}", end_record::SACCT_FORMAT_COLUMNS.join(","));
    let sacct_args = ["sacct", "-j", &ids, "-X", "-n", "-P", &format];
    let (output, local_mid_ns) = match host {
      Some(host) => {
        let remote = format!("{}; {}", end_record::SACCT_REMOTE_CLOCK_PREFIX, sacct_args.join(" "));
        let run = self.run_remote_ssh(host, &remote, trigger_id, purpose).await;
        match run.result {
          Ok(output) => (output, run.local_mid_ns),
          Err(error) => return SacctReading::error(error),
        }
      }
      None => {
        let command = std::iter::once(self.probe_programs.sacct.clone())
          .chain(sacct_args.iter().skip(1).map(|arg| arg.to_string()))
          .collect();
        match run_probe_command(command, None, "sacct").await {
          Ok(output) => (output, None),
          Err(error) => return SacctReading::error(error),
        }
      }
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
      return SacctReading::error(format!(
        "sacct exit {}: {stderr}",
        output.status.code().map_or_else(|| "None".to_string(), |code| code.to_string())
      ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut states = HashMap::new();
    for line in stdout.lines() {
      let fields = line.trim().split('|').collect::<Vec<_>>();
      if fields.len() < 3 {
        continue;
      }
      let Ok(job_id) = fields[0].parse::<i64>() else {
        // The date and zone lines the remote command leads with parse as no job id and fall through here.
        continue;
      };
      let end_raw = fields.get(3).map(|field| field.trim()).filter(|end| !end.is_empty()).map(str::to_string);
      states.insert(
        job_id,
        SlurmRow { state: fields[1].trim().to_string(), exit_code: fields[2].trim().to_string(), end_raw },
      );
    }
    SacctReading { states, error: None, prefix: end_record::sacct_probe_prefix(&stdout), local_mid_ns }
  }

  /// Read one `--cmd` target's exit file back over ssh: the exit code, the file's mtime as the launch's end
  /// instant, and the date line that measures the offset. A failure is logged and yields no reading; it never
  /// delays or blocks the fire.
  async fn remote_exit_read(&self, target: &RemotePid, trigger_id: &str) -> (end_record::ExitRead, Option<i128>) {
    let failed = |read: end_record::ExitRead| (read, None);
    let Some(exit_file) = target.exit_file.as_deref() else {
      return failed(end_record::ExitRead { code: None, mtime_ns: None, now_ns: None });
    };
    let remote = match end_record::exit_read_command(exit_file) {
      Ok(remote) => remote,
      Err(error) => {
        tracing::debug!(target: "charliebot_triggers", host = %target.host, error = %error, "ssh_exit_read_failed");
        return failed(end_record::ExitRead { code: None, mtime_ns: None, now_ns: None });
      }
    };
    let run = self.run_remote_ssh(&target.host, &remote, trigger_id, SshPurpose::Readback).await;
    let output = match run.result {
      Ok(output) if output.status.success() => output,
      Ok(output) => {
        tracing::debug!(
          target: "charliebot_triggers",
          host = %target.host,
          returncode = output.status.code().unwrap_or(-1),
          "ssh_exit_read_failed"
        );
        return failed(end_record::ExitRead { code: None, mtime_ns: None, now_ns: None });
      }
      Err(error) => {
        tracing::debug!(target: "charliebot_triggers", host = %target.host, error, "ssh_exit_read_failed");
        return failed(end_record::ExitRead { code: None, mtime_ns: None, now_ns: None });
      }
    };
    (end_record::exit_read(&String::from_utf8_lossy(&output.stdout)), run.local_mid_ns)
  }

  /// The label and the end reading one remote target carries once a probe saw it gone. The reading rides the ssh
  /// calls that already exist: a pid-only target's end file is read by the DEAD probe itself, and a `--cmd`
  /// target's mtime by the exit read it pays anyway. A readback failure logs a warning naming the trigger and
  /// leaves the fire untouched.
  async fn remote_finish(
    &self,
    target: &RemotePid,
    trigger_id: &str,
    probe: &PidProbeOutcome,
    seen_gone_at: time::OffsetDateTime,
  ) -> (String, RemoteTargetEnd) {
    let base = format!("{}:{}", target.host, target.pid);
    if target.exit_file.is_none() {
      let end = match (probe.read.end_ns, probe.read.now_ns, probe.local_mid_ns) {
        (Some(end_remote_ns), Some(now_ns), Some(local_mid_ns)) => end_reading_or_warn(
          end_remote_ns,
          now_ns,
          local_mid_ns,
          EndSource::EndRecorder,
          seen_gone_at,
          trigger_id,
          &base,
        )
        .unwrap_or_else(|| seen_gone_only(seen_gone_at)),
        _ => {
          tracing::warn!(target: "charliebot_triggers", trigger_id, target = %base, "remote_end_read_failed");
          seen_gone_only(seen_gone_at)
        }
      };
      return (base, end);
    }
    let (read, local_mid_ns) = self.remote_exit_read(target, trigger_id).await;
    let end = match (read.mtime_ns, read.now_ns, local_mid_ns) {
      (Some(mtime_ns), Some(now_ns), Some(local_mid_ns)) => {
        end_reading_or_warn(mtime_ns, now_ns, local_mid_ns, EndSource::ExitFileMtime, seen_gone_at, trigger_id, &base)
          .unwrap_or_else(|| seen_gone_only(seen_gone_at))
      }
      _ => {
        tracing::warn!(target: "charliebot_triggers", trigger_id, target = %base, "remote_end_read_failed");
        seen_gone_only(seen_gone_at)
      }
    };
    (exit_state_label(&base, read.code.as_deref(), target.log.as_deref()), end)
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

  async fn wait_for_trigger(&self, trigger: &PendingTrigger) -> anyhow::Result<WaitOutcome> {
    let groups = wait_groups(&trigger.watch_targets);
    if groups.is_empty() {
      sleep(remaining_until(trigger.fire_at)).await;
      return Ok(WaitOutcome {
        reason: WaitReason::Timeout,
        finished: Vec::new(),
        still_alive: Vec::new(),
        remote_ends: Vec::new(),
      });
    }
    let waits = groups.into_iter().map(|group| async move {
      let (group_finished, group_alive, group_ends) = match group {
        WaitGroup::Local(targets) => {
          let (finished, still_alive) = self.wait_with_pidfd(trigger, &targets).await?;
          (finished, still_alive, Vec::new())
        }
        WaitGroup::Remote(targets) => self.wait_with_remote_probe(trigger, &targets).await?,
        WaitGroup::Slurm(targets) => self.wait_with_sacct(trigger, &targets).await?,
      };
      Ok::<_, anyhow::Error>((group_finished, group_alive, group_ends))
    });
    let results = join_all(waits).await;
    let mut finished = Vec::new();
    let mut still_alive = Vec::new();
    let mut remote_ends = Vec::new();
    for result in results {
      let (group_finished, group_alive, group_ends) = result?;
      finished.extend(group_finished);
      still_alive.extend(group_alive);
      remote_ends.extend(group_ends);
    }
    let reason = if still_alive.is_empty() { WaitReason::Completed } else { WaitReason::Timeout };
    Ok(WaitOutcome { reason, finished, still_alive, remote_ends })
  }

  async fn watch_dormancy(&self, session_id: &str) -> anyhow::Result<String> {
    loop {
      sleep(self.timing.dormancy_interval).await;
      if let Some(reason) = self.dormancy_reason(session_id).await? {
        return Ok(reason);
      }
    }
  }

  async fn wait_and_fire(self: &Arc<Self>, trigger: PendingTrigger, recovered: bool) -> anyhow::Result<()> {
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
    // The wake moment, taken before the fire path runs, so lag_ms and wait_s measure the wait and not the dispatch.
    let fired_at = UtcDateTime::now();
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
    // Only a trigger with remote targets gains the end fields; a local or pure-delay trigger keeps its plain line.
    let fire_log = has_remote_target(&fresh.watch_targets)
      .then(|| build_fire_log(&fresh, fired_at, &outcome.remote_ends, recovered));
    self.fire_task_tree(&mut fresh, &message).await?;
    self.stamp_fired(&mut fresh, outcome.reason, fire_log).await?;
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
    let fields = json!({ (event_types::INPUT_ID): trigger.id });
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

  async fn stamp_fired(
    &self,
    trigger: &mut PendingTrigger,
    reason: WaitReason,
    fire_log: Option<FireLog>,
  ) -> anyhow::Result<()> {
    trigger.status = TriggerStatus::Fired;
    trigger.fired_at = Some(UtcDateTime::now());
    trigger.fire_reason = Some(reason.to_string());
    self.save_trigger(trigger).await?;
    self.tasks.lock().expect("trigger task map is not poisoned").remove(&trigger.id);
    match fire_log {
      None => tracing::info!(
        target: "charliebot_triggers",
        trigger_id = %trigger.id,
        session = %trigger.session_id,
        reason = %reason,
        "trigger_fired"
      ),
      Some(log) => log_remote_fired(trigger, reason, &log),
    }
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
  ) -> anyhow::Result<(Vec<String>, Vec<String>, Vec<RemoteTargetEnd>)> {
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
    let keys = remaining.iter().map(|(host, pid, _)| HeldTarget::Pid { host: host.clone(), pid: *pid }).collect();
    let mut held = self.held.watch(&trigger.id, keys);
    let mut finished = Vec::new();
    let mut remote_ends = Vec::new();
    let mut step = 0;
    // The next interval probe keeps the schedule it has without a held session: a held end never moves it.
    let mut next_probe = Instant::now() + self.remote_interval(step).min(remaining_until(trigger.fire_at));
    loop {
      if remaining_until(trigger.fire_at).is_zero() {
        break;
      }
      tokio::select! {
        _ = sleep_until(next_probe) => {
          if remaining_until(trigger.fire_at).is_zero() {
            break;
          }
          let probes = remaining.iter().map(|(_, _, target)| {
            // A pid-only target's end file rides this probe: the DEAD answer reads and deletes it in the same ssh.
            let end_read = target.exit_file.is_none().then(|| end_record::end_file_word(&trigger.id, target.pid));
            let remote = pid_probe_command(target, None, end_read.as_deref());
            async move { self.ssh_probe_pid(target, &trigger.id, SshPurpose::Probe, &remote).await }
          });
          let results = join_all(probes).await;
          let mut keep = Vec::new();
          for ((host, pid, target), outcome) in remaining.into_iter().zip(results) {
            if outcome.status == ProbeStatus::Dead {
              let (label, end) =
                self.remote_finish(&target, &trigger.id, &outcome, time::OffsetDateTime::now_utc()).await;
              finished.push(label);
              remote_ends.push(end);
            } else {
              if outcome.status != ProbeStatus::Alive {
                tracing::debug!(
                  target: "charliebot_triggers",
                  trigger_id = %trigger.id,
                  host,
                  pid,
                  raw = outcome.raw.trim(),
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
          next_probe = Instant::now() + self.remote_interval(step).min(remaining_until(trigger.fire_at));
        }
        end = held.recv() => {
          let HeldEnd::Pid { host, pid } = end else {
            unreachable!("a pid wait watches only pid ends");
          };
          let Some(index) = remaining.iter().position(|(held_host, held_pid, _)| held_host == &host && *held_pid == pid)
          else {
            continue;
          };
          tracing::info!(target: "charliebot_triggers", trigger_id = %trigger.id, host = %host, pid, "held_end_received");
          let target = remaining[index].2.clone();
          let outcome = self.held_readback_pid(&target, &trigger.id).await;
          if outcome.status == ProbeStatus::Dead {
            let (label, end) = self.remote_finish(&target, &trigger.id, &outcome, time::OffsetDateTime::now_utc()).await;
            finished.push(label);
            remote_ends.push(end);
            remaining.remove(index);
            if remaining.is_empty() {
              break;
            }
          } else {
            tracing::debug!(
              target: "charliebot_triggers",
              trigger_id = %trigger.id,
              host = %host,
              pid,
              raw = outcome.raw.trim(),
              "held_readback_not_dead"
            );
            // The registry dropped this target when it dispatched the end, so a readback that still finds it alive
            // must put it back; otherwise only the interval probe watches it.
            held.resubscribe(HeldTarget::Pid { host, pid });
          }
        }
      }
    }
    let still_alive = remaining
      .iter()
      .map(|(_, _, target)| still_alive_label(&format!("{}:{}", target.host, target.pid), target.log.as_deref()))
      .collect();
    Ok((finished, still_alive, remote_ends))
  }

  async fn wait_with_sacct(
    &self,
    trigger: &PendingTrigger,
    targets: &[SlurmJob],
  ) -> anyhow::Result<(Vec<String>, Vec<String>, Vec<RemoteTargetEnd>)> {
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
    let mut remote_ends = Vec::new();
    for result in results {
      let (group_finished, group_alive, group_ends) = result?;
      finished.extend(group_finished);
      still_alive.extend(group_alive);
      remote_ends.extend(group_ends);
    }
    Ok((finished, still_alive, remote_ends))
  }

  async fn wait_sacct_group(
    &self,
    trigger: &PendingTrigger,
    host: Option<&str>,
    mut remaining: Vec<i64>,
  ) -> anyhow::Result<(Vec<String>, Vec<String>, Vec<RemoteTargetEnd>)> {
    remaining.sort_unstable();
    if host.is_none() && !program_available(&self.probe_programs.sacct) {
      tracing::warn!(target: "charliebot_triggers", trigger_id = %trigger.id, job_ids = ?remaining, "slurm_watch_no_sacct_skip");
      sleep(remaining_until(trigger.fire_at)).await;
      return Ok((Vec::new(), remaining.iter().map(|job_id| slurm_label(None, *job_id)).collect(), Vec::new()));
    }
    let keys = match host {
      Some(host) => {
        remaining.iter().map(|job_id| HeldTarget::Slurm { host: host.to_string(), job_id: *job_id }).collect()
      }
      None => Vec::new(),
    };
    let mut held = self.held.watch(&trigger.id, keys);
    let mut finished = Vec::new();
    let mut remote_ends = Vec::new();
    let mut step = 0;
    let mut last_success = Instant::now();
    let mut next_probe = Instant::now();
    loop {
      tokio::select! {
        _ = sleep_until(next_probe) => {
          let reading = self.probe_sacct(&remaining, &trigger.id, host, SshPurpose::Probe).await;
          if let Some(error) = &reading.error {
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
              return Ok((finished, alive, remote_ends));
            }
          } else {
            last_success = Instant::now();
          }
          remaining = self.settle_sacct(trigger, host, &reading, remaining, &mut finished, &mut remote_ends);
          if remaining.is_empty() || remaining_until(trigger.fire_at).is_zero() {
            break;
          }
          let sleep_for = if host.is_none() { self.timing.sacct_interval } else { self.remote_interval(step) };
          next_probe = Instant::now() + sleep_for.min(remaining_until(trigger.fire_at));
          step += 1;
        }
        end = held.recv() => {
          let HeldEnd::Slurm { job_id, state, .. } = end else {
            unreachable!("a Slurm wait watches only Slurm ends");
          };
          if !remaining.contains(&job_id) {
            continue;
          }
          tracing::info!(
            target: "charliebot_triggers",
            trigger_id = %trigger.id,
            host = host.unwrap_or(""),
            job_id,
            state = %state,
            "held_end_received"
          );
          let reading = self.probe_sacct(&remaining, &trigger.id, host, SshPurpose::Readback).await;
          if let Some(error) = &reading.error {
            tracing::debug!(target: "charliebot_triggers", trigger_id = %trigger.id, host = host.unwrap_or(""), %error, "held_readback_failed");
          }
          remaining = self.settle_sacct(trigger, host, &reading, remaining, &mut finished, &mut remote_ends);
          if remaining.is_empty() {
            break;
          }
          // A job the readback could not settle stays in the wait, and the registry dropped it when it dispatched
          // the end, so the watch takes it back; otherwise only the interval probe watches it.
          if let (Some(host), true) = (host, remaining.contains(&job_id)) {
            held.resubscribe(HeldTarget::Slurm { host: host.to_string(), job_id });
          }
        }
      }
    }
    let still_alive = remaining.iter().map(|job_id| slurm_label(host, *job_id)).collect();
    Ok((finished, still_alive, remote_ends))
  }

  /// Apply one sacct reading to the jobs a wait still watches: a terminal row finishes its job with the end reading
  /// the same ssh answered, and every other job stays. Returns the jobs that still wait.
  fn settle_sacct(
    &self,
    trigger: &PendingTrigger,
    host: Option<&str>,
    reading: &SacctReading,
    remaining: Vec<i64>,
    finished: &mut Vec<String>,
    remote_ends: &mut Vec<RemoteTargetEnd>,
  ) -> Vec<i64> {
    let mut keep = Vec::new();
    for job_id in remaining {
      let Some(row) = reading.states.get(&job_id) else {
        keep.push(job_id);
        continue;
      };
      let state_key = row.state.split_whitespace().next().unwrap_or_default();
      if SLURM_ACTIVE_STATES.contains(&state_key) {
        keep.push(job_id);
      } else if SLURM_TERMINAL_STATES.contains(&state_key) {
        let label = format!("{}: {} {}", slurm_label(host, job_id), row.state, row.exit_code);
        finished.push(label);
        // The sacct readback ssh that answered this row is also the ssh the end conversion measures.
        if let Some(host) = host {
          remote_ends.push(self.sacct_end_reading(&trigger.id, host, job_id, reading, time::OffsetDateTime::now_utc()));
        }
      } else {
        tracing::warn!(target: "charliebot_triggers", trigger_id = %trigger.id, job_id, state = %row.state, "slurm_unknown_state");
        keep.push(job_id);
      }
    }
    keep
  }

  /// The end reading one terminal remote slurm row carries: the `End` column names the remote host's local zone,
  /// which the same readback ssh printed. A row without a readable end logs a warning naming the trigger and
  /// leaves the fire untouched.
  fn sacct_end_reading(
    &self,
    trigger_id: &str,
    host: &str,
    job_id: i64,
    reading: &SacctReading,
    seen_gone_at: time::OffsetDateTime,
  ) -> RemoteTargetEnd {
    let label = slurm_label(Some(host), job_id);
    let end_raw = reading.states.get(&job_id).and_then(|row| row.end_raw.as_deref());
    let parts = (end_raw, reading.prefix.zone_seconds, reading.prefix.now_ns, reading.local_mid_ns);
    if let (Some(end_raw), Some(zone), Some(now_ns), Some(local_mid_ns)) = parts
      && let Some(end_remote_ns) = end_record::sacct_end_remote_ns(end_raw, zone)
    {
      return end_reading_or_warn(
        end_remote_ns,
        now_ns,
        local_mid_ns,
        EndSource::SacctEnd,
        seen_gone_at,
        trigger_id,
        &label,
      )
      .unwrap_or_else(|| seen_gone_only(seen_gone_at));
    }
    tracing::warn!(target: "charliebot_triggers", trigger_id, target = %label, "remote_end_read_failed");
    seen_gone_only(seen_gone_at)
  }

  fn remote_interval(&self, step: usize) -> Duration {
    let base = self.timing.remote_intervals.get(step).copied().unwrap_or(self.timing.remote_plateau);
    let noise = if self.timing.noise_max_seconds == 0 {
      0
    } else {
      (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time is after the Unix epoch")
        .subsec_nanos() as u64)
        % (self.timing.noise_max_seconds + 1)
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
  /// One entry per remote target the server saw gone during the wait.
  remote_ends: Vec<RemoteTargetEnd>,
}

/// One sacct query's answer: the rows the fire decision reads, plus the clock and zone lines that convert a
/// terminal row's `End` column, and the readback ssh's local midpoint.
struct SacctReading {
  states: HashMap<i64, SlurmRow>,
  error: Option<String>,
  prefix: end_record::SacctProbePrefix,
  local_mid_ns: Option<i128>,
}

impl SacctReading {
  fn error(error: String) -> Self {
    SacctReading {
      states: HashMap::new(),
      error: Some(error),
      prefix: end_record::SacctProbePrefix { now_ns: None, zone_seconds: None },
      local_mid_ns: None,
    }
  }
}

/// One sacct row: the state and exit code the fire decision reads, and the `End` column the end readback reads.
struct SlurmRow {
  state: String,
  exit_code: String,
  end_raw: Option<String>,
}

/// The remote-end fields one `trigger_fired` line of a remote-target trigger gains. `end` is set when at least one
/// remote target recorded an end; `seen_gone_at` is set when the server saw any remote target gone.
struct FireLog {
  seen_gone_at: Option<time::OffsetDateTime>,
  end: Option<EndFields>,
  recovered: bool,
  wait_s: f64,
}

/// The end-derived fields: they appear on the `trigger_fired` line only when an end was recorded.
struct EndFields {
  end_at: time::OffsetDateTime,
  source: EndSource,
  lag_ms: i64,
  clock_offset_ms: i64,
}

/// Select one trigger's fire log from its remote end readings: the latest recorded end wins, and a job that ended
/// before the trigger was created lags from the creation instead.
fn build_fire_log(
  trigger: &PendingTrigger,
  fired_at: UtcDateTime,
  ends: &[RemoteTargetEnd],
  recovered: bool,
) -> FireLog {
  let created_at = trigger.created_at.into_inner();
  let seen_gone_at = ends.iter().map(|end| end.seen_gone_at).max();
  let end = ends
    .iter()
    .filter_map(|end| Some((end.end_at?, end.source?, end.clock_offset_ms?)))
    .max_by_key(|(end_at, _, _)| *end_at)
    .map(|(end_at, source, clock_offset_ms)| {
      let basis = end_at.max(created_at);
      EndFields { end_at, source, lag_ms: (fired_at.into_inner() - basis).whole_milliseconds() as i64, clock_offset_ms }
    });
  FireLog { seen_gone_at, end, recovered, wait_s: (fired_at.into_inner() - created_at).as_seconds_f64() }
}

/// The `trigger_fired` line of a remote-target trigger. The end-derived fields are absent when no end was
/// recorded; `seen_gone_at` is absent when the server never saw a remote target gone.
fn log_remote_fired(trigger: &PendingTrigger, reason: WaitReason, log: &FireLog) {
  let end_source = end_record::end_source_label(log.end.as_ref().map(|end| end.source));
  let wait_s = format!("{:.3}", log.wait_s);
  match (&log.end, log.seen_gone_at) {
    (Some(end), Some(seen_gone_at)) => tracing::info!(
      target: "charliebot_triggers",
      trigger_id = %trigger.id,
      session = %trigger.session_id,
      reason = %reason,
      recovered = %log.recovered,
      wait_s = %wait_s,
      seen_gone_at = %end_record::rfc3339(seen_gone_at),
      remote_end_at = %end_record::rfc3339(end.end_at),
      end_source = %end_source,
      lag_ms = end.lag_ms,
      clock_offset_ms = end.clock_offset_ms,
      "trigger_fired"
    ),
    (None, Some(seen_gone_at)) => tracing::info!(
      target: "charliebot_triggers",
      trigger_id = %trigger.id,
      session = %trigger.session_id,
      reason = %reason,
      recovered = %log.recovered,
      wait_s = %wait_s,
      seen_gone_at = %end_record::rfc3339(seen_gone_at),
      end_source = %end_source,
      "trigger_fired"
    ),
    (None, None) => tracing::info!(
      target: "charliebot_triggers",
      trigger_id = %trigger.id,
      session = %trigger.session_id,
      reason = %reason,
      recovered = %log.recovered,
      wait_s = %wait_s,
      end_source = %end_source,
      "trigger_fired"
    ),
    (Some(_), None) => unreachable!("an end reading carries the moment the server saw the target gone"),
  }
}

/// One remote target's end reading from the ssh that read the end back, or a warning naming the trigger when the
/// clock math fails. A failure never delays or blocks the fire.
fn end_reading_or_warn(
  end_remote_ns: i128,
  remote_now_ns: i128,
  local_midpoint_ns: i128,
  source: EndSource,
  seen_gone_at: time::OffsetDateTime,
  trigger_id: &str,
  target: &str,
) -> Option<RemoteTargetEnd> {
  match end_record::reading(end_remote_ns, remote_now_ns, local_midpoint_ns, source, seen_gone_at) {
    Some(end) => Some(end),
    None => {
      tracing::warn!(target: "charliebot_triggers", trigger_id, target, "remote_end_read_failed");
      None
    }
  }
}

/// The entry a target the server saw gone carries when no end was recorded.
fn seen_gone_only(seen_gone_at: time::OffsetDateTime) -> RemoteTargetEnd {
  RemoteTargetEnd { end_at: None, source: None, clock_offset_ms: None, seen_gone_at }
}

/// Whether one watch target reaches a remote host, and so gains the end fields on its `trigger_fired` line.
fn has_remote_target(targets: &[WatchTarget]) -> bool {
  targets
    .iter()
    .any(|target| matches!(target, WatchTarget::RemotePid { .. } | WatchTarget::SlurmJob { host: Some(_), .. }))
}

/// The remote command one pid probe runs. The end-recorder start and the end-file readback ride this command, so
/// the probe verdict stays the last stdout line the parser reads and no wake gains an ssh process.
fn pid_probe_command(target: &RemotePid, recorder: Option<&RecorderStart<'_>>, end_read: Option<&str>) -> String {
  let verdict = format!("kill -0 {} 2>&1 && echo {PROBE_ALIVE_WORD} || echo {PROBE_DEAD_WORD}", target.pid);
  let mut segments = Vec::new();
  if let Some(recorder) = recorder {
    segments.push(end_record::recorder_command(target.pid, recorder.trigger_id, recorder.max_wait_seconds));
  }
  if let Some(end_file) = end_read {
    segments.push(end_record::probe_read_prefix(end_file));
  }
  segments.push(verdict);
  segments.concat()
}

/// The remote command that reads back a pid the held session reported gone. It carries the same `kill -0` verdict
/// and, for a pid-only target, reads the end file the recorder writes. The recorder polls the pid on its own clock,
/// so the file can trail the held report by about a second; the read waits up to three seconds for it. The two
/// tagged dates bracket that wait, so the offset math keeps the ssh's own midpoint. A `--cmd` target has no recorder
/// file, because its end is the exit file `remote_finish` reads, so its readback carries the verdict alone.
fn pid_readback_command(pid: i64, end_file: Option<&str>) -> String {
  let read = end_file.map_or_else(String::new, |end_file| {
    format!(
      "n=0; while [ ! -s {end_file} ] && [ $n -lt 30 ]; do sleep 0.1; n=$((n + 1)); done; \
       cat {end_file} 2>/dev/null; rm -f {end_file}; "
    )
  });
  format!(
    "echo \"{start_tag} $(date +%s.%N)\"; if kill -0 {pid} >/dev/null 2>&1; then echo {PROBE_ALIVE_WORD}; \
     else {read}echo \"{end_tag} $(date +%s.%N)\"; echo {PROBE_DEAD_WORD}; fi",
    start_tag = end_record::READBACK_START_TAG,
    end_tag = end_record::READBACK_END_TAG,
  )
}

/// The argv of one remote command over the batch-mode ssh that the probes share. The control directory is the
/// process's own unless a test names one.
fn remote_ssh_argv(program: &str, control_dir: Option<&Path>, host: &str, remote: &str) -> io::Result<Vec<String>> {
  let remote_argv = [remote];
  let mut argv = match control_dir {
    Some(directory) => {
      ssh::ensure_control_dir(directory)?;
      ssh::argv(host, &remote_argv, directory)
    }
    None => ssh::ssh_cmd(host, &remote_argv)?,
  };
  argv[0] = program.to_string();
  Ok(argv)
}

/// The argv builder the held sessions use, with the same program and control directory the probes use.
fn ssh_argv_builder(program: String, control_dir: Option<PathBuf>) -> SshArgv {
  Arc::new(move |host: &str, remote: &str| remote_ssh_argv(&program, control_dir.as_deref(), host, remote))
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

/// Logs one session's trigger read that took [`SLOW_TRIGGER_RECOVERY_MS`] or more. One line per slow session: the time
/// covers the session's trigger-file stat walk and trigger loads, the work a slow boot's trigger phase waits in.
fn log_slow_trigger_recovery(session_dir: &Path, recovered: u64, read_time: std::time::Duration) {
  let elapsed_ms = read_time.as_millis() as u64;
  if elapsed_ms < SLOW_TRIGGER_RECOVERY_MS {
    return;
  }
  let session = session_dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
  tracing::info!(target: "charliebot_triggers", session, triggers = recovered, elapsed_ms, "trigger_recovery_session_slow");
}

/// One session's trigger files, each with its load result, in the stat walk's order.
struct SessionTriggers {
  dir: PathBuf,
  triggers: Vec<(PathBuf, anyhow::Result<PendingTrigger>)>,
  /// The non-pending files this session's walk proved, for the next boot's index.
  index_rows: Vec<TriggerIndexRow>,
  read_time: std::time::Duration,
}

/// The sessions [`read_session_triggers`] read, the rows they proved, and how the walk ended: an error ends it at the
/// session it names.
struct TriggerWalk {
  sessions: Vec<SessionTriggers>,
  index_rows: Vec<TriggerIndexRow>,
  end: anyhow::Result<()>,
}

/// Read every session's trigger files under `sessions_dir`, in directory order, until a stop request or an error. The
/// sessions are read in parallel, and the walk keeps those before the first error in directory order.
fn read_session_triggers(sessions_dir: &Path, stop: &StartupStop, index: &TriggerIndex) -> TriggerWalk {
  let none_read = |end| TriggerWalk { sessions: Vec::new(), index_rows: Vec::new(), end };
  let entries = match std::fs::read_dir(sessions_dir) {
    Ok(entries) => entries,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return none_read(Ok(())),
    Err(error) => {
      let end = Err(error).with_context(|| format!("read {}", sessions_dir.display()));
      return none_read(end);
    }
  };
  // The listing ends at its first error, and the sessions listed before it are read.
  let mut listed = Vec::new();
  let mut end = Ok(());
  for entry in entries {
    match entry {
      Ok(entry) => listed.push(entry),
      Err(error) => {
        end = Err(error.into());
        break;
      }
    }
  }
  let read_one = |entry: &std::fs::DirEntry| read_one_session_triggers(entry, index);
  let Some(reads) = parallel_walk::map_until(&listed, &|| stop.is_cancelled(), read_one) else {
    return none_read(Ok(()));
  };
  let mut sessions: Vec<SessionTriggers> = Vec::new();
  let mut index_rows = Vec::new();
  for read in reads {
    match read {
      Ok(Some(session)) => {
        index_rows.extend(session.index_rows.iter().cloned());
        sessions.push(session);
      }
      Ok(None) => {}
      // The sessions read before the error keep their place: their pending triggers start, and the error ends the step.
      Err(error) => return TriggerWalk { sessions, index_rows, end: Err(error) },
    }
  }
  TriggerWalk { sessions, index_rows, end }
}

/// One session directory's trigger files, each with its load result; `None` for an entry that is not a directory or
/// holds no triggers directory.
fn read_one_session_triggers(
  entry: &std::fs::DirEntry,
  index: &TriggerIndex,
) -> anyhow::Result<Option<SessionTriggers>> {
  if !entry.file_type()?.is_dir() {
    return Ok(None);
  }
  let started = std::time::Instant::now();
  let session_id = entry.file_name().to_string_lossy().into_owned();
  let directory = layout::triggers_dir(&entry.path());
  let files = match trigger_files::iter_trigger_file_stats(&directory) {
    Ok(files) => files,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(error).with_context(|| format!("scan {}", directory.display())),
  };
  let mut triggers = Vec::with_capacity(files.len());
  let mut index_rows = Vec::new();
  for (path, stat) in files {
    let file_name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let identity = TriggerFileIdentity::of(&stat);
    // A row the file still matches proves it not pending: the status can change only by replacing the file.
    if index.serves(&session_id, &file_name, identity) {
      index_rows.push(TriggerIndexRow { session_id: session_id.clone(), file_name, identity });
      continue;
    }
    let loaded = trigger_files::read_trigger(&path);
    if let Ok(trigger) = &loaded
      && trigger.status != TriggerStatus::Pending
    {
      index_rows.push(TriggerIndexRow { session_id: session_id.clone(), file_name, identity });
    }
    triggers.push((path, loaded));
  }
  Ok(Some(SessionTriggers { dir: entry.path(), triggers, index_rows, read_time: started.elapsed() }))
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

/// The route path that the `session trigger` command posts to. The router and `ROUTE_PATTERNS` use it, and so does the
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
    delay_seconds: integer_field(payload, DELAY_SECONDS_FIELD)?,
    message: payload["message"].as_str().expect("validated string field").to_string(),
    watch_targets: match payload.get(WATCH_TARGETS_FIELD) {
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

/// A validated integer field as the 64-bit field it is stored in. The validator reads any integer up to `u64::MAX`;
/// a value past `i64::MAX` has no stored form.
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
    self.timing.noise_max_seconds = 0;
    self.timing.sacct_interval = interval;
    self.timing.remote_sacct_grace = Duration::from_millis(100);
    self.timing.dormancy_interval = Duration::from_millis(20);
    self.held = self.held_for_tests(false, PRODUCTION_TIMING);
    self
  }

  /// Turn the held sessions on or off for a test, with the given silence and reconnect limits.
  fn with_held_session(mut self, enabled: bool, timing: held_session::HeldTiming) -> Self {
    self.held = self.held_for_tests(enabled, timing);
    self
  }

  fn held_for_tests(&self, enabled: bool, timing: held_session::HeldTiming) -> HeldSessions {
    let argv = ssh_argv_builder(self.probe_programs.ssh.clone(), self.ssh_control_dir.clone());
    HeldSessions::new(enabled, argv, timing, &SLURM_TERMINAL_STATES)
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
  use charliebot_core::test_support::LogCapture;
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
  use time::OffsetDateTime;
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
    Arc::new(fake_probe_manager(features, root, ssh_behavior, sacct_behavior, Duration::from_millis(2)))
  }

  /// A manager whose probe programs are the fake helper. The remote `sacct` of a held script finds the same fake by
  /// name on the helper's `PATH`, and the probes' interval is `interval`.
  fn fake_probe_manager(
    features: Arc<FeatureContext>,
    root: &Path,
    ssh_behavior: Value,
    sacct_behavior: Value,
    interval: Duration,
  ) -> TriggerManager {
    let bin = root.join("fake-bin");
    std::fs::create_dir_all(&bin).expect("fake bin directory");
    let helper = fake_probe_bin();
    for role in ["fake-ssh", "fake-sacct", "sacct"] {
      std::os::unix::fs::symlink(&helper, bin.join(role))
        .unwrap_or_else(|error| panic!("link fake program {role}: {error}"));
    }
    let behavior = json!({ "fake-ssh": ssh_behavior, "fake-sacct": sacct_behavior.clone(), "sacct": sacct_behavior });
    std::fs::write(bin.join("probe.json"), serde_json::to_vec(&behavior).expect("serialize probe behavior"))
      .expect("write probe behavior");
    TriggerManager::new(features).test_probe_setup(
      &bin.join("fake-ssh"),
      &bin.join("fake-sacct"),
      &root.join("synthetic-home/.ssh/controlmasters"),
      interval,
    )
  }

  /// How many held sessions the fake ssh has started: each one runs the held script as a `sh -c` remote command.
  fn held_session_starts(root: &Path) -> usize {
    let log = root.join("fake-bin/fake-ssh.argv.jsonl");
    if !log.exists() {
      return 0;
    }
    std::fs::read_to_string(log)
      .expect("read the fake ssh argv log")
      .lines()
      .filter(|line| line.contains("sh -c"))
      .count()
  }

  /// How many interval probes the fake ssh has run: each one names the pid's `kill -0` verdict.
  fn pid_probe_runs(root: &Path) -> usize {
    let log = root.join("fake-bin/fake-ssh.argv.jsonl");
    if !log.exists() {
      return 0;
    }
    std::fs::read_to_string(log)
      .expect("read the fake ssh argv log")
      .lines()
      .filter(|line| line.contains("kill -0"))
      .count()
  }

  async fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
      assert!(Instant::now() < deadline, "timed out waiting for {what}");
      sleep(Duration::from_millis(20)).await;
    }
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
  #[ignore = "integration"]
  fn exit_read_command_hands_the_remote_shell_the_same_path() {
    let dir = tempfile::tempdir().expect("temporary directory");
    for name in ["exit code.txt", "it's done", "quote\"d", "dollar$HOME", "semi;colon"] {
      let path = dir.path().join(name);
      // The wrapper writes the exit file with `echo`, which ends in a newline; the content line ends there.
      std::fs::write(&path, format!("{name}-status\n")).expect("write exit file");
      let command = end_record::exit_read_command(path.to_str().expect("UTF-8 path")).expect("path has no NUL byte");
      let output = std::process::Command::new("sh").arg("-c").arg(&command).output().expect("sh runs");
      let stdout = String::from_utf8_lossy(&output.stdout);
      assert_eq!(
        end_record::exit_read(&stdout).code.as_deref(),
        Some(&format!("{name}-status")[..]),
        "command: {command}"
      );
    }
    assert!(end_record::exit_read_command("bad\0path").is_err());
  }

  #[test]
  #[ignore = "integration"]
  fn open_pidfd_opens_a_live_process_and_reports_a_reaped_one_as_esrch() {
    assert!(open_pidfd(i64::from(std::process::id())).is_ok());
    let mut child = std::process::Command::new("true").spawn().expect("true starts");
    let pid = i64::from(child.id());
    child.wait().expect("true exits");
    let error = open_pidfd(pid).expect_err("a reaped process has no pidfd");
    assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
  }

  #[tokio::test]
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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

  /// The walk records the non-pending trigger files it read, and a later boot serves those rows instead of opening the
  /// files. A file rewritten after its row was written no longer matches the row, so the boot reads it and starts it.
  #[tokio::test]
  #[ignore = "integration"]
  async fn the_trigger_index_serves_a_file_status_until_the_file_is_rewritten() {
    let (home, features) = feature_context();
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    let mut filed = pending_trigger("synthetic-index-filed", SESSION_ID, near_future(3600), Vec::new());
    filed.status = TriggerStatus::Fired;
    manager.save_trigger(&filed).await.expect("save the filed trigger");
    manager.recover_pending().await.expect("the recording walk");

    let path = trigger_files::trigger_path(&features.store.session_dir(SESSION_ID), &filed.id);
    let file_name = format!("{}.json", filed.id);
    let stat = std::fs::metadata(&path).expect("the trigger file stats");
    let index = TriggerIndex::read(home.path());
    assert!(
      index.serves(SESSION_ID, &file_name, TriggerFileIdentity::of(&stat)),
      "the walk recorded the filed trigger"
    );

    crate::trigger_index::reset_served_count();
    manager.recover_pending().await.expect("the serving walk");
    assert!(crate::trigger_index::served_count() > 0, "the second walk served the filed file");
    assert!(
      !manager.tasks.lock().expect("trigger task map is not poisoned").contains_key(&filed.id),
      "a served file starts nothing"
    );

    // The status changes to pending by replacing the file, so the row no longer matches and the fit is read.
    let mut pending = filed.clone();
    pending.status = TriggerStatus::Pending;
    pending.fire_at = near_future(3600);
    manager.save_trigger(&pending).await.expect("the trigger returns to pending");
    manager.recover_pending().await.expect("the walk after the rewrite");
    assert!(
      manager.tasks.lock().expect("trigger task map is not poisoned").contains_key(&filed.id),
      "a rewritten file is read and its pending status starts the trigger"
    );
    manager.cancel_trigger(SESSION_ID, &filed.id).await.expect("cancel the recovered waiter");
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
    let (label, end) =
      manager.remote_finish(&target, "synthetic-trigger", &dead_probe(), time::OffsetDateTime::now_utc()).await;
    assert_eq!(label, "synthetic-host:41: exit 7, log /synthetic/remote log");
    assert_eq!(end.end_at, None, "a static-output fake prints no date or mtime line");
  }

  #[tokio::test]
  #[ignore = "integration"]
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
    let (label, _) =
      manager.remote_finish(&target, "synthetic-trigger", &dead_probe(), time::OffsetDateTime::now_utc()).await;
    assert_eq!(label, "synthetic-host:41: exit file missing");
  }

  #[tokio::test]
  #[ignore = "integration"]
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
    let (label, end) =
      manager.remote_finish(&target, "synthetic-trigger", &dead_probe(), time::OffsetDateTime::now_utc()).await;
    assert_eq!(label, "synthetic-host:41");
    assert_eq!(end.end_at, None, "a pid-only target without an end file records no end");
  }

  /// A DEAD probe outcome without any readback lines, for the label tests.
  fn dead_probe() -> PidProbeOutcome {
    PidProbeOutcome {
      status: ProbeStatus::Dead,
      raw: String::new(),
      read: end_record::PidProbeRead { now_ns: None, end_ns: None },
      local_mid_ns: None,
    }
  }

  #[tokio::test]
  #[ignore = "integration"]
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
    let (finished, still_alive, _ends) =
      manager.wait_with_remote_probe(&trigger, &[target]).await.expect("remote wait");
    assert!(finished.is_empty());
    assert_eq!(still_alive, ["synthetic-host:41, log /synthetic/remote log"]);
  }

  fn held_pid_target(pid: i64) -> RemotePid {
    RemotePid { kind: REMOTE_PID_KIND.to_string(), host: "synthetic-host".to_string(), pid, exit_file: None, log: None }
  }

  fn held_pid(pid: i64) -> HeldTarget {
    HeldTarget::Pid { host: "synthetic-host".to_string(), pid }
  }

  fn spawn_sleeper() -> std::process::Child {
    std::process::Command::new("sleep").arg("600").spawn().expect("spawn a live sleeper")
  }

  /// Kill and reap a sleeper. A reaped pid stops answering `kill -0`, as a finished remote job does.
  fn end_sleeper(mut child: std::process::Child) {
    child.kill().expect("kill the sleeper");
    child.wait().expect("reap the sleeper");
  }

  const SACCT_RUNNING_ROW: &str = "473328|RUNNING|0:0|\n";
  const SACCT_COMPLETED_ROW: &str = "473328|COMPLETED|0:0|2026-11-08T01:00:00\n";

  #[tokio::test]
  #[ignore = "integration"]
  async fn held_end_wakes_a_pid_wait_before_its_interval_probe() {
    let (home, features) = feature_context();
    let manager = fake_probe_manager(
      Arc::clone(&features),
      home.path(),
      json!({ "run_remote": true }),
      probe_behavior("", "", 0),
      Duration::from_secs(3600),
    )
    .with_held_session(true, PRODUCTION_TIMING);
    let child = spawn_sleeper();
    let pid = i64::from(child.id());
    let trigger = pending_trigger("synthetic-held-pid", SESSION_ID, near_future(3600), Vec::new());
    let killer = async {
      sleep(Duration::from_millis(300)).await;
      end_sleeper(child);
    };
    let targets = [held_pid_target(pid)];
    let (waited, ()) = tokio::join!(manager.wait_with_remote_probe(&trigger, &targets), killer);
    let (finished, still_alive, _ends) = waited.expect("remote wait");
    assert!(still_alive.is_empty());
    assert_eq!(finished, [format!("synthetic-host:{pid}")]);
    assert!(held_session_starts(home.path()) >= 1, "a held session carried the end");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn pid_end_reported_by_both_paths_wakes_once() {
    let (home, features) = feature_context();
    let manager = fake_probe_manager(
      Arc::clone(&features),
      home.path(),
      json!({ "run_remote": true }),
      probe_behavior("", "", 0),
      Duration::from_millis(2),
    )
    .with_held_session(true, PRODUCTION_TIMING);
    let child = spawn_sleeper();
    let pid = i64::from(child.id());
    let trigger = pending_trigger("synthetic-held-both", SESSION_ID, near_future(3600), Vec::new());
    let killer = async {
      sleep(Duration::from_millis(300)).await;
      end_sleeper(child);
    };
    let targets = [held_pid_target(pid)];
    let (waited, ()) = tokio::join!(manager.wait_with_remote_probe(&trigger, &targets), killer);
    let (finished, _still_alive, _ends) = waited.expect("remote wait");
    assert_eq!(finished.len(), 1, "an end that both paths report wakes the wait once");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn held_end_wakes_a_slurm_wait_before_its_interval_probe() {
    let (home, features) = feature_context();
    let sacct = json!({
      "sequence": [
        probe_behavior(SACCT_RUNNING_ROW, "", 0),
        probe_behavior(SACCT_RUNNING_ROW, "", 0),
        probe_behavior(SACCT_COMPLETED_ROW, "", 0),
      ]
    });
    let manager = fake_probe_manager(
      Arc::clone(&features),
      home.path(),
      json!({ "run_remote": true }),
      sacct,
      Duration::from_secs(3600),
    )
    .with_held_session(true, PRODUCTION_TIMING);
    let trigger = pending_trigger("synthetic-held-slurm", SESSION_ID, near_future(3600), Vec::new());
    let targets =
      [SlurmJob { kind: SLURM_JOB_KIND.to_string(), host: Some("synthetic-host".to_string()), job_id: 473328 }];
    let (finished, still_alive, _ends) = manager.wait_with_sacct(&trigger, &targets).await.expect("slurm wait");
    assert!(still_alive.is_empty());
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert!(finished[0].starts_with("synthetic-host:slurm:473328: COMPLETED"), "{finished:?}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn silent_held_session_is_killed_and_reconnects() {
    let (home, features) = feature_context();
    let timing =
      held_session::HeldTiming { silence: Duration::from_millis(300), reconnect_pause: Duration::from_millis(50) };
    let manager = fake_probe_manager(
      Arc::clone(&features),
      home.path(),
      json!({ "run_remote": true }),
      probe_behavior("", "", 0),
      Duration::from_secs(3600),
    )
    .with_held_session(true, timing);
    let child = spawn_sleeper();
    let pid = i64::from(child.id());
    let watch = manager.held.watch("synthetic-held-silence", vec![held_pid(pid)]);
    // The script prints its first heartbeat and then stays quiet for thirty seconds, so each session goes silent.
    wait_until(|| held_session_starts(home.path()) >= 3, "reconnects after silence").await;
    drop(watch);
    end_sleeper(child);
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn reopened_session_reports_an_end_inside_the_gap() {
    let (home, features) = feature_context();
    let manager = fake_probe_manager(
      Arc::clone(&features),
      home.path(),
      json!({ "run_remote": true }),
      probe_behavior("", "", 0),
      Duration::from_secs(3600),
    )
    .with_held_session(true, PRODUCTION_TIMING);
    let alive = spawn_sleeper();
    let alive_pid = i64::from(alive.id());
    let first = manager.held.watch("synthetic-held-first", vec![held_pid(alive_pid)]);
    wait_until(|| held_session_starts(home.path()) >= 1, "the first held session").await;
    // The second wait names a pid that ended before it registered. Only the reopened session's first pass can
    // report it, because the interval probe is an hour away.
    let gone = spawn_sleeper();
    let gone_pid = i64::from(gone.id());
    end_sleeper(gone);
    let mut second = manager.held.watch("synthetic-held-second", vec![held_pid(gone_pid)]);
    let end = timeout(Duration::from_secs(20), second.recv()).await.expect("the reopened session reports the end");
    assert_eq!(end, HeldEnd::Pid { host: "synthetic-host".to_string(), pid: gone_pid });
    assert!(held_session_starts(home.path()) >= 2, "the wait-set change reopened the session");
    drop(first);
    drop(second);
    end_sleeper(alive);
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_watch_that_names_a_reported_pid_reopens_the_session() {
    let (home, features) = feature_context();
    let manager = fake_probe_manager(
      Arc::clone(&features),
      home.path(),
      json!({ "run_remote": true }),
      probe_behavior("", "", 0),
      Duration::from_secs(3600),
    )
    .with_held_session(true, PRODUCTION_TIMING);
    let alive = spawn_sleeper();
    let alive_pid = i64::from(alive.id());
    let gone = spawn_sleeper();
    let gone_pid = i64::from(gone.id());
    let mut first = manager.held.watch("synthetic-held-existing", vec![held_pid(alive_pid), held_pid(gone_pid)]);
    wait_until(|| held_session_starts(home.path()) >= 1, "the first held session").await;
    end_sleeper(gone);
    let end = timeout(Duration::from_secs(20), first.recv()).await.expect("the first watch reports the end");
    assert_eq!(end, HeldEnd::Pid { host: "synthetic-host".to_string(), pid: gone_pid });
    // A second wait names the pid the running script already reported. Only a reopened session can report it again,
    // because the running script dropped the pid from its watch set; reusing it leaves this wait to the interval
    // probe, an hour away.
    let mut second = manager.held.watch("synthetic-held-late", vec![held_pid(gone_pid)]);
    let end = timeout(Duration::from_secs(20), second.recv()).await.expect("the reopened session reports the end");
    assert_eq!(end, HeldEnd::Pid { host: "synthetic-host".to_string(), pid: gone_pid });
    assert!(held_session_starts(home.path()) >= 2, "the late watch reopened the session");
    drop(second);
    drop(first);
    end_sleeper(alive);
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_resubscribed_pid_is_reported_again() {
    let (home, features) = feature_context();
    let manager = fake_probe_manager(
      Arc::clone(&features),
      home.path(),
      json!({ "run_remote": true }),
      probe_behavior("", "", 0),
      Duration::from_secs(3600),
    )
    .with_held_session(true, PRODUCTION_TIMING);
    let child = spawn_sleeper();
    let pid = i64::from(child.id());
    let mut watch = manager.held.watch("synthetic-held-rearm", vec![held_pid(pid)]);
    wait_until(|| held_session_starts(home.path()) >= 1, "the held session").await;
    end_sleeper(child);
    let end = timeout(Duration::from_secs(20), watch.recv()).await.expect("the first end");
    assert_eq!(end, HeldEnd::Pid { host: "synthetic-host".to_string(), pid });
    // A readback that still finds the pid alive takes the target back, and the reopened session reports it again.
    // Without the call the watch keeps no target and only the interval probe would see a later end.
    let starts = held_session_starts(home.path());
    watch.resubscribe(held_pid(pid));
    let end = timeout(Duration::from_secs(20), watch.recv()).await.expect("the reopened session reports the pid");
    assert_eq!(end, HeldEnd::Pid { host: "synthetic-host".to_string(), pid });
    assert!(held_session_starts(home.path()) > starts, "the re-armed target reopened the session");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn startup_recovery_opens_the_held_session_at_once() {
    let (home, features) = feature_context();
    let manager = Arc::new(
      fake_probe_manager(
        Arc::clone(&features),
        home.path(),
        json!({ "run_remote": true }),
        probe_behavior("", "", 0),
        Duration::from_secs(3600),
      )
      .with_held_session(true, PRODUCTION_TIMING),
    );
    let child = spawn_sleeper();
    let pid = i64::from(child.id());
    let targets = vec![WatchTarget::RemotePid { host: "synthetic-host".to_string(), pid, exit_file: None, log: None }];
    let trigger = pending_trigger("synthetic-held-recovery", SESSION_ID, near_future(3600), targets);
    manager.save_trigger(&trigger).await.expect("save the pending trigger");
    manager.recover_pending().await.expect("recover pending triggers");
    wait_until(|| held_session_starts(home.path()) >= 1, "the recovered held session").await;
    manager.cancel_trigger(SESSION_ID, &trigger.id).await.expect("cancel the recovered trigger");
    end_sleeper(child);
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn switch_off_runs_only_the_interval_probes() {
    let (home, features) = feature_context();
    let manager = fake_probe_manager(
      Arc::clone(&features),
      home.path(),
      json!({ "run_remote": true }),
      probe_behavior("", "", 0),
      Duration::from_millis(2),
    )
    .with_held_session(false, PRODUCTION_TIMING);
    let child = spawn_sleeper();
    let pid = i64::from(child.id());
    let trigger = pending_trigger("synthetic-held-switch-off", SESSION_ID, near_future(3600), Vec::new());
    let killer = async {
      sleep(Duration::from_millis(300)).await;
      end_sleeper(child);
    };
    let targets = [held_pid_target(pid)];
    let (waited, ()) = tokio::join!(manager.wait_with_remote_probe(&trigger, &targets), killer);
    let (finished, _still_alive, _ends) = waited.expect("remote wait");
    assert_eq!(finished.len(), 1);
    assert_eq!(held_session_starts(home.path()), 0, "no held session opens with the switch off");
    assert!(pid_probe_runs(home.path()) >= 1, "the interval probes carried the wait");
  }

  #[tokio::test]
  #[ignore = "integration"]
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
    let (finished, still_alive, _ends) = manager.wait_with_sacct(&trigger, &[target]).await.expect("remote SLURM wait");
    assert_eq!(finished, ["synthetic-host:slurm:123: COMPLETED 0:0"]);
    assert!(still_alive.is_empty());
  }

  #[tokio::test]
  #[ignore = "integration"]
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
    let (finished, still_alive, _ends) = manager.wait_with_sacct(&trigger, &[target]).await.expect("remote SLURM wait");
    assert!(finished.is_empty());
    assert_eq!(still_alive, ["synthetic-host:slurm:123 (unreachable 0m: sacct exit 255: synthetic ssh refusal)"]);
  }

  #[tokio::test]
  #[ignore = "integration"]
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
    let (finished, still_alive, _ends) = manager.wait_with_sacct(&trigger, &[target]).await.expect("local SLURM wait");
    assert_eq!(finished, ["slurm:123: COMPLETED 0:0"]);
    assert!(still_alive.is_empty());
  }

  #[test]
  fn a_cmd_readback_waits_for_no_recorder_file() {
    let remote = pid_readback_command(41, None);
    assert!(!remote.contains("watch-"), "a `--cmd` target has no recorder end file: {remote}");
    assert!(!remote.contains("sleep"), "a `--cmd` readback waits for nothing: {remote}");
    assert!(remote.contains("kill -0 41"), "{remote}");
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
  async fn fire_admits_one_durable_scheduled_input_before_stamping_fired() {
    let (_home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    install_dispatcher(&features);
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    let trigger = pending_trigger("synthetic-durable-trigger", SESSION_ID, near_future(-1), Vec::new());
    manager.save_trigger(&trigger).await.expect("save trigger");
    manager.wait_and_fire(trigger.clone(), false).await.expect("fire trigger");
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
  async fn fire_time_backstop_cancels_an_archived_target() {
    let (_home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
    let trigger = pending_trigger("synthetic-archived-trigger", SESSION_ID, near_future(0), Vec::new());
    manager.save_trigger(&trigger).await.expect("save trigger");
    archive_task(&features, SESSION_ID).await;
    manager.wait_and_fire(trigger.clone(), false).await.expect("dormant trigger retires");
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
  #[ignore = "integration"]
  async fn dormancy_watchdog_carries_its_reason_to_the_cancel_record() {
    let (_home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    archive_task(&features, SESSION_ID).await;
    let mut manager_value = TriggerManager::new(Arc::clone(&features));
    manager_value.timing.dormancy_interval = Duration::from_millis(2);
    let manager = Arc::new(manager_value);
    let trigger = pending_trigger("synthetic-watchdog-trigger", SESSION_ID, near_future(3600), Vec::new());
    manager.save_trigger(&trigger).await.expect("save trigger");
    manager.wait_and_fire(trigger.clone(), false).await.expect("watchdog cancels trigger");
    let fresh = manager.load_trigger(SESSION_ID, &trigger.id).await.expect("read cancelled trigger");
    assert_eq!(fresh.status, TriggerStatus::Cancelled);
    assert_eq!(fresh.fire_reason.as_deref(), Some("target task is archived"));
  }

  #[tokio::test]
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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

  /// Assert every `detail` entry's `type` and `loc`, in field order.
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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

  // ---- End-time readback: parsers and clock math ----

  #[test]
  fn epoch_and_zone_parsing_cover_the_forms_the_readbacks_print() {
    assert_eq!(end_record::parse_epoch_ns("1700000000.500000000"), Some(1_700_000_000_500_000_000));
    assert_eq!(end_record::parse_epoch_ns("1700000000.5"), Some(1_700_000_000_500_000_000));
    assert_eq!(end_record::parse_epoch_ns("7"), None, "an exit code is no epoch");
    assert_eq!(end_record::parse_epoch_ns("COMPLETED"), None);
    assert_eq!(end_record::parse_epoch_ns(""), None);
    assert_eq!(end_record::zone_seconds("+0000"), Some(0));
    assert_eq!(end_record::zone_seconds("-0700"), Some(-25_200));
    assert_eq!(end_record::zone_seconds("123|RUNNING|0:0"), None);
    assert_eq!(end_record::zone_seconds(""), None, "a blank line states no zone");
    assert_eq!(end_record::end_source_label(None), "none");
    assert_eq!(end_record::end_source_label(Some(EndSource::SacctEnd)), "sacct_end");
    assert_eq!(end_record::end_source_label(Some(EndSource::ExitFileMtime)), "exit_file_mtime");
    assert_eq!(end_record::end_source_label(Some(EndSource::EndRecorder)), "end_recorder");
  }

  #[test]
  fn sacct_end_converts_the_local_zone_value_to_a_remote_epoch() {
    let expected =
      |text: &str| OffsetDateTime::parse(text, &Rfc3339).expect("the expected instant parses").unix_timestamp_nanos();
    assert_eq!(end_record::sacct_end_remote_ns("2026-11-03T09:12:29", 0).unwrap(), expected("2026-11-03T09:12:29Z"));
    assert_eq!(
      end_record::sacct_end_remote_ns("2026-11-03T09:12:29", -25_200).unwrap(),
      expected("2026-11-03T16:12:29Z")
    );
    assert_eq!(end_record::sacct_end_remote_ns("Unknown", 0), None, "a running job states no end");
    assert_eq!(end_record::sacct_end_remote_ns("", 0), None);
  }

  #[test]
  fn pid_probe_read_takes_the_date_and_end_lines_between_it_and_the_verdict() {
    let read = end_record::pid_probe_read("1700000100.0\n1700000060.5\nDEAD\n");
    assert_eq!(read.now_ns, Some(1_700_000_100_000_000_000));
    assert_eq!(read.end_ns, Some(1_700_000_060_500_000_000));
    let read = end_record::pid_probe_read("1700000100.0\nALIVE\n");
    assert_eq!(read.now_ns, Some(1_700_000_100_000_000_000));
    assert_eq!(read.end_ns, None);
    let read = end_record::pid_probe_read("ALIVE\n");
    assert_eq!(read.now_ns, None, "a probe without the read prefix states no clock");
    assert_eq!(read.end_ns, None);
    // A DEAD probe whose verdict failed prints the shell's kill error between the date line and the verdict.
    let read = end_record::pid_probe_read("1700000100.0\nbash: kill: (41) - No such process\nDEAD\n");
    assert_eq!(read.now_ns, Some(1_700_000_100_000_000_000));
    assert_eq!(read.end_ns, None);
  }

  #[test]
  fn exit_read_keeps_the_whole_stdout_as_the_code_without_a_date_line() {
    // A static-output fake prints only the exit file, which is all the exit read ever printed.
    let read = end_record::exit_read("7\n");
    assert_eq!(read.code.as_deref(), Some("7"));
    assert_eq!(read.mtime_ns, None);
    assert_eq!(read.now_ns, None);
    let read = end_record::exit_read("1700000100.0\n7\n1700000060.5\n");
    assert_eq!(read.code.as_deref(), Some("7"));
    assert_eq!(read.mtime_ns, Some(1_700_000_060_500_000_000));
    assert_eq!(read.now_ns, Some(1_700_000_100_000_000_000));
    // A missing exit file prints only the date line.
    let read = end_record::exit_read("1700000100.0\n");
    assert_eq!(read.code, None);
    assert_eq!(read.mtime_ns, None);
  }

  #[test]
  fn the_offset_math_places_the_remote_end_on_the_server_clock() {
    let started = SystemTime::UNIX_EPOCH + Duration::from_millis(1_000_000_000_000);
    let ended = started + Duration::from_millis(700);
    let mid = end_record::local_midpoint_ns(started, ended).expect("post-epoch instants convert");
    assert_eq!(mid, 1_000_000_000_000_000_000 + 350_000_000);
    let remote_now = mid + 400_000_000; // the remote clock is 400 ms ahead
    let seen_gone = OffsetDateTime::from_unix_timestamp(1).expect("a fixed instant");
    let end = end_record::reading(1_700_000_060_500_000_000, remote_now, mid, EndSource::EndRecorder, seen_gone)
      .expect("the reading converts");
    assert_eq!(end.clock_offset_ms, Some(400));
    // end_server = end_remote − offset.
    assert_eq!(end.end_at.unwrap().unix_timestamp_nanos(), 1_700_000_060_500_000_000 - 400_000_000);
  }

  #[test]
  fn recorder_command_detaches_like_the_cmd_launcher_and_bounds_itself() {
    let command = end_record::recorder_command(41, "abc-123", 3600);
    assert!(
      command.contains("setsid sh -c 'timeout 3600 tail --pid=41 -f /dev/null"),
      "bounded by the trigger's max wait: {command}"
    );
    assert!(command.contains("</dev/null >/dev/null 2>&1 &"), "fully detached: {command}");
    assert!(
      command.contains("$HOME/charliebot_runs/watch-abc-123/41.end"),
      "end file under the remote home: {command}"
    );
    assert!(command.ends_with("; "), "the probe verdict follows the recorder: {command}");
    let probe = format!("{command}kill -0 41 2>&1 && echo ALIVE || echo DEAD");
    assert!(probe.lines().last().expect("one line").ends_with("echo DEAD"), "verdict stays last: {probe}");
    assert_eq!(end_record::end_file_word("abc-123", 41), "\"$HOME/charliebot_runs/watch-abc-123/41.end\"");
  }

  // ---- End-time readback: the fire log ----

  #[test]
  fn fire_log_takes_the_latest_end_and_lags_from_creation_when_the_end_precedes_it() {
    let trigger = pending_trigger(
      "synthetic-fire-log",
      SESSION_ID,
      near_future(10),
      vec![WatchTarget::RemotePid { host: "synthetic-host".to_string(), pid: 41, exit_file: None, log: None }],
    );
    let created = trigger.created_at.into_inner();
    let seen_gone = |offset: time::Duration| created + offset;
    let early = RemoteTargetEnd {
      end_at: Some(created - time::Duration::seconds(5)),
      source: Some(EndSource::SacctEnd),
      clock_offset_ms: Some(10),
      seen_gone_at: seen_gone(time::Duration::seconds(1)),
    };
    let late = RemoteTargetEnd {
      end_at: Some(created + time::Duration::seconds(30)),
      source: Some(EndSource::EndRecorder),
      clock_offset_ms: Some(20),
      seen_gone_at: seen_gone(time::Duration::seconds(31)),
    };
    let fired_at = UtcDateTime::from(created + time::Duration::seconds(60));
    let log = build_fire_log(&trigger, fired_at, &[early, late], false);
    let end = log.end.expect("the latest end wins");
    assert_eq!(end.source, EndSource::EndRecorder);
    assert_eq!(end.clock_offset_ms, 20);
    assert_eq!(end.end_at, created + time::Duration::seconds(30));
    assert_eq!(end.lag_ms, 30_000);
    assert_eq!(log.seen_gone_at, Some(created + time::Duration::seconds(31)));
    assert!(!log.recovered);
    assert!((log.wait_s - 60.0).abs() < 0.001);
    // No recorded end: the end-derived fields stay absent and the label reads none.
    let seen = seen_gone_only(created);
    let log = build_fire_log(&trigger, fired_at, &[seen], true);
    assert!(log.end.is_none());
    assert_eq!(end_record::end_source_label(log.end.as_ref().map(|end| end.source)), "none");
    assert_eq!(log.seen_gone_at, Some(created));
    assert!(log.recovered);
    let log = build_fire_log(&trigger, fired_at, &[], false);
    assert!(log.end.is_none() && log.seen_gone_at.is_none(), "a timeout fire with nothing seen gone logs neither");
  }

  // ---- End-time readback: scripted ssh sequences ----

  /// The fixed end instant a scripted sequence writes, far before any test's creation time, as one epoch.ns line.
  const SCRIPTED_END_NS: i128 = 1_700_000_060_500_000_000;

  /// The same instant as the epoch.ns line the recorder writes.
  fn scripted_end_line() -> String {
    format!("{}.500000000", SCRIPTED_END_NS / 1_000_000_000)
  }

  fn sequence(entries: serde_json::Value) -> Value {
    json!({ "sequence": entries })
  }

  fn scripted_manager(
    features: Arc<FeatureContext>,
    root: &Path,
    ssh_sequence: Value,
    sacct_sequence: Value,
  ) -> Arc<TriggerManager> {
    fake_manager(features, root, sequence(ssh_sequence), sequence(sacct_sequence))
  }

  fn fake_calls(root: &Path, role: &str) -> usize {
    std::fs::read_to_string(root.join("fake-bin").join(format!("{role}.calls")))
      .expect("the fake recorded its call count")
      .trim()
      .parse()
      .expect("the call count parses")
  }

  /// One JSON array of argv per invocation of the fake program.
  fn fake_argv(root: &Path, role: &str) -> Vec<Vec<String>> {
    std::fs::read_to_string(root.join("fake-bin").join(format!("{role}.argv.jsonl")))
      .expect("the fake recorded its argv")
      .lines()
      .map(|line| serde_json::from_str(line).expect("an argv line parses"))
      .collect()
  }

  fn log_line_count(text: &str, line: &str) -> usize {
    text.lines().filter(|logged| logged.contains(line)).count()
  }

  /// One `key=value` field of the rendered log text, up to the next space or newline.
  fn log_field(text: &str, field: &str) -> String {
    let start = text.find(&format!("{field}=")).expect("the log line names the field") + field.len() + 1;
    let rest = &text[start..];
    let end = rest.find([' ', '\n']).unwrap_or(rest.len());
    rest[..end].to_string()
  }

  async fn wait_until_fired(manager: &Arc<TriggerManager>, trigger_id: &str) -> PendingTrigger {
    for _ in 0..500 {
      sleep(Duration::from_millis(10)).await;
      if let Ok(trigger) = manager.load_trigger(SESSION_ID, trigger_id).await
        && trigger.status == TriggerStatus::Fired
      {
        return trigger;
      }
    }
    panic!("trigger {trigger_id} never fired");
  }

  /// Wait until the capture holds one trigger's `trigger_fired` line. `stamp_fired` writes the Fired record before it
  /// emits that line, so a test that read the capture the moment the record turned Fired could still miss it.
  async fn wait_for_fire_line(log: &LogCapture, trigger_id: &str) {
    for _ in 0..500 {
      if log.text().lines().any(|line| line.contains("trigger_fired") && line.contains(trigger_id)) {
        return;
      }
      sleep(Duration::from_millis(10)).await;
    }
    panic!("the trigger_fired line for {trigger_id} never appeared; captured:\n{}", log.text());
  }

  /// A scripted pid-only wake, end to end: the verify ssh starts the end recorder, the probes wake on the
  /// ALIVE→DEAD transition, and the DEAD probe reads the end file back in the same ssh.
  #[tokio::test]
  #[ignore = "integration"]
  async fn scripted_remote_pid_wake_wakes_and_logs_the_recorded_end() {
    let (home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    install_dispatcher(&features);
    let end_line = scripted_end_line();
    let ssh_sequence = json!([
      { "stdout": "ALIVE\n", "stderr": "", "exit": 0 },
      { "stdout": "$NOW_NS\nALIVE\n", "stderr": "", "exit": 0 },
      { "stdout": format!("$NOW_NS\n{end_line}\nDEAD\n"), "stderr": "", "exit": 0 },
    ]);
    let manager = scripted_manager(Arc::clone(&features), home.path(), ssh_sequence, json!([]));
    let log = LogCapture::default();
    let _capture = log.install();

    let trigger = manager
      .create_trigger(
        SESSION_ID,
        3600,
        "scripted wake",
        Some(vec![WatchTarget::RemotePid { host: "synthetic-host".to_string(), pid: 41, exit_file: None, log: None }]),
        None,
        None,
        true,
      )
      .await
      .expect("the scripted wake registers");
    let fired = wait_until_fired(&manager, &trigger.id).await;
    assert_eq!(fired.fire_reason.as_deref(), Some("completed"), "the wait that wakes today still wakes");

    // ssh processes per wake: the same 3 the before build starts (1 verify + 2 probe rounds), each logged once.
    assert_eq!(fake_calls(home.path(), "fake-ssh"), 3);
    let started_lines = log_line_count(&log.text(), "remote_ssh_started");
    assert_eq!(started_lines, 3, "{}", log.text());
    let purposes: Vec<String> = log
      .text()
      .lines()
      .filter(|line| line.contains("remote_ssh_started"))
      .map(|line| log_field(line, "purpose"))
      .collect();
    assert_eq!(purposes, ["verify", "probe", "probe"], "{}", log.text());
    assert_eq!(fake_argv(home.path(), "fake-ssh").len(), 3, "every logged start names one ssh process");

    // The recorder rode the verify ssh, and the end readback rode the DEAD probe: no extra ssh per wake.
    let argv = fake_argv(home.path(), "fake-ssh");
    let verify_command = argv[0].last().expect("the remote command");
    assert!(verify_command.contains(&format!("watch-{}/41.end", trigger.id)), "{verify_command}");
    assert!(verify_command.contains("setsid sh -c 'timeout 3600 tail --pid=41"), "{verify_command}");
    let dead_command = argv[2].last().expect("the remote command");
    assert!(
      dead_command.contains(&format!("cat \"$HOME/charliebot_runs/watch-{}/41.end\"", trigger.id)),
      "{dead_command}"
    );
    assert!(
      dead_command.contains(&format!("rm -f \"$HOME/charliebot_runs/watch-{}/41.end\"", trigger.id)),
      "{dead_command}"
    );

    // The trigger_fired line carries the new fields, and the end converts by the offset it measured in the same ssh.
    wait_for_fire_line(&log, &trigger.id).await;
    let text = log.text();
    let fired_lines: Vec<&str> =
      text.lines().filter(|line| line.contains("trigger_fired") && line.contains(&trigger.id)).collect();
    assert_eq!(fired_lines.len(), 1, "{text}");
    let line = fired_lines[0];
    for field in [
      "remote_end_at=",
      "end_source=end_recorder",
      "lag_ms=",
      "clock_offset_ms=",
      "seen_gone_at=",
      "recovered=false",
      "wait_s=",
    ] {
      assert!(line.contains(field), "missing {field}: {line}");
    }
    let offset_ms: i128 = log_field(line, "clock_offset_ms").parse().expect("the offset is an integer");
    let remote_end_at = log_field(line, "remote_end_at");
    let end_ns =
      OffsetDateTime::parse(&remote_end_at, &Rfc3339).expect("the end instant parses").unix_timestamp_nanos();
    assert_eq!(end_ns, SCRIPTED_END_NS - offset_ms * 1_000_000);
    let lag_ms: i64 = log_field(line, "lag_ms").parse().expect("the lag is an integer");
    assert!(lag_ms >= 0, "the job ended before the trigger was created, so the lag runs from creation: {line}");
    let wait_s: f64 = log_field(line, "wait_s").parse().expect("the wait is a float");
    assert!(wait_s > 0.0, "{line}");
  }

  /// The same scripted wake after boot recovery: the waiter the recovery started logs `recovered=true`.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_recovered_remote_wake_logs_recovered_true() {
    let (home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    install_dispatcher(&features);
    let ssh_sequence = json!([
      { "stdout": "$NOW_NS\nALIVE\n", "stderr": "", "exit": 0 },
      { "stdout": format!("$NOW_NS\n{}\nDEAD\n", scripted_end_line()), "stderr": "", "exit": 0 },
    ]);
    let manager = scripted_manager(Arc::clone(&features), home.path(), ssh_sequence, json!([]));
    let trigger = pending_trigger(
      "synthetic-recovered-remote",
      SESSION_ID,
      near_future(60),
      vec![WatchTarget::RemotePid { host: "synthetic-host".to_string(), pid: 41, exit_file: None, log: None }],
    );
    manager.save_trigger(&trigger).await.expect("save the pending trigger");
    let log = LogCapture::default();
    let _capture = log.install();
    manager.recover_pending().await.expect("recover pending triggers");
    let fired = wait_until_fired(&manager, &trigger.id).await;
    assert_eq!(fired.fire_reason.as_deref(), Some("completed"));
    wait_for_fire_line(&log, &trigger.id).await;
    let text = log.text();
    let line =
      text.lines().find(|line| line.contains("trigger_fired") && line.contains(&trigger.id)).expect("the fire logged");
    assert!(line.contains("recovered=true"), "{line}");
    assert!(line.contains("end_source=end_recorder"), "{line}");
  }

  /// A scripted remote slurm wake: the sacct readback ssh that answers the terminal state also carries the clock
  /// and zone lines the `End` conversion reads.
  #[tokio::test]
  #[ignore = "integration"]
  async fn scripted_remote_slurm_wake_reads_the_sacct_end() {
    let (home, features) = feature_context();
    let end_raw = "2026-11-03T09:12:29";
    let ssh_sequence = json!([
      { "stdout": "$NOW_NS\n+0000\n123|RUNNING|0:0|\n", "stderr": "", "exit": 0 },
      { "stdout": format!("$NOW_NS\n+0000\n123|COMPLETED|0:0|{end_raw}\n"), "stderr": "", "exit": 0 },
    ]);
    let manager = scripted_manager(Arc::clone(&features), home.path(), ssh_sequence, json!([]));
    let trigger = pending_trigger("synthetic-scripted-slurm", SESSION_ID, near_future(10), Vec::new());
    let log = LogCapture::default();
    let _capture = log.install();
    let target = SlurmJob { kind: "slurm_job".to_string(), host: Some("synthetic-host".to_string()), job_id: 123 };
    let (finished, still_alive, ends) = manager.wait_with_sacct(&trigger, &[target]).await.expect("remote SLURM wait");
    assert_eq!(finished, ["synthetic-host:slurm:123: COMPLETED 0:0"]);
    assert!(still_alive.is_empty());
    assert_eq!(fake_calls(home.path(), "fake-ssh"), 2, "one ssh per probe round, as the before build starts");
    assert_eq!(log_line_count(&log.text(), "remote_ssh_started"), 2, "{}", log.text());
    assert_eq!(ends.len(), 1);
    let end = &ends[0];
    assert_eq!(end.source, Some(EndSource::SacctEnd));
    let expected_utc =
      OffsetDateTime::parse("2026-11-03T09:12:29Z", &Rfc3339).expect("the end instant parses").unix_timestamp_nanos();
    let offset_ms = i128::from(end.clock_offset_ms.expect("the readback measured an offset"));
    assert_eq!(end.end_at.expect("the end converts").unix_timestamp_nanos(), expected_utc - offset_ms * 1_000_000);
  }

  /// A scripted `--cmd` wake: the exit read it pays anyway also reads the file's mtime as the end instant.
  #[tokio::test]
  #[ignore = "integration"]
  async fn scripted_remote_cmd_wake_reads_the_exit_file_mtime() {
    let (home, features) = feature_context();
    let ssh_sequence = json!([
      { "stdout": "ALIVE\n", "stderr": "", "exit": 0 },
      { "stdout": "DEAD\n", "stderr": "", "exit": 0 },
      { "stdout": "$NOW_NS\n7\n1700000060.500000000\n", "stderr": "", "exit": 0 },
    ]);
    let manager = scripted_manager(Arc::clone(&features), home.path(), ssh_sequence, json!([]));
    let trigger = pending_trigger("synthetic-scripted-cmd", SESSION_ID, near_future(10), Vec::new());
    let log = LogCapture::default();
    let _capture = log.install();
    let target = RemotePid {
      kind: "remote_pid".to_string(),
      host: "synthetic-host".to_string(),
      pid: 41,
      exit_file: Some("/synthetic/exit".to_string()),
      log: None,
    };
    let (finished, still_alive, ends) = manager.wait_with_remote_probe(&trigger, &[target]).await.expect("remote wait");
    assert_eq!(finished, ["synthetic-host:41: exit 7"]);
    assert!(still_alive.is_empty());
    // ssh processes per wake: the same 3 the before build starts (2 probe rounds + 1 exit read).
    assert_eq!(fake_calls(home.path(), "fake-ssh"), 3);
    assert_eq!(log_line_count(&log.text(), "remote_ssh_started"), 3, "{}", log.text());
    assert!(ends[0].source == Some(EndSource::ExitFileMtime));
    let offset_ms = i128::from(ends[0].clock_offset_ms.expect("the readback measured an offset"));
    assert_eq!(
      ends[0].end_at.expect("the end converts").unix_timestamp_nanos(),
      1_700_000_060_500_000_000 - offset_ms * 1_000_000
    );
    let purposes: Vec<String> = log
      .text()
      .lines()
      .filter(|line| line.contains("remote_ssh_started"))
      .map(|line| log_field(line, "purpose"))
      .collect();
    assert_eq!(purposes, ["probe", "probe", "readback"], "{}", log.text());
  }

  /// A scripted pid-only wake whose recorder left no end file: the fire is untouched and the warning names the
  /// trigger, and the line reads `end_source=none` without the end-derived fields.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_dead_probe_without_an_end_file_warns_and_fires_without_end_fields() {
    let (home, features) = feature_context();
    add_task_node(&features, SESSION_ID).await;
    install_dispatcher(&features);
    let ssh_sequence = json!([
      { "stdout": "ALIVE\n", "stderr": "", "exit": 0 },
      { "stdout": "$NOW_NS\nALIVE\n", "stderr": "", "exit": 0 },
      { "stdout": "$NOW_NS\nDEAD\n", "stderr": "", "exit": 0 },
    ]);
    let manager = scripted_manager(Arc::clone(&features), home.path(), ssh_sequence, json!([]));
    let log = LogCapture::default();
    let _capture = log.install();
    let trigger = manager
      .create_trigger(
        SESSION_ID,
        3600,
        "wake without an end file",
        Some(vec![WatchTarget::RemotePid { host: "synthetic-host".to_string(), pid: 41, exit_file: None, log: None }]),
        None,
        None,
        true,
      )
      .await
      .expect("the scripted wake registers");
    let fired = wait_until_fired(&manager, &trigger.id).await;
    assert_eq!(fired.fire_reason.as_deref(), Some("completed"), "the missing end file never blocks the fire");
    wait_for_fire_line(&log, &trigger.id).await;
    let text = log.text();
    let warning = text.lines().find(|line| line.contains("remote_end_read_failed")).expect("the warning logged");
    assert!(warning.contains(&trigger.id), "{warning}");
    let line = text.lines().find(|line| line.contains("trigger_fired")).expect("the fire logged");
    assert!(line.contains("end_source=none"), "{line}");
    for absent in ["remote_end_at=", "lag_ms=", "clock_offset_ms="] {
      assert!(!line.contains(absent), "{absent} must be absent: {line}");
    }
    assert!(line.contains("seen_gone_at="), "the server did see the target gone: {line}");
  }
}
