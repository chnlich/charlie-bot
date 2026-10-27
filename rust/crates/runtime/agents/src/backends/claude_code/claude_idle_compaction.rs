//! Timed, same-model compaction for idle Claude master sessions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types;
use charliebot_registry::Registry;
use charliebot_registry::backend::{
  CompactionOutcome, CompactionRun, CompactionRunFn, Env, IdleArmContext, LaunchArgs,
};
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::{SessionStatus, UtcDateTime};
use charliebot_store::run_records::RunKind;
use futures_util::future::BoxFuture;
use serde_json::{Map, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use super::{
  EXCLUDE_DYNAMIC_SECTIONS_FLAG, claude_accounts, claude_compaction, claude_config, claude_metadata, login_dirs,
};

pub const IDLE_DELAY: Duration = Duration::from_secs(54 * 60);

pub type InspectFn = Arc<dyn Fn(UtcDateTime) -> anyhow::Result<Option<IdleCandidate>> + Send + Sync>;
pub type BusyFn = Arc<dyn Fn(&str) -> anyhow::Result<bool> + Send + Sync>;
pub type RunFn = Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// Facts used by the idle-compaction decision.
#[derive(Clone, Debug)]
pub struct IdleFacts {
  pub archived: bool,
  pub backend_unchanged: bool,
  pub turn_busy: bool,
  pub armed_at: Option<UtcDateTime>,
  pub last_request_at: Option<UtcDateTime>,
  pub compacted_after_request: bool,
  pub context_tokens: Option<i64>,
  pub resume_id: Option<String>,
  pub transcript_present: bool,
}

/// One refreshed candidate and the operation to run if every gate holds.
pub struct IdleCandidate {
  pub facts: IdleFacts,
  pub idle_tokens: u64,
  pub run: RunFn,
}

/// Session identity and turn kind used to construct the resumed Claude process.
pub struct IdleProcessIdentity {
  pub session_id: String,
  pub cc_session_id: String,
  pub run_kind: RunKind,
}

/// Session-specific timer callbacks supplied by the task layer.
#[derive(Clone)]
pub struct IdleTimerInput {
  pub session_id: String,
  pub armed_at: UtcDateTime,
  pub inspect: InspectFn,
  pub is_busy: BusyFn,
}

struct Timer {
  task: JoinHandle<()>,
  input: IdleTimerInput,
}

/// A running compaction: its id, and its outcome once it ends (None while it runs).
struct Running {
  id: u64,
  done: watch::Receiver<Option<Result<(), String>>>,
}

#[derive(Default)]
struct State {
  timers: HashMap<String, Timer>,
  running: HashMap<String, Running>,
  next_running_id: u64,
}

/// Owns pending idle timers and running compactions.
#[derive(Clone, Default)]
pub struct IdleCompaction {
  state: Arc<Mutex<State>>,
}

impl IdleCompaction {
  pub fn new() -> Self {
    Self::default()
  }

  /// The server-wide idle timer owner used by lifecycle placement and the task queue.
  pub fn shared() -> &'static Self {
    static IDLE: OnceLock<IdleCompaction> = OnceLock::new();
    IDLE.get_or_init(IdleCompaction::new)
  }

  /// Replace any timer for this session and schedule the supplied timer after `delay`.
  pub async fn arm_for_test(&self, input: IdleTimerInput, delay: Duration) {
    self.arm_after(input, delay);
  }

  /// Replace any timer for this session and schedule it for the supplied request time plus `IDLE_DELAY`.
  pub fn arm(&self, input: IdleTimerInput) {
    let remaining = input.armed_at.into_inner() + time::Duration::seconds(IDLE_DELAY.as_secs() as i64)
      - UtcDateTime::now().into_inner();
    let delay = Duration::from_nanos(remaining.whole_nanoseconds().max(0) as u64);
    self.arm_after(input, delay);
  }

  fn arm_after(&self, input: IdleTimerInput, delay: Duration) {
    let session_id = input.session_id.clone();
    let stored_input = input.clone();
    self.disarm(&session_id);
    let manager = self.clone();
    let task_session_id = session_id.clone();
    let (start, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
      ready.await.expect("idle timer is released after its handle is stored");
      sleep(delay).await;
      match manager.fire_at_inner(&task_session_id, UtcDateTime::now(), true) {
        Ok(Some(compaction)) => {
          if let Err(error) = compaction.await {
            tracing::error!(target: "charliebot_agents", session = %task_session_id, error = %error, "claude_idle_compaction_task_join_failed");
          }
        }
        Ok(None) => {}
        Err(error) => {
          tracing::error!(target: "charliebot_agents", session = %task_session_id, error = %error, "claude_idle_compaction_trigger_failed");
        }
      }
    });
    self
      .state
      .lock()
      .expect("idle compaction state mutex is not poisoned")
      .timers
      .insert(session_id, Timer { task, input: stored_input });
    start.send(()).expect("idle timer task is waiting for its start signal");
  }

  /// Disarm a pending timer. A running compaction keeps going.
  pub fn disarm(&self, session_id: &str) {
    if let Some(timer) =
      self.state.lock().expect("idle compaction state mutex is not poisoned").timers.remove(session_id)
    {
      timer.task.abort();
    }
  }

  /// Evaluate a timer at a supplied instant; return the spawned compaction when it qualifies.
  pub fn fire_at(&self, session_id: &str, now: UtcDateTime) -> anyhow::Result<Option<JoinHandle<()>>> {
    self.fire_at_inner(session_id, now, false)
  }

  fn fire_at_inner(
    &self,
    session_id: &str,
    now: UtcDateTime,
    fired_by_timer: bool,
  ) -> anyhow::Result<Option<JoinHandle<()>>> {
    let (timer, already_running) = {
      let mut state = self.state.lock().expect("idle compaction state mutex is not poisoned");
      (state.timers.remove(session_id), state.running.contains_key(session_id))
    };
    let Some(timer) = timer else {
      return Ok(None);
    };
    if !fired_by_timer {
      timer.task.abort();
    }
    if already_running {
      return Ok(None);
    }
    let Some(mut candidate) = (timer.input.inspect)(now)? else {
      return Ok(None);
    };
    candidate.facts.turn_busy = false;
    if !should_compact(&candidate.facts, candidate.idle_tokens, now) {
      return Ok(None);
    }
    if (timer.input.is_busy)(session_id)? {
      return Ok(None);
    }
    let run = Arc::clone(&candidate.run);
    let (done_sender, done_receiver) = watch::channel(None);
    let run_id = {
      let mut state = self.state.lock().expect("idle compaction state mutex is not poisoned");
      if state.running.contains_key(session_id) {
        return Ok(None);
      }
      state.next_running_id += 1;
      let id = state.next_running_id;
      state.running.insert(session_id.to_string(), Running { id, done: done_receiver });
      id
    };
    let manager = self.clone();
    let session_id = session_id.to_string();
    let task = tokio::spawn(async move {
      // The run gets its own task, so a panic in it ends that task and reaches here as a join error.
      let outcome = match tokio::spawn(async move { run().await }).await {
        Ok(result) => result.map_err(|error| format!("{error:#}")),
        Err(error) => Err(format!("idle compaction task failed: {error}")),
      };
      if let Err(message) = &outcome {
        tracing::error!(target: "charliebot_agents", session = %session_id, error = %message, "claude_idle_compaction_run_failed");
      }
      // The entry goes before the outcome is sent, so a turn that starts next finds no running compaction.
      {
        let mut state = manager.state.lock().expect("idle compaction state mutex is not poisoned");
        if state.running.get(&session_id).is_some_and(|running| running.id == run_id) {
          state.running.remove(&session_id);
        }
      }
      done_sender.send_replace(Some(outcome));
    });
    Ok(Some(task))
  }

  /// Cancel a pending timer and wait for a running compaction without cancelling that compaction. A compaction that
  /// ended with an error fails this turn with that error, as the shielded await in Python re-raises it.
  pub async fn settle_for_turn(&self, session_id: &str) -> anyhow::Result<()> {
    self.disarm(session_id);
    let Some(mut done) = self
      .state
      .lock()
      .expect("idle compaction state mutex is not poisoned")
      .running
      .get(session_id)
      .map(|running| running.done.clone())
    else {
      return Ok(());
    };
    let finished = done.wait_for(Option::is_some).await.context("waiting for idle compaction to finish")?;
    let outcome = finished.clone().expect("wait_for returns once the compaction has an outcome");
    outcome.map_err(|message| anyhow::anyhow!(message))
  }

  pub fn timer_count(&self) -> usize {
    self.state.lock().expect("idle compaction state mutex is not poisoned").timers.len()
  }

  pub fn running_count(&self) -> usize {
    self.state.lock().expect("idle compaction state mutex is not poisoned").running.len()
  }

  pub fn has_timer(&self, session_id: &str) -> bool {
    self.state.lock().expect("idle compaction state mutex is not poisoned").timers.contains_key(session_id)
  }

  pub fn is_running(&self, session_id: &str) -> bool {
    self.state.lock().expect("idle compaction state mutex is not poisoned").running.contains_key(session_id)
  }

  /// The abort handle of the session's pending timer, for a test that checks the timer was cancelled.
  #[cfg(test)]
  pub(crate) fn timer_abort_handle(&self, session_id: &str) -> Option<tokio::task::AbortHandle> {
    self
      .state
      .lock()
      .expect("idle compaction state mutex is not poisoned")
      .timers
      .get(session_id)
      .map(|timer| timer.task.abort_handle())
  }
}

