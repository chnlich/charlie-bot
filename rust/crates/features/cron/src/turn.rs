//! The cron turn contribution: it renders a skipped scheduled run as a system message in the transcript.

use charliebot_core::text_repr;
use charliebot_registry::turn::{EventRenderer, TurnContribution};
use serde_json::{Value, json};

pub(crate) struct CronTurnContribution;

fn render_scheduled_run_skipped(event: &Value) -> Value {
  let field = |name: &str| event.get(name).map(text_repr::value_str).unwrap_or_default();
  json!({"role":"system", "content":format!("Scheduled run of '{}' skipped at {}: {}", field("task"), field("skipped_at"), field("reason"))})
}

impl TurnContribution for CronTurnContribution {
  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![(charliebot_core::event_types::SCHEDULED_RUN_SKIPPED, render_scheduled_run_skipped)]
  }
}
