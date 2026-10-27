//! Agent process transport, backend implementations, launch planning, and environment setup.

pub mod agent_environment;
pub mod backend_pool;
pub mod backend_types;
pub mod backends;
pub mod cli_base;
pub mod event_builders;
pub mod launch_loop;
pub mod runner;
pub mod runs;
pub mod transport;

use charliebot_registry::{Registry, RegistryError};

pub use backend_types::build_backend;
pub use one_shot::{agent_one_shot_text, one_shot_text};

mod one_shot;

/// Register session cgroup and backend pool startup steps before package registration.
pub fn register_runtime(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.pre_recovery_step("session_cgroups", agent_environment::session_cgroup_step)?;
  registry.pre_recovery_step("backend_pool", backend_pool::pre_recovery_step)
}
