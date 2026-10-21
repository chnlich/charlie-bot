//! The lifespan steps of the server and the runner that orders them.
//!
//! Startup runs in this order, after the home writer fence and the home seeding:
//!
//! 1. the pre-recovery steps, in registration order;
//! 2. the recovery step, at most one: started as a task before the early services start, and awaited after them, so
//!    no route, trigger or cron fire starts an agent process before it completes;
//! 3. the early services, in registration order;
//! 4. the trigger step, at most one, after recovery;
//! 5. the ready services, in registration order.
//!
//! Shutdown stops the ready services in reverse registration order, then the early ones in reverse, then runs the
//! let-go steps in registration order and waits for them before the process exits.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};

use crate::context::FeatureContext;
use crate::registry::{Registry, RegistryError};
use crate::wiring::{Phase, Service, ServiceContext};

/// One lifespan step: it runs with the feature context and resolves when the step is done. A step that fails stops
/// the startup, or is part of the shutdown's error report.
pub type StepFn = fn(Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>>;

impl Registry {
  /// Register a pre-recovery step (for example the session cgroup sweep). Steps run in registration order.
  pub fn pre_recovery_step(&mut self, name: &'static str, step: StepFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.pre_recovery_steps.insert(package, name, step)
  }

  /// Register the recovery step. At most one package registers it.
  pub fn recovery_step(&mut self, name: &'static str, step: StepFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.recovery_step.insert_single(package, name, step)
  }

  /// Register the trigger step, which runs after recovery and before the ready services. At most one registers it.
  pub fn trigger_step(&mut self, name: &'static str, step: StepFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.trigger_step.insert_single(package, name, step)
  }

  /// Register a let-go step, which runs at shutdown after the services stop. Steps run in registration order.
  pub fn let_go_step(&mut self, name: &'static str, step: StepFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.let_go_steps.insert(package, name, step)
  }
}

/// The lifespan of one running server: the services it started and the let-go steps it runs at shutdown.
pub struct Lifespan {
  features: Arc<FeatureContext>,
  /// The services that started, in start order.
  started: Vec<(Phase, Arc<dyn Service>)>,
}

/// One service or let-go step and its elapsed shutdown time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepTiming {
  /// The Python service name or the Rust-only step name.
  pub name: &'static str,
  /// The time spent stopping the service or running the let-go step.
  pub elapsed: Duration,
}

/// How long each shutdown step took. The `charliebot_shutdown` line reports each value in milliseconds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloseTimings {
  /// The service and let-go timings, in shutdown order.
  pub step_closes: Vec<StepTiming>,
  /// Closing the shared HTTP client (`http_client_ms`).
  pub http_client: Duration,
  /// Closing every open session WebSocket (`ws_close_ms`).
  pub ws_close: Duration,
}

/// The outcome of [`Lifespan::shutdown`]. Every stop runs even when one fails, so the closes are timed either way.
pub struct Shutdown {
  pub closes: CloseTimings,
  /// The first stop failure, if any.
  pub error: Option<anyhow::Error>,
}

/// The outcome of running the lifespan's let-go steps.
pub struct LetGo {
  pub step_closes: Vec<StepTiming>,
  /// The first let-go failure.
  pub error: Option<anyhow::Error>,
}

impl Lifespan {
  /// Runs the startup steps in the order of the module docs. A failure stops the services that started, runs the
  /// let-go steps, and returns the startup error.
  pub async fn start(features: Arc<FeatureContext>) -> anyhow::Result<Lifespan> {
    let mut lifespan = Lifespan { features, started: Vec::new() };
    match lifespan.startup().await {
      Ok(()) => Ok(lifespan),
      Err(error) => Err(lifespan.abort(error).await),
    }
  }

  async fn startup(&mut self) -> anyhow::Result<()> {
    let registry = Arc::clone(&self.features.registry);
    for (name, step) in registry.pre_recovery_steps() {
      step(Arc::clone(&self.features)).await.with_context(|| format!("pre-recovery step {name:?} failed"))?;
    }

    let recovery = registry.recovery_entry().map(|(name, step)| {
      let run = step(Arc::clone(&self.features));
      let outcome = async move { run.await.map_err(|error| format!("{name:?}: {error:#}")) }.boxed().shared();
      // Python's `create_task`: the recovery runs while the early services start.
      tokio::spawn(outcome.clone());
      outcome
    });
    let recovery_done: Shared<BoxFuture<'static, ()>> = match &recovery {
      Some(outcome) => outcome.clone().map(|_| ()).boxed().shared(),
      None => futures_util::future::ready(()).boxed().shared(),
    };

    self.start_services(Phase::Early, &recovery_done).await?;
    if let Some(outcome) = recovery {
      outcome.await.map_err(|message| anyhow::anyhow!("crash recovery failed: {message}"))?;
    }

