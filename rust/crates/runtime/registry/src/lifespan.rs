//! The lifespan steps of the server and the runner that orders them.
//!
//! Startup runs after the home writer fence and the home seeding. In registration order it runs the pre-recovery
//! steps, then the recovery steps, then the early services, then at most one trigger step, then the ready services.
//! Each recovery step starts as a task before the early services and is awaited after them, so no agent process
//! starts before recovery completes. Each step and service logs one `startup_segment` line through the server's
//! [`StartupClock`].
//!
//! Shutdown stops the ready services in reverse registration order, then the early ones in reverse, then closes the
//! shared HTTP client and WebSockets. The server releases the home fence and logs `charliebot_shutdown` next. On
//! SIGINT and failed bind it then runs let-go steps; on SIGTERM it skips them, restores SIGTERM's default
//! disposition and re-raises it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};

use crate::context::FeatureContext;
pub use crate::registry::StepFn;
use crate::startup_stop::StartupStop;
use crate::startup_timing::StartupClock;
use crate::wiring::{Phase, Service, ServiceContext};

/// The lifespan of one running server: its started services and registered let-go steps.
pub struct Lifespan {
  features: Arc<FeatureContext>,
  stop: StartupStop,
  /// The services that started, in start order.
  started: Vec<(Phase, Arc<dyn Service>)>,
}

/// The outcome of startup: the server is ready, or a stop signal ended startup at a safe point.
pub enum StartupOutcome {
  Ready(Lifespan),
  Stopped(Lifespan),
}

/// One service and its elapsed shutdown time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepTiming {
  /// The service name.
  pub name: &'static str,
  /// The time spent stopping the service.
  pub elapsed: Duration,
}

/// How long each shutdown step took. The `charliebot_shutdown` line reports each value in milliseconds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloseTimings {
  /// The service timings, in shutdown order.
  pub step_closes: Vec<StepTiming>,
  /// Closing the shared HTTP client (`http_client_ms`).
  pub http_client: Duration,
  /// Closing every open session WebSocket (`ws_close_ms`).
  pub ws_close: Duration,
}

/// One spawned recovery step's outcome: the step's error, prefixed with its name.
type RecoveryOutcome = Shared<BoxFuture<'static, Result<(), String>>>;

/// The outcome of [`Lifespan::shutdown`]. Every stop runs even when one fails, so the closes are timed either way.
pub struct Shutdown {
  pub closes: CloseTimings,
  /// The first stop failure, if any.
  pub error: Option<anyhow::Error>,
}

impl Lifespan {
  /// The startup steps run in the order of the module docs, ending one `clock` segment per step and service. A failure
  /// stops the services that started, runs the let-go steps, and returns the startup error.
  pub async fn start(features: Arc<FeatureContext>, clock: &mut StartupClock) -> anyhow::Result<StartupOutcome> {
    let mut lifespan = Lifespan { stop: features.startup.stop(), features, started: Vec::new() };
    match lifespan.startup(clock).await {
      Ok(true) => Ok(StartupOutcome::Ready(lifespan)),
      Ok(false) => Ok(StartupOutcome::Stopped(lifespan)),
      Err(error) if lifespan.stop.is_cancelled() => {
        tracing::error!(error = format!("{error:#}"), "lifespan_startup_interrupted_failed");
        Ok(StartupOutcome::Stopped(lifespan))
      }
      Err(error) => Err(lifespan.abort(error).await),
    }
  }

  async fn startup(&mut self, clock: &mut StartupClock) -> anyhow::Result<bool> {
    let registry = Arc::clone(&self.features.registry);
    for (name, step) in registry.pre_recovery_steps() {
      if self.stop.is_cancelled() {
        return Ok(false);
      }
      step(Arc::clone(&self.features)).await.with_context(|| format!("pre-recovery step {name:?} failed"))?;
      clock.segment(&format!("pre_recovery:{name}"));
      if self.stop.is_cancelled() {
        return Ok(false);
      }
    }
    if self.stop.is_cancelled() {
      return Ok(false);
    }

    let recovery = self.spawn_recovery_steps();
    // Resolves when every step has succeeded or the first one has failed, as the startup path's own waits do.
    let outcomes: Vec<RecoveryOutcome> = recovery.iter().map(|(_, outcome)| outcome.clone()).collect();
    let recovery_done: Shared<BoxFuture<'static, ()>> = async move {
      for outcome in outcomes {
        if outcome.await.is_err() {
          return;
        }
      }
    }
    .boxed()
    .shared();
    clock.segment("recovery_spawn");

