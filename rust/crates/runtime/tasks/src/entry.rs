//! The tasks package's registration: the task routes, the execution and recovery steps and the worktree removal hook.

use charliebot_registry::{Registry, RegistryError};

use crate::{execution, git, recovery, routes};

/// Register task orchestration routes and the execution startup step.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router_patterns(routes::routes, routes::ROUTE_PATTERNS)?;
  registry.pre_recovery_step("task_execution", execution::pre_recovery_step)?;
  registry.recovery_step("task_recovery", recovery::recovery_step)?;
  registry.worktree_removal(git::remove_worktree_hook)?;
  registry.let_go_step("task_execution", recovery::let_go_step)
}