/// Whether the last request still merits a same-model compaction.
pub fn should_compact(facts: &IdleFacts, idle_tokens: u64, now: UtcDateTime) -> bool {
  let Some(last_request_at) = facts.last_request_at else {
    return false;
  };
  !facts.archived
    && facts.backend_unchanged
    && !facts.turn_busy
    && facts.armed_at == Some(last_request_at)
    && now.into_inner() - last_request_at.into_inner() < time::Duration::hours(1)
    && !facts.compacted_after_request
    && facts.context_tokens.is_some_and(|tokens| u64::try_from(tokens).is_ok_and(|tokens| tokens >= idle_tokens))
    && facts.resume_id.as_ref().is_some_and(|value| !value.is_empty())
    && facts.transcript_present
}

/// Pick the selected Claude account or the process login directory for an unpooled session.
pub fn account_for_session(
  cfg: &CharlieBotConfig,
  option: &BackendOption,
  account_label: Option<&str>,
  default_dir: &Path,
) -> anyhow::Result<Option<claude_config::ClaudeAccount>> {
  if let Some(account) = claude_accounts::account_by_label(cfg, account_label)? {
    return Ok(Some(account));
  }
  if claude_accounts::is_pooled(option, cfg) {
    return Ok(None);
  }
  Ok(Some(claude_config::ClaudeAccount {
    label: "default".to_string(),
    config_dir: default_dir.to_string_lossy().into_owned(),
  }))
}

