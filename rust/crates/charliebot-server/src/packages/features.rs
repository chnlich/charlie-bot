//! The `src.features.cron` stub: the metadata slot of the session file. The other feature packages register from
//! their own crates under `rust/crates/features/`.

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_core::pyfmt;
use charliebot_registry::turn::{EventRenderer, TurnContribution};
use charliebot_registry::{Registry, RegistryError};
use serde_json::{Value, json};
use std::sync::Arc;

struct CronTurnContribution;

fn render_scheduled_run_skipped(event: &Value) -> Value {
  let field = |name: &str| event.get(name).map(pyfmt::python_str).unwrap_or_default();
  json!({
    "role": "system",
    "content": format!(
      "Scheduled run of '{}' skipped at {}: {}",
      field("task"),
      field("skipped_at"),
      field("reason")
    ),
  })
}

impl TurnContribution for CronTurnContribution {
  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![(charliebot_core::event_types::SCHEDULED_RUN_SKIPPED, render_scheduled_run_skipped)]
  }
}

/// `src/features/cron`: the scheduled-task fields of each session.
pub fn cron(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.turn_contribution("cron", Arc::new(CronTurnContribution))?;
  registry.metadata_slot(MetadataSlot {
    owner: "cron",
    on: MetadataFile::Session,
    after: Some("backend"),
    fields: ["scheduled_task", "last_scheduled_run", "last_run_status", "last_scheduled_cron"]
      .into_iter()
      .map(|name| SlotField { name, default: serde_json::Value::Null })
      .collect(),
  })
}