    if !self.start_services(Phase::Early, &recovery_done, clock).await? {
      await_recovery(recovery, clock).await?;
      return Ok(false);
    }
    await_recovery(recovery, clock).await?;
    if self.stop.is_cancelled() {
      return Ok(false);
    }

    if let Some((name, step)) = registry.trigger_entry() {
      if self.stop.is_cancelled() {
        return Ok(false);
      }
      step(Arc::clone(&self.features)).await.with_context(|| format!("trigger step {name:?} failed"))?;
      clock.segment(&format!("trigger:{name}"));
      if self.stop.is_cancelled() {
        return Ok(false);
      }
    }
    self.start_services(Phase::Ready, &recovery_done, clock).await
  }

  /// Every registered recovery step spawned as its own task, so the early services start while the steps run. Returns
  /// each step's name and outcome in registration order.
  fn spawn_recovery_steps(&self) -> Vec<(&'static str, RecoveryOutcome)> {
    self
      .features
      .registry
      .recovery_steps()
      .into_iter()
      .map(|(name, step)| {
        let run = step(Arc::clone(&self.features));
        let outcome = async move { run.await.map_err(|error| format!("{name:?}: {error:#}")) }.boxed().shared();
        tokio::spawn(outcome.clone());
        (name, outcome)
      })
      .collect()
  }

  async fn start_services(
    &mut self,
    phase: Phase,
    recovery_done: &Shared<BoxFuture<'static, ()>>,
    clock: &mut StartupClock,
  ) -> anyhow::Result<bool> {
    for service in self.features.registry.services_in(phase) {
      if self.stop.is_cancelled() {
        return Ok(false);
      }
      let name = service.name();
      let ctx = ServiceContext { features: Arc::clone(&self.features), recovery_done: recovery_done.clone() };
      service.start(ctx).await.with_context(|| format!("service {name:?} failed to start"))?;
      clock.segment(&format!("service:{name}"));
      self.started.push((phase, service));
      if self.stop.is_cancelled() {
        return Ok(false);
      }
    }
    Ok(true)
  }

  /// The startup error returned after stopping what started, with any shutdown error added to it.
  async fn abort(mut self, error: anyhow::Error) -> anyhow::Error {
    let (step_closes, mut first_error) = self.stop_services().await;
    if let Err(error) = self.run_let_go_steps().await {
      first_error.get_or_insert(error);
    }
    let _ = self.close_clients(step_closes).await;
    match first_error {
      None => error,
      Some(stop_error) => error.context(format!("stopping after the failed startup also failed: {stop_error:#}")),
    }
  }

  /// The first service failure, after stopping started services in reverse phase order and closing the shared clients.
  pub async fn shutdown(&mut self) -> Shutdown {
    let (step_closes, error) = self.stop_services().await;
    let closes = self.close_clients(step_closes).await;
    Shutdown { closes, error }
  }

  async fn stop_services(&mut self) -> (Vec<StepTiming>, Option<anyhow::Error>) {
    let mut first_error: Option<anyhow::Error> = None;
    let mut step_closes = Vec::new();
    let started_services = std::mem::take(&mut self.started);
    for phase in [Phase::Ready, Phase::Early] {
      for (_, service) in started_services.iter().rev().filter(|(started, _)| *started == phase) {
        let name = service.name();
        let started = Instant::now();
        if let Err(error) = service.stop().await.with_context(|| format!("service {name:?} failed to stop")) {
          first_error.get_or_insert(error);
        }
        step_closes.push(StepTiming { name, elapsed: started.elapsed() });
      }
    }
    (step_closes, first_error)
  }

  /// The first failure of the let-go steps, run in registration order, after all steps run.
  pub async fn run_let_go_steps(&self) -> anyhow::Result<()> {
    let mut first_error: Option<anyhow::Error> = None;
    for (name, step) in self.features.registry.let_go_steps() {
      if let Err(error) = step(Arc::clone(&self.features)).await.with_context(|| format!("let-go step {name:?} failed"))
      {
        first_error.get_or_insert(error);
      }
    }
    first_error.map_or(Ok(()), Err)
  }

  async fn close_clients(&self, step_closes: Vec<StepTiming>) -> CloseTimings {
    let http_started = Instant::now();
    charliebot_compat::http_client::close_shared_client();
    let http_client = http_started.elapsed();
    let ws_started = Instant::now();
    self.features.store.close_subscribers().await;
    CloseTimings { step_closes, http_client, ws_close: ws_started.elapsed() }
  }
}

