//! Manager turns and task execution adapters.

pub mod autonamer;
pub mod cold_read;
pub mod dispatch;
pub mod execution;
pub mod master_queue;
pub mod master_run;
pub mod master_state;
pub mod master_trigger;
pub mod prompts;
pub mod review_text;
pub mod routes;
pub mod session_anchors;
pub mod spawner_backends;
pub mod spawner_prompt;
pub mod task_sessions;
pub mod verify_trailer;

use charliebot_registry::{Registry, RegistryError};

/// Register task orchestration routes and the execution startup step.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(routes::routes, routes::matches)?;
  registry.pre_recovery_step("task_execution", execution::pre_recovery_step)
}