/// Build the session's resumed Claude command and its selected-account environment.
pub fn build_idle_process(
  registry: Arc<Registry>,
  cfg: &CharlieBotConfig,
  option: &BackendOption,
  account: &claude_config::ClaudeAccount,
  identity: IdleProcessIdentity,
  base_env: &Env,
) -> anyhow::Result<(Vec<String>, Env)> {
  let mut backend_kwargs = Map::new();
  backend_kwargs
    .insert("claude_account".to_string(), json!({"label": account.label, "config_dir": account.config_dir}));
  backend_kwargs.insert("run_kind".to_string(), json!(identity.run_kind));
  let launch = LaunchArgs {
    extra_flags: vec!["--resume".to_string(), identity.cc_session_id, EXCLUDE_DYNAMIC_SECTIONS_FLAG.to_string()],
    cgroup_session_id: Some(identity.session_id.clone()),
    backend_kwargs,
    ..LaunchArgs::default()
  };
  let built = crate::backend_types::build_backend(Arc::clone(&registry), option, cfg, launch)?;
  let argv = built.backend.build_command("/compact\n")?;
  let child_env = crate::launch_loop::build_child_env(
    base_env,
    &registry,
    crate::launch_loop::ChildEnvPolicy::RegisteredEdits,
    Some(&identity.session_id),
    &Env::new(),
  );
  let env = built.backend.prepare_env(child_env)?;
  Ok((argv, env))
}

/// Arguments and account metadata for one idle compaction.
pub struct IdleRunOptions {
  pub config_dir: String,
  pub cwd: PathBuf,
  pub pre_tokens: Option<i64>,
  pub argv: Vec<String>,
  pub env: Env,
  pub trigger: String,
  pub failure_trigger: Option<String>,
  pub model: String,
  pub family: String,
}

/// Describe the idle event and model identity alongside the built command.
pub fn idle_run_options(
  option: &BackendOption,
  account: &claude_config::ClaudeAccount,
  cwd: &Path,
  pre_tokens: Option<i64>,
  argv: Vec<String>,
  env: Env,
) -> IdleRunOptions {
  IdleRunOptions {
    config_dir: account.config_dir.clone(),
    cwd: cwd.to_path_buf(),
    pre_tokens,
    argv,
    env,
    trigger: "idle".to_string(),
    failure_trigger: Some("idle".to_string()),
    model: option.model.clone(),
    family: claude_compaction::model_family(Some(&option.model)),
  }
}

/// The runner the idle hook uses when the queue holds none.
pub fn compaction_runner() -> CompactionRunFn {
  Arc::new(run_real_compaction)
}

fn run_real_compaction<'a>(run: CompactionRun<'a>) -> BoxFuture<'a, anyhow::Result<CompactionOutcome>> {
  Box::pin(claude_compaction::run_compaction(run))
}

