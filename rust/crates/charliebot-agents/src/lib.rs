//! The `charliebot-agents` crate owns process transport below agent backends.

pub mod backend_types;
pub mod backends;
pub mod event_builders;
pub mod launch_loop;
pub mod runner;
pub mod runs;
pub mod transport;

use charliebot_registry::{Registry, RegistryError};

pub use backend_types::build_backend;
pub use one_shot::agent_one_shot_text;

mod one_shot;

/// Register agent environment and session cgroup startup steps before package registration.
pub fn register_runtime(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.pre_recovery_step(runner::pre_recovery_step);
  registry.startup_step("session_cgroups", runner::session_cgroup_step);
  Ok(())
}
