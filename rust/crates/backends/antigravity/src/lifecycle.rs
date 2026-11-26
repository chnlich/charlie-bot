//! The antigravity lifecycle: a gated manager turn names its Run to the process, which then takes live input from it.

use charliebot_registry::backend::BackendLifecycle;
use charliebot_registry::midturn_delivery::launch_kwargs;
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::run_records::RunKind;
use serde_json::{Map, Value};

use super::antigravity_cli::MIDTURN_RUN_ID_KWARG;

pub struct AntigravityLifecycle;

impl BackendLifecycle for AntigravityLifecycle {
  fn midturn_launch_kwargs(
    &self,
    meta: &SessionMetadata,
    turn: bool,
    run_kind: RunKind,
    run_id: Option<&str>,
  ) -> anyhow::Result<Map<String, Value>> {
    Ok(match launch_kwargs(meta, turn, run_kind, run_id)? {
      Some(run_id) => Map::from_iter([(MIDTURN_RUN_ID_KWARG.to_string(), Value::String(run_id))]),
      None => Map::new(),
    })
  }
}