/// The wait for each spawned recovery step in registration order, ending one `recovery:<step>` segment per step. The
/// first failing step's error ends the wait, with the step's name in it.
async fn await_recovery(
  recovery: Vec<(&'static str, RecoveryOutcome)>,
  clock: &mut StartupClock,
) -> anyhow::Result<()> {
  for (name, outcome) in recovery {
    outcome.await.map_err(|message| anyhow::anyhow!("crash recovery failed: {message}"))?;
    clock.segment(&format!("recovery:{name}"));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::sync::Mutex;
  use std::time::Duration;

  use super::*;
  use crate::context::features_over;
  use crate::registry::{Registry, RegistryError};

  /// What the steps and services of these tests logged, in order.
  static TRACE: Mutex<Vec<String>> = Mutex::new(Vec::new());
  static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

  fn log(entry: &str) {
    TRACE.lock().expect("the trace lock is never poisoned").push(entry.to_string());
  }

  /// A typed startup value: a pre-recovery step inserts it, and the ready services read it.
  struct Marker;

  fn pre_alpha(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async move {
      features.startup.insert(Marker)?;
      log("pre_alpha");
      Ok(())
    })
  }

  fn pre_beta(_features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async {
      log("pre_beta");
      Ok(())
    })
  }

  /// A recovery step slow enough that the early services start before it ends.
  fn recover(_features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async {
      log("recovery_start");
      tokio::time::sleep(Duration::from_millis(20)).await;
      log("recovery_end");
      Ok(())
    })
  }

  fn trigger(_features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async {
      log("trigger");
      Ok(())
    })
  }

  fn let_go(_features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async {
      log("let_go");
      Ok(())
    })
  }

  /// A service that logs its start and stop. A ready service fails its start when no step inserted the marker.
  struct Probe {
    name: &'static str,
    phase: Phase,
  }

  impl Service for Probe {
    fn name(&self) -> &'static str {
      self.name
    }

    fn phase(&self) -> Phase {
      self.phase
    }

    fn start<'a>(&'a self, ctx: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
      Box::pin(async move {
        if self.phase == Phase::Ready {
          ctx.features.startup.require::<Marker>()?;
        }
        log(&format!("{}:start", self.name));
        Ok(())
      })
    }

    fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
      Box::pin(async move {
        log(&format!("{}:stop", self.name));
        Ok(())
      })
    }
  }

  fn alpha(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.pre_recovery_step("pre_alpha", pre_alpha)?;
    registry.recovery_step("recover", recover)?;
    registry.service(Arc::new(Probe { name: "early_alpha", phase: Phase::Early }))?;
    registry.service(Arc::new(Probe { name: "ready_alpha", phase: Phase::Ready }))
  }

  fn beta(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.pre_recovery_step("pre_beta", pre_beta)?;
    registry.service(Arc::new(Probe { name: "early_beta", phase: Phase::Early }))?;
    registry.trigger_step("trigger", trigger)?;
    registry.service(Arc::new(Probe { name: "ready_beta", phase: Phase::Ready }))?;
    registry.let_go_step("let_go", let_go)
  }

  fn let_go_only(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.let_go_step("let_go", let_go)
  }

  /// A second package's recovery step, slow enough that the first recovery's start and end interleave with it.
  fn recover_second(_features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async {
      log("second_recovery_start");
      tokio::time::sleep(Duration::from_millis(20)).await;
      log("second_recovery_end");
      Ok(())
    })
  }

  fn second_recovery(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.recovery_step("second_recover", recover_second)
  }

  /// Every registered recovery step runs, and each completes before the trigger step.
  #[tokio::test]
  async fn lifespan_runs_every_registered_recovery_step() {
    let _guard = TEST_LOCK.lock().await;
    TRACE.lock().unwrap().clear();
    let home = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.register_package("features.alpha", alpha).unwrap();
    registry.register_package("features.second", second_recovery).unwrap();
    registry.register_package("features.beta", beta).unwrap();
    let features = features_over(home.path(), Arc::new(registry));

    let mut lifespan = match Lifespan::start(features, &mut StartupClock::new(Instant::now())).await.unwrap() {
      StartupOutcome::Ready(lifespan) => lifespan,
      StartupOutcome::Stopped(_) => panic!("startup stopped without a stop request"),
    };
    charliebot_compat::http_client::shared_client().unwrap();
    let started = TRACE.lock().unwrap().clone();
    let position = |entry: &str| {
      started.iter().position(|logged| logged == entry).unwrap_or_else(|| panic!("{entry} did not run: {started:?}"))
    };
    assert!(position("recovery_end") < position("trigger"));
    assert!(position("second_recovery_end") < position("trigger"));

    let shutdown = lifespan.shutdown().await;
    assert!(shutdown.error.is_none(), "{:?}", shutdown.error);
    assert!(!charliebot_compat::http_client::close_shared_client(), "the shutdown left the shared HTTP client open");
  }

  #[tokio::test]
  async fn lifespan_runs_its_steps_and_services_in_the_stated_order() {
    let _guard = TEST_LOCK.lock().await;
    TRACE.lock().unwrap().clear();
    let home = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.register_package("features.alpha", alpha).unwrap();
    registry.register_package("features.beta", beta).unwrap();
    let features = features_over(home.path(), Arc::new(registry));

    let mut lifespan = match Lifespan::start(features, &mut StartupClock::new(Instant::now())).await.unwrap() {
      StartupOutcome::Ready(lifespan) => lifespan,
      StartupOutcome::Stopped(_) => panic!("startup stopped without a stop request"),
    };
    charliebot_compat::http_client::shared_client().unwrap();
    let started = TRACE.lock().unwrap().clone();
    let position = |entry: &str| {
      started.iter().position(|logged| logged == entry).unwrap_or_else(|| panic!("{entry} did not run: {started:?}"))
    };
    assert!(position("pre_alpha") < position("pre_beta"));
    assert!(position("pre_beta") < position("recovery_start"));
    assert!(position("early_alpha:start") < position("early_beta:start"));
    assert!(position("early_beta:start") < position("trigger"));
    assert!(position("recovery_end") < position("trigger"));
    assert!(position("trigger") < position("ready_alpha:start"));
    assert!(position("ready_alpha:start") < position("ready_beta:start"));

    let shutdown = lifespan.shutdown().await;
    assert!(shutdown.error.is_none(), "{:?}", shutdown.error);
    assert!(!charliebot_compat::http_client::close_shared_client(), "the shutdown left the shared HTTP client open");
    let all = TRACE.lock().unwrap().clone();
    let stopped: Vec<&str> = all[started.len()..].iter().map(String::as_str).collect();
    // Shutdown stops services and clients before logging, without a let-go phase.
    assert_eq!(stopped, ["ready_beta:stop", "ready_alpha:stop", "early_beta:stop", "early_alpha:stop"]);
    lifespan.run_let_go_steps().await.unwrap();
    assert_eq!(TRACE.lock().unwrap().last().map(String::as_str), Some("let_go"));
  }

  #[tokio::test]
  async fn shutdown_leaves_let_go_steps_for_the_explicit_call() {
    let _guard = TEST_LOCK.lock().await;
    TRACE.lock().unwrap().clear();
    let home = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.register_package("features.gamma", let_go_only).unwrap();
    let features = features_over(home.path(), Arc::new(registry));
    let mut lifespan = match Lifespan::start(features, &mut StartupClock::new(Instant::now())).await.unwrap() {
      StartupOutcome::Ready(lifespan) => lifespan,
      StartupOutcome::Stopped(_) => panic!("startup stopped without a stop request"),
    };
    TRACE.lock().unwrap().clear();

    let shutdown = lifespan.shutdown().await;
    assert!(shutdown.error.is_none(), "{:?}", shutdown.error);
    let stopped = TRACE.lock().unwrap().clone();
    assert!(!stopped.iter().any(|entry| entry == "let_go"), "shutdown ran a let-go step: {stopped:?}");

    lifespan.run_let_go_steps().await.unwrap();
    assert_eq!(*TRACE.lock().unwrap(), ["let_go"]);
  }
}
