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
  async fn abort(self, error: anyhow::Error) -> anyhow::Error {
    match self.shutdown().await {
      Ok(()) => error,
      Err(stop_error) => error.context(format!("stopping after the failed startup also failed: {stop_error:#}")),
    }
  }

  /// Stops the started services (ready ones in reverse order, then early ones in reverse), then runs the let-go steps
  /// in registration order. Every step runs even when an earlier one fails; the first failure is returned.
  pub async fn shutdown(self) -> anyhow::Result<()> {
    let mut first_error: Option<anyhow::Error> = None;
    for phase in [Phase::Ready, Phase::Early] {
      for (_, service) in self.started.iter().rev().filter(|(started, _)| *started == phase) {
        let name = service.name();
        if let Err(error) = service.stop().await.with_context(|| format!("service {name:?} failed to stop")) {
          first_error.get_or_insert(error);
        }
      }
    }
    for (name, step) in self.features.registry.let_go_steps() {
      if let Err(error) = step(Arc::clone(&self.features)).await.with_context(|| format!("let-go step {name:?} failed"))
      {
        first_error.get_or_insert(error);
      }
    }
    first_error.map_or(Ok(()), Err)
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
  use std::path::Path;
  use std::sync::Mutex;
  use std::time::Duration;

  use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME};
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_store::Store;

  use super::*;
  use crate::context::StartupState;
  use crate::store_hooks::store_hooks;

  /// What the steps and services of this test logged, in order. Only one test in this binary uses it.
  static TRACE: Mutex<Vec<String>> = Mutex::new(Vec::new());

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

  /// The feature context over the tempdir `home`: the config and credentials reload from it, and the store runs with
  /// the registry's hooks.
  fn features_over(home: &Path, registry: Arc<Registry>) -> Arc<FeatureContext> {
    let config_registry = Arc::clone(&registry);
    let config_home = home.to_path_buf();
    let credentials_home = home.to_path_buf();
    let cfg = registry.load_config(home).unwrap();
    let store = Store::new(cfg, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).unwrap();
    Arc::new(FeatureContext {
      config: Arc::new(HotReload::new(
        home.join(CONFIG_FILENAME),
        "config_reload_failed",
        Box::new(move || config_registry.load_config(&config_home)),
      )),
      credentials: Arc::new(HotReload::new(
        home.join(CREDENTIALS_FILENAME),
        "credentials_reload_failed",
        Box::new(move || load_credentials(&credentials_home)),
      )),
      store: Arc::new(store),
      registry,
      startup: Arc::new(StartupState::default()),
    })
  }

  #[tokio::test]
  async fn lifespan_runs_its_steps_and_services_in_the_stated_order() {
    let home = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.features.alpha", alpha).unwrap();
    registry.register_package("src.features.beta", beta).unwrap();
    let features = features_over(home.path(), Arc::new(registry));

    let lifespan = Lifespan::start(features).await.unwrap();
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

    lifespan.shutdown().await.unwrap();
    let all = TRACE.lock().unwrap().clone();
    let stopped: Vec<&str> = all[started.len()..].iter().map(String::as_str).collect();
    assert_eq!(stopped, ["ready_beta:stop", "ready_alpha:stop", "early_beta:stop", "early_alpha:stop", "let_go"]);
  }
}
