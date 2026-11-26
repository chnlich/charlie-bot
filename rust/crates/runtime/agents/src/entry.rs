//! The agents package's registration: the pre-recovery steps, the child environment of every agent process and the
//! model pool.

use charliebot_registry::{Registry, RegistryError};

use crate::{agent_environment, backend_pool, local_work_guard, model_pool};

/// Register session cgroup and backend pool startup steps before package registration, and the local work guard's
/// variables for every agent child process.
pub fn register_runtime(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.pre_recovery_step("session_cgroups", agent_environment::session_cgroup_step)?;
  registry.pre_recovery_step("backend_pool", backend_pool::pre_recovery_step)?;
  registry.child_env("local_work_guard", local_work_guard::child_env)?;
  model_pool::register(registry)
}
