//! Manager turns and task execution adapters.

pub mod autonamer;
pub mod cold_read;
pub mod completion;
pub mod dispatch;
pub mod execution;
pub mod git;
pub mod master_queue;
pub mod master_run;
pub mod master_state;
pub mod master_trigger;
pub mod midturn_delivery;
pub mod prompts;
pub mod recovery;
pub mod review;
pub mod review_text;
pub mod routes;
pub mod run_support;
pub mod session_anchors;
pub mod spawner_backends;
pub mod spawner_prompt;
pub mod task_input_prompt;
pub mod task_sessions;
pub mod text_length;
pub mod upload;
pub mod verify_trailer;
pub mod worker;

#[cfg(test)]
mod test_fixtures;

use charliebot_registry::{Registry, RegistryError};

/// Register task orchestration routes and the execution startup step.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router_patterns(routes::routes, routes::ROUTE_PATTERNS)?;
  registry.pre_recovery_step("task_execution", execution::pre_recovery_step)?;
  registry.recovery_step("task_recovery", recovery::recovery_step)?;
  registry.let_go_step("task_execution", recovery::let_go_step)
}