/// Arm the session's idle compaction after a master round (`claude_idle_compaction.arm`).
///
/// The trigger path reads the session's facts again when the timer fires, so each closure captures what it reads
/// rather than the arm-time snapshot.
pub fn arm_for_round(ctx: IdleArmContext<'_>) -> BoxFuture<'_, anyhow::Result<()>> {
  Box::pin(async move {
    let IdleArmContext { features, option, session_id, context_state, cwd, base_env, emit, compaction, gate } = ctx;
    let session_id = session_id.to_string();
    let features = Arc::clone(features);
    let option = option.clone();
    let Some(metadata) = features.store.read_metadata_fresh(&session_id)? else {
      anyhow::bail!("session {session_id} disappeared before idle compaction was armed");
    };
    let (_, last_request_at) = context_state(&metadata)?;
    let idle = IdleCompaction::shared();
    idle.disarm(&session_id);
    let Some(armed_at) = last_request_at else {
      return Ok(());
    };
    let compaction = compaction.unwrap_or_else(compaction_runner);
    let inspect_features = Arc::clone(&features);
    let inspect_option = option.clone();
    let inspect_registry = Arc::clone(&features.registry);
    let inspect_session_id = session_id.clone();
    let inspect_context_state = Arc::clone(&context_state);
    let inspect_env = base_env.clone();
    let inspect_cwd = cwd.clone();
    let inspect_emit = Arc::clone(&emit);
    let inspect_armed_at = armed_at;
    let inspect_compaction = Arc::clone(&compaction);
    let inspect_gate = Arc::clone(&gate);
    let reservation_slot = Arc::new(Mutex::new(None::<Arc<AtomicBool>>));
    let inspect_reservation_slot = Arc::clone(&reservation_slot);
    let inspect = Arc::new(move |_now| {
      let Some(metadata) = inspect_features.store.read_metadata_fresh(&inspect_session_id)? else {
        return Ok(None);
      };
      let current_cfg = inspect_features.config.get().context("loading config.yaml for idle Claude compaction")?;
      let (context_tokens, last_request_at) = inspect_context_state(&metadata)?;
      let compacted_after_request = if let Some(last_request_at) = last_request_at {
        // Python scans its cached history in asyncio.to_thread (claude_idle_compaction.py:176-195).
        let store = Arc::clone(&inspect_features.store);
        let session_id = inspect_session_id.clone();
        tokio::task::block_in_place(move || {
          let events = store.load_chat_events_shared_sync(&session_id)?;
          let newest_compaction = events
            .iter()
            .rev()
            .find(|event| {
              event.get("type").and_then(CompactValue::as_str) == Some(event_types::CONTEXT_COMPACTED)
                && event.get("timestamp").and_then(CompactValue::as_str).is_some()
            })
            .and_then(|event| event.get("timestamp"))
            .and_then(CompactValue::as_str)
            .map(UtcDateTime::parse)
            .transpose()?;
          Ok::<_, anyhow::Error>(newest_compaction.is_some_and(|timestamp| timestamp > last_request_at))
        })?
      } else {
        false
      };
      let account_label = claude_metadata::account_of(&metadata);
      let default_dir = login_dirs::claude_config_dir()?;
      let account = account_for_session(&current_cfg, &inspect_option, account_label.as_deref(), &default_dir)?;
      let resume_id = metadata.cc_session_id.clone();
      let transcript_present = match account.as_ref().zip(resume_id.as_deref()) {
        Some((account, resume_id)) => {
          !claude_accounts::transcript_matches(Path::new(&account.config_dir), resume_id)?.is_empty()
        }
        None => false,
      };
      let facts = IdleFacts {
        archived: metadata.status == SessionStatus::Archived,
        backend_unchanged: metadata.backend == inspect_option.id,
        turn_busy: false,
        armed_at: Some(inspect_armed_at),
        last_request_at,
        compacted_after_request,
        context_tokens,
        resume_id: resume_id.clone(),
        transcript_present,
      };
      let reservation = Arc::new(AtomicBool::new(false));
      *inspect_reservation_slot.lock().expect("idle reservation slot is not poisoned") = Some(Arc::clone(&reservation));
      let run_reservation = Arc::clone(&reservation);
      let run_registry = Arc::clone(&inspect_registry);
      let run_option = inspect_option.clone();
      let run_cfg = Arc::clone(&current_cfg);
      let run_account = account.clone();
      let run_session_id = inspect_session_id.clone();
      let run_cc_session_id = resume_id.clone();
      let run_cwd = inspect_cwd.clone();
      let run_env = inspect_env.clone();
      let run_emit = Arc::clone(&inspect_emit);
      let run_tokens = context_tokens;
      let compaction = Arc::clone(&inspect_compaction);
      let run_gate = Arc::clone(&inspect_gate);
      let run: RunFn = Arc::new(move || {
        let reserved = Arc::clone(&run_reservation);
        let registry = Arc::clone(&run_registry);
        let option = run_option.clone();
        let cfg = Arc::clone(&run_cfg);
        let account = run_account.clone();
        let session_id = run_session_id.clone();
        let cc_session_id = run_cc_session_id.clone();
        let cwd = run_cwd.clone();
        let base_env = run_env.clone();
        let emit = Arc::clone(&run_emit);
        let pre_tokens = run_tokens;
        let compaction = Arc::clone(&compaction);
        let gate = Arc::clone(&run_gate);
        Box::pin(async move {
          if reserved.swap(false, Ordering::AcqRel) {
            gate.release(&session_id)?;
          }
          let account = account.expect("a compaction candidate has an account");
          let cc_session_id = cc_session_id.expect("a compaction candidate has a resume id");
          let (argv, env) = build_idle_process(
            registry,
            &cfg,
            &option,
            &account,
            IdleProcessIdentity { session_id: session_id.clone(), cc_session_id, run_kind: RunKind::ManagerTurn },
            &base_env,
          )?;
          let run = idle_run_options(&option, &account, &cwd, pre_tokens, argv, env);
          let outcome = compaction(CompactionRun {
            argv: run.argv,
            env: run.env,
            cwd: &run.cwd,
            config_dir: Path::new(&run.config_dir),
            session_id: &session_id,
            family: &run.family,
            model: &run.model,
            trigger: &run.trigger,
            failure_trigger: run.failure_trigger.as_deref(),
            pre_tokens: run.pre_tokens,
            emit: &emit,
            cfg: &cfg,
            session_cgroup_id: Some(&session_id),
            account_label: &account.label,
            timeout_duration: claude_compaction::CLAUDE_COMPACTION_TIMEOUT,
          })
          .await?;
          if outcome.ok {
            tracing::info!(
              target: "charliebot_agents",
              session = %session_id,
              pre_tokens,
              post_tokens = ?outcome.post_tokens,
              cache_read_tokens = ?outcome.cache_read_tokens,
              account = account.label,
              "claude_idle_compaction_done"
            );
          }
          Ok(())
        }) as BoxFuture<'static, anyhow::Result<()>>
      });
      Ok(Some(IdleCandidate {
        facts,
        idle_tokens: current_cfg.section::<claude_config::AccountsConfig>("accounts").claude_compaction.idle_tokens,
        run,
      }))
    });
    let busy_gate = Arc::clone(&gate);
    let is_busy: BusyFn = Arc::new(move |session_id| {
      let reservation = reservation_slot
        .lock()
        .expect("idle reservation slot is not poisoned")
        .clone()
        .context("idle compaction candidate has no reservation state")?;
      let reserved = busy_gate.reserve(session_id)?;
      if reserved {
        reservation.store(true, Ordering::Release);
      }
      Ok(!reserved)
    });
    idle.arm(IdleTimerInput { session_id, armed_at, inspect, is_busy });
    Ok(())
  })
}

/// Timer inputs for the idle compaction tests here and in the Claude lifecycle tests.
#[cfg(test)]
pub(crate) mod test_support {
  use super::*;

  const IDLE_TOKENS: u64 = 100_000;

  /// An inspect that finds no candidate, so the timer's fire starts nothing.
  pub fn no_candidate() -> InspectFn {
    Arc::new(|_: UtcDateTime| -> anyhow::Result<Option<IdleCandidate>> { Ok(None) })
  }

  /// A timer whose fire reads no candidate and whose busy check reads idle.
  pub fn timer_input(session_id: &str, armed_at: UtcDateTime, inspect: InspectFn) -> IdleTimerInput {
    IdleTimerInput {
      session_id: session_id.to_string(),
      armed_at,
      inspect,
      is_busy: Arc::new(|_: &str| -> anyhow::Result<bool> { Ok(false) }),
    }
  }

  /// A timer whose fire reads `facts` and starts `run` when every idle condition holds.
  pub fn candidate_input(session_id: &str, facts: IdleFacts, busy: bool, run: RunFn) -> IdleTimerInput {
    let armed_at = facts.armed_at.expect("a candidate's facts name the armed request");
    let inspect = Arc::new(move |_: UtcDateTime| -> anyhow::Result<Option<IdleCandidate>> {
      Ok(Some(IdleCandidate { facts: facts.clone(), idle_tokens: IDLE_TOKENS, run: Arc::clone(&run) }))
    });
    IdleTimerInput {
      session_id: session_id.to_string(),
      armed_at,
      inspect,
      is_busy: Arc::new(move |_: &str| -> anyhow::Result<bool> { Ok(busy) }),
    }
  }

  /// Facts of a session whose last request is `request` and whose every idle condition holds.
  pub fn armed_facts(request: UtcDateTime, resume_id: &str) -> IdleFacts {
    IdleFacts {
      archived: false,
      backend_unchanged: true,
      turn_busy: false,
      armed_at: Some(request),
      last_request_at: Some(request),
      compacted_after_request: false,
      context_tokens: Some(IDLE_TOKENS as i64),
      resume_id: Some(resume_id.to_string()),
      transcript_present: true,
    }
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::time::Duration;

  use charliebot_core::config::CharlieBotConfig;
  use charliebot_registry::Registry;
  use charliebot_registry::backend::Env;
  use charliebot_store::metadata::UtcDateTime;
  use charliebot_store::run_records::RunKind;
  use serde_json::Value;

  use crate::backends::claude_code::claude_config;

  use super::*;

  const IDLE_TOKENS: u64 = 100_000;
  const SESSION_ID: &str = "synthetic-idle-session";
  const CC_SESSION_ID: &str = "synthetic-claude-session";

  fn at(value: &str) -> UtcDateTime {
    UtcDateTime::parse(value).unwrap()
  }

  fn base_facts() -> IdleFacts {
    let request = at("2026-09-06T19:05:00Z");
    IdleFacts {
      archived: false,
      backend_unchanged: true,
      turn_busy: false,
      armed_at: Some(request),
      last_request_at: Some(request),
      compacted_after_request: false,
      context_tokens: Some(IDLE_TOKENS as i64),
      resume_id: Some(CC_SESSION_ID.to_string()),
      transcript_present: true,
    }
  }

  fn input(
    facts: IdleFacts,
    busy: bool,
    runs: Arc<AtomicUsize>,
    started: Option<Arc<tokio::sync::Notify>>,
    release: Option<Arc<tokio::sync::Notify>>,
  ) -> IdleTimerInput {
    let armed_at = facts.armed_at.expect("test facts have an armed request");
    IdleTimerInput {
      session_id: SESSION_ID.to_string(),
      armed_at,
      inspect: Arc::new(move |_| {
        let runs = Arc::clone(&runs);
        let started = started.clone();
        let release = release.clone();
        Ok(Some(IdleCandidate {
          facts: facts.clone(),
          idle_tokens: IDLE_TOKENS,
          run: Arc::new(move || {
            let runs = Arc::clone(&runs);
            let started = started.clone();
            let release = release.clone();
            Box::pin(async move {
              runs.fetch_add(1, Ordering::SeqCst);
              if let Some(started) = started {
                started.notify_one();
              }
              if let Some(release) = release {
                release.notified().await;
              }
              Ok(())
            })
          }),
        }))
      }),
      is_busy: Arc::new(move |_| Ok(busy)),
    }
  }

  fn config(root: &std::path::Path, pooled: bool) -> (Arc<Registry>, CharlieBotConfig) {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let body = if pooled {
      "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n      account_pool: alpha\naccounts:\n  claude:\n    - label: acct-a\n      config_dir: /synthetic/account-a\n  claude_pools:\n    alpha: [acct-a]\n"
    } else {
      "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n"
    };
    std::fs::write(home.join("config.yaml"), body).unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", crate::backends::claude_code::register).unwrap();
    let cfg = registry.load_config(&home).unwrap();
    (Arc::new(registry), cfg)
  }

  // Python: tests/test_claude_idle_compaction.py:148.
  // Python: tests/test_claude_idle_compaction.py:149.
  // Python: tests/test_claude_idle_compaction.py:150.
  // Python: tests/test_claude_idle_compaction.py:151.
  // Python: tests/test_claude_idle_compaction.py:152.
  // Python: tests/test_claude_idle_compaction.py:156.
  // Python: tests/test_claude_idle_compaction.py:163.
  // Python: tests/test_claude_idle_compaction.py:164.
  // Python: tests/test_claude_idle_compaction.py:165.
  // Python: tests/test_claude_idle_compaction.py:166.
  // Python: tests/test_claude_idle_compaction.py:167.
  #[test]
  fn every_idle_compaction_gate_rejects_its_python_case() {
    let now = at("2026-09-06T20:00:00Z");
    type GateCase = (&'static str, fn(&mut IdleFacts));
    let cases: [GateCase; 11] = [
      ("archived", |facts| facts.archived = true),
      ("backend_switched", |facts| facts.backend_unchanged = false),
      ("turn_running_or_queued", |facts| facts.turn_busy = true),
      ("armed_for_another_request", |facts| {
        facts.armed_at = Some(at("2026-09-06T19:04:00Z"));
      }),
      ("no_request", |facts| {
        facts.last_request_at = None;
        facts.armed_at = None;
      }),
      ("cache_expired", |facts| {
        let expired = at("2026-09-06T19:00:00Z");
        facts.last_request_at = Some(expired);
        facts.armed_at = Some(expired);
      }),
      ("compacted_after_request", |facts| facts.compacted_after_request = true),
      ("context_below_floor", |facts| facts.context_tokens = Some(IDLE_TOKENS as i64 - 1)),
      ("no_context_reading", |facts| facts.context_tokens = None),
      ("no_resume_id", |facts| facts.resume_id = None),
      ("transcript_missing", |facts| facts.transcript_present = false),
    ];

    for (name, change) in cases {
      let mut facts = base_facts();
      change(&mut facts);
      assert!(!should_compact(&facts, IDLE_TOKENS, now), "{name}");
    }
  }

  // Python: tests/test_claude_idle_compaction.py:176.
  #[test]
  fn every_idle_compaction_gate_true_starts_a_qualifying_candidate() {
    assert!(should_compact(&base_facts(), IDLE_TOKENS, at("2026-09-06T20:00:00Z"),));
  }

  // Python: tests/test_claude_idle_compaction.py:222.
  #[tokio::test]
  async fn a_second_arm_keeps_one_timer_and_replaces_the_first() {
    let idle = IdleCompaction::new();
    let runs = Arc::new(AtomicUsize::new(0));
    idle.arm_for_test(input(base_facts(), false, Arc::clone(&runs), None, None), Duration::from_millis(10)).await;
    let first_task = idle.timer_abort_handle(SESSION_ID).expect("the first arm leaves a timer");
    idle.arm_for_test(input(base_facts(), false, runs.clone(), None, None), Duration::from_secs(3600)).await;
    tokio::task::yield_now().await;

    assert_eq!(idle.timer_count(), 1);
    assert!(first_task.is_finished(), "the first timer is cancelled");
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
  }

  /// A run that starts, waits for `release`, and then returns `outcome`.
  fn gated_run(release: Arc<tokio::sync::Notify>, outcome: fn() -> anyhow::Result<()>) -> RunFn {
    Arc::new(move || {
      let release = Arc::clone(&release);
      Box::pin(async move {
        release.notified().await;
        outcome()
      }) as BoxFuture<'static, anyhow::Result<()>>
    })
  }

  fn fail_after_release() -> anyhow::Result<()> {
    Err(anyhow::anyhow!("synthetic compaction failure"))
  }

  fn panic_after_release() -> anyhow::Result<()> {
    panic!("synthetic compaction panic")
  }

  // Python: src/backends/claude_code/claude_idle_compaction.py:_compact, whose finally pops the running entry.
  #[tokio::test]
  async fn a_failed_compaction_fails_the_turn_that_waits_and_the_next_turn_starts() {
    let idle = IdleCompaction::new();
    let release = Arc::new(tokio::sync::Notify::new());
    let run = gated_run(Arc::clone(&release), fail_after_release);
    idle
      .arm_for_test(test_support::candidate_input(SESSION_ID, base_facts(), false, run), Duration::from_secs(3600))
      .await;
    let compaction = idle.fire_at(SESSION_ID, at("2026-09-06T19:55:00Z")).unwrap().expect("all idle conditions hold");
    let waiting_idle = idle.clone();
    let turn = tokio::spawn(async move { waiting_idle.settle_for_turn(SESSION_ID).await });
    tokio::task::yield_now().await;
    assert!(!turn.is_finished(), "the turn waits for the running compaction");

    release.notify_one();
    let settled = turn.await.unwrap();
    compaction.await.unwrap();

    assert!(
      settled.as_ref().is_err_and(|error| format!("{error:#}").contains("synthetic compaction failure")),
      "the waiting turn ends with the compaction's error: {settled:?}"
    );
    assert!(!idle.is_running(SESSION_ID));
    idle.settle_for_turn(SESSION_ID).await.unwrap();
  }

  // Python: src/backends/claude_code/claude_idle_compaction.py:_compact, whose finally pops the running entry.
  #[tokio::test]
  async fn a_panicking_compaction_fails_the_turn_that_waits_and_leaves_no_running_entry() {
    let idle = IdleCompaction::new();
    let release = Arc::new(tokio::sync::Notify::new());
    let run = gated_run(Arc::clone(&release), panic_after_release);
    idle
      .arm_for_test(test_support::candidate_input(SESSION_ID, base_facts(), false, run), Duration::from_secs(3600))
      .await;
    let compaction = idle.fire_at(SESSION_ID, at("2026-09-06T19:55:00Z")).unwrap().expect("all idle conditions hold");
    let waiting_idle = idle.clone();
    let turn = tokio::spawn(async move { waiting_idle.settle_for_turn(SESSION_ID).await });
    tokio::task::yield_now().await;
    assert!(!turn.is_finished(), "the turn waits for the running compaction");

    release.notify_one();
    let settled = turn.await.unwrap();
    let finished = compaction.await;

    assert!(
      settled.as_ref().is_err_and(|error| format!("{error:#}").contains("panicked")),
      "the waiting turn ends with the panic: {settled:?}"
    );
    assert!(finished.is_ok(), "the compaction task ends normally: {finished:?}");
    assert!(!idle.is_running(SESSION_ID));
    idle.settle_for_turn(SESSION_ID).await.unwrap();
  }

  // Python: tests/test_claude_idle_compaction.py:280.
  #[tokio::test]
  async fn a_queued_turn_makes_the_idle_trigger_skip() {
    let idle = IdleCompaction::new();
    let runs = Arc::new(AtomicUsize::new(0));
    idle.arm_for_test(input(base_facts(), true, Arc::clone(&runs), None, None), Duration::from_secs(3600)).await;

    assert!(idle.fire_at(SESSION_ID, at("2026-09-06T19:55:00Z")).unwrap().is_none());
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(idle.running_count(), 0);
  }

  // Python: tests/test_claude_idle_compaction.py:309.
  #[test]
  fn idle_run_options_keep_the_session_model_and_idle_event_triggers() {
    let root = tempfile::tempdir().unwrap();
    let (_registry, cfg) = config(root.path(), true);
    let option = cfg.backends.options[0].clone();
    let account =
      claude_config::ClaudeAccount { label: "acct-a".to_string(), config_dir: "/synthetic/account-a".to_string() };
    let run = idle_run_options(
      &option,
      &account,
      std::path::Path::new("/synthetic/session"),
      Some(120_000),
      vec!["claude".to_string()],
      Env::new(),
    );

    assert_eq!(run.config_dir, "/synthetic/account-a");
    assert_eq!(run.cwd, std::path::Path::new("/synthetic/session"));
    assert_eq!(run.pre_tokens, Some(120_000));
    assert_eq!((run.trigger.as_str(), run.failure_trigger.as_deref()), ("idle", Some("idle")));
    assert_eq!((run.model.as_str(), run.family.as_str()), ("claude-fable-5-1", "fable"));
  }

  // Python: tests/test_claude_idle_compaction.py:341.
  #[test]
  fn an_unpooled_session_uses_the_default_login_directory() {
    let root = tempfile::tempdir().unwrap();
    let (registry, cfg) = config(root.path(), false);
    let option = cfg.backends.options[0].clone();
    let default_dir = root.path().join("default-claude");
    let account = account_for_session(&cfg, &option, None, &default_dir).unwrap().unwrap();
    let base_env = Env::from([
      ("HOME".to_string(), root.path().to_string_lossy().into_owned()),
      ("PATH".to_string(), "/synthetic/bin:/usr/bin:/bin".to_string()),
    ]);
    let (_, env) = build_idle_process(
      registry,
      &cfg,
      &option,
      &account,
      IdleProcessIdentity {
        session_id: SESSION_ID.to_string(),
        cc_session_id: CC_SESSION_ID.to_string(),
        run_kind: RunKind::ManagerTurn,
      },
      &base_env,
    )
    .unwrap();

    assert_eq!(account.label, "default");
    assert_eq!(env.get("CLAUDE_CONFIG_DIR").unwrap(), &default_dir.to_string_lossy());
  }

  // Python: tests/test_claude_idle_compaction.py:211 (the hook assertions of the argv-equality test).
  #[test]
  fn the_idle_argv_installs_the_compaction_context_hook() {
    let root = tempfile::tempdir().unwrap();
    let (registry, cfg) = config(root.path(), true);
    let option = cfg.backends.options[0].clone();
    let account =
      claude_config::ClaudeAccount { label: "acct-a".to_string(), config_dir: "/synthetic/account-a".to_string() };
    let base_env = Env::from([
      ("HOME".to_string(), root.path().to_string_lossy().into_owned()),
      ("PATH".to_string(), "/synthetic/bin:/usr/bin:/bin".to_string()),
    ]);
    let (argv, _) = build_idle_process(
      registry,
      &cfg,
      &option,
      &account,
      IdleProcessIdentity {
        session_id: SESSION_ID.to_string(),
        cc_session_id: CC_SESSION_ID.to_string(),
        run_kind: RunKind::ManagerTurn,
      },
      &base_env,
    )
    .unwrap();
    let settings = argv[argv.iter().position(|value| value == "--settings").unwrap() + 1].clone();
    assert_eq!(
      settings,
      concat!(
        r#"{"disableClaudeAiConnectors":true,"syncClaudeAiSkills":false,"hooks":{"SessionStart":["#,
        r#"{"matcher":"compact","hooks":["#,
        r#"{"type":"command","command":"charliebot compaction-context --session synthetic-idle-session --slot 1"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session synthetic-idle-session --slot 2"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session synthetic-idle-session --slot 3"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session synthetic-idle-session --slot 4"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session synthetic-idle-session --slot 5"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session synthetic-idle-session --slot 6"}]}]}}"#,
      )
    );
    let settings: Value = serde_json::from_str(&settings).unwrap();
    let [session_start] = settings["hooks"]["SessionStart"].as_array().unwrap().as_slice() else {
      panic!("one SessionStart entry");
    };
    assert_eq!(session_start["matcher"], "compact");
    let commands: Vec<&str> =
      session_start["hooks"].as_array().unwrap().iter().map(|hook| hook["command"].as_str().unwrap()).collect();
    let expected: Vec<String> =
      (1..=6).map(|slot| format!("charliebot compaction-context --session {SESSION_ID} --slot {slot}")).collect();
    assert_eq!(commands, expected);
  }
}
