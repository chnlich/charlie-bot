//! Scheduled tasks in `config.d/cron.d`: routes, file loading, scheduling and sequence recovery.

mod api;
mod boundary;
pub mod config;
mod loader;
mod scheduler;
pub mod seed;
mod sequence_controller;
#[cfg(test)]
mod test_support;

use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_core::pyfmt;
use charliebot_registry::turn::{EventRenderer, TurnContribution};
use charliebot_registry::{Registry, RegistryError};
use serde_json::{Map, Value, json};

struct CronTurnContribution;

fn render_scheduled_run_skipped(event: &Value) -> Value {
  let field = |name: &str| event.get(name).map(pyfmt::python_str).unwrap_or_default();
  json!({"role":"system", "content":format!("Scheduled run of '{}' skipped at {}: {}", field("task"), field("skipped_at"), field("reason"))})
}

impl TurnContribution for CronTurnContribution {
  fn class_name(&self) -> &'static str {
    "CronTurnContribution"
  }

  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![(charliebot_core::event_types::SCHEDULED_RUN_SKIPPED, render_scheduled_run_skipped)]
  }
}

/// Register scheduled-task routes, metadata, the scheduler service and sequence owner.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router_patterns(api::routes, api::ROUTE_PATTERNS)?;
  registry.turn_contribution("cron", Arc::new(CronTurnContribution))?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: "cron",
      on: MetadataFile::Session,
      after: Some("backend"),
      fields: ["scheduled_task", "last_scheduled_run", "last_run_status", "last_scheduled_cron"]
        .into_iter()
        .map(|name| SlotField { name, default: Value::Null })
        .collect(),
    },
    validate_cron_fields,
  )?;
  registry.startup_check("cron_backend_refs", loader::backend_refs_check)?;
  let (registry_sender, registry_receiver) = tokio::sync::watch::channel(None);
  registry.service(Arc::new(sequence_controller::BindingContextService::new(registry_sender)))?;
  registry.service(Arc::new(scheduler::SchedulerService::default()))?;
  registry.sequence_controller("cron", Arc::new(sequence_controller::CronSequenceController::new(registry_receiver)))
}

fn validate_cron_fields(values: &mut Map<String, Value>) -> Option<String> {
  for name in ["scheduled_task", "last_scheduled_run", "last_scheduled_cron"] {
    if values.get(name).is_some_and(|value| !value.is_null() && !value.is_string()) {
      return Some(format!("{name} must be a string or null"));
    }
  }
  if values.get("last_run_status").is_some_and(|value| {
    !value.is_null() && !matches!(value.as_str(), Some("running" | "success" | "failed" | "skipped"))
  }) {
    return Some("last_run_status must be a LastRunStatus or null".to_string());
  }
  None
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;
  use std::sync::atomic::{AtomicUsize, Ordering};

  use charliebot_registry::Registry;
  use charliebot_registry::context::FeatureContext;
  use futures_util::future::BoxFuture;

  use crate::register;

  static HANDLER_POLLS: AtomicUsize = AtomicUsize::new(0);

  fn handler(_features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<String>> {
    Box::pin(async {
      HANDLER_POLLS.fetch_add(1, Ordering::SeqCst);
      Ok("synthetic result".to_string())
    })
  }

  fn register_handler(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
    registry.scheduled_handler("synthetic", handler)
  }

  fn register_duplicate_handler(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
    registry.scheduled_handler("synthetic", handler)
  }

  #[test]
  fn scheduled_handler_registration_keeps_python_name_and_lazy_lookup_rules() {
    // Python: tests/test_scheduled_handlers.py::test_a_second_registration_raises
    // Python: tests/test_scheduled_handlers.py::test_an_unregistered_name_resolves_to_none
    // Python: tests/test_scheduled_handlers.py::test_importing_the_scheduler_loads_no_handler_stack
    HANDLER_POLLS.store(0, Ordering::SeqCst);
    let mut registry = Registry::new();
    registry.register_package("test.cron", register).unwrap();
    assert!(registry.scheduled_handler_named("synthetic").is_none());
    registry.register_package("test.handler", register_handler).unwrap();
    assert!(registry.scheduled_handler_named("synthetic").is_some());
    assert_eq!(HANDLER_POLLS.load(Ordering::SeqCst), 0);
    assert!(registry.register_package("test.duplicate", register_duplicate_handler).is_err());
    assert_eq!(HANDLER_POLLS.load(Ordering::SeqCst), 0);
  }
}
