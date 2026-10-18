//! Agent process transport, backend implementations, launch planning, and environment setup.

pub mod agent_environment;
pub mod backend_types;
pub mod backends;
pub mod launch_loop;
pub mod runner;
pub mod transport;

use charliebot_registry::{Registry, RegistryError};

/// Register agent environment and session cgroup startup steps before package registration.
pub fn register_runtime(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.pre_recovery_step("agent_environment", agent_environment::pre_recovery_step)?;
  registry.pre_recovery_step("session_cgroups", agent_environment::session_cgroup_step)
}