    if let Some((name, step)) = registry.trigger_entry() {
      step(Arc::clone(&self.features)).await.with_context(|| format!("trigger step {name:?} failed"))?;
    }
    self.start_services(Phase::Ready, &recovery_done).await
  }

  async fn start_services(
    &mut self,
    phase: Phase,
    recovery_done: &Shared<BoxFuture<'static, ()>>,
  ) -> anyhow::Result<()> {
    for service in self.features.registry.services_in(phase) {
      let name = service.name();
      let ctx = ServiceContext { features: Arc::clone(&self.features), recovery_done: recovery_done.clone() };
      service.start(ctx).await.with_context(|| format!("service {name:?} failed to start"))?;
      self.started.push((phase, service));
    }
    Ok(())
  }

  /// Stops what started after a failed startup and returns the startup error, with any shutdown error added to it.
  async fn abort(mut self, error: anyhow::Error) -> anyhow::Error {
    match self.shutdown().await.error {
      None => error,
      Some(stop_error) => error.context(format!("stopping after the failed startup also failed: {stop_error:#}")),
    }
  }

  /// Stops the started services (ready ones in reverse order, then early ones in reverse), runs the let-go steps in
  /// registration order, then closes the shared HTTP client and the open WebSockets. Every step runs even when an
  /// earlier one fails; the first failure is returned with the close timings.
  pub async fn shutdown(&mut self) -> Shutdown {
    let mut first_error: Option<anyhow::Error> = None;
    // Python registers `usage_tally`; Rust has no matching service stop, so its field is zero.
    let mut step_closes = vec![StepTiming { name: "usage_tally", elapsed: Duration::ZERO }];
    for phase in [Phase::Ready, Phase::Early] {
      for (_, service) in self.started.iter().rev().filter(|(started, _)| *started == phase) {
        let name = service.name();
        let started = Instant::now();
        if let Err(error) = service.stop().await.with_context(|| format!("service {name:?} failed to stop")) {
          first_error.get_or_insert(error);
        }
        step_closes.push(StepTiming { name, elapsed: started.elapsed() });
      }
    }
    let let_go = self.run_let_go_steps().await;
    first_error = first_error.or(let_go.error);
    step_closes.extend(let_go.step_closes);
    let closes = self.close_clients(step_closes).await;
    Shutdown { closes, error: first_error }
  }

  /// Runs every let-go step in registration order and returns the first failure with each step's timing.
  pub async fn run_let_go_steps(&self) -> LetGo {
    let mut first_error: Option<anyhow::Error> = None;
    let mut step_closes = Vec::new();
    for (name, step) in self.features.registry.let_go_steps() {
      let started = Instant::now();
      if let Err(error) = step(Arc::clone(&self.features)).await.with_context(|| format!("let-go step {name:?} failed"))
      {
        first_error.get_or_insert(error);
      }
      step_closes.push(StepTiming { name, elapsed: started.elapsed() });
    }
    LetGo { step_closes, error: first_error }
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

impl Registry {
  /// The pre-recovery steps in registration order.
  pub(crate) fn pre_recovery_steps(&self) -> Vec<(&'static str, StepFn)> {
    self.pre_recovery_steps.pairs().map(|(name, step)| (name, *step)).collect()
  }

  /// The recovery step, if one is registered.
  pub(crate) fn recovery_entry(&self) -> Option<(&'static str, StepFn)> {
    self.recovery_step.pairs().next().map(|(name, step)| (name, *step))
  }

  /// The trigger step, if one is registered.
  pub(crate) fn trigger_entry(&self) -> Option<(&'static str, StepFn)> {
    self.trigger_step.pairs().next().map(|(name, step)| (name, *step))
  }

  /// The let-go steps in registration order.
  pub(crate) fn let_go_steps(&self) -> Vec<(&'static str, StepFn)> {
    self.let_go_steps.pairs().map(|(name, step)| (name, *step)).collect()
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Mutex;
  use std::time::Duration;

  use super::*;
  use crate::context::features_over;

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

  /// Takes long enough that the early services start before it ends.
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

  #[tokio::test]
  async fn lifespan_runs_its_steps_and_services_in_the_stated_order() {
    let _guard = TEST_LOCK.lock().await;
    TRACE.lock().unwrap().clear();
    let home = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.features.alpha", alpha).unwrap();
    registry.register_package("src.features.beta", beta).unwrap();
    let features = features_over(home.path(), Arc::new(registry));

    let mut lifespan = Lifespan::start(features).await.unwrap();
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
    assert_eq!(stopped, ["ready_beta:stop", "ready_alpha:stop", "early_beta:stop", "early_alpha:stop", "let_go"]);
  }

  #[tokio::test]
  async fn shutdown_leaves_let_go_steps_for_the_explicit_call() {
    let _guard = TEST_LOCK.lock().await;
    TRACE.lock().unwrap().clear();
    let home = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.features.gamma", let_go_only).unwrap();
    let features = features_over(home.path(), Arc::new(registry));
    let mut lifespan = Lifespan::start(features).await.unwrap();
    TRACE.lock().unwrap().clear();

    let shutdown = lifespan.shutdown().await;
    assert!(shutdown.error.is_none(), "{:?}", shutdown.error);
    let stopped = TRACE.lock().unwrap().clone();
    assert!(!stopped.iter().any(|entry| entry == "let_go"), "shutdown ran a let-go step: {stopped:?}");

    let let_go = lifespan.run_let_go_steps().await;
    assert!(let_go.error.is_none(), "{:?}", let_go.error);
    assert_eq!(*TRACE.lock().unwrap(), ["let_go"]);
  }
}
