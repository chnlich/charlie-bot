//! Agent process transport, backend implementations, launch planning, and environment setup.

pub mod agent_environment;
pub mod backend_availability;
pub mod backend_pool;
pub mod backend_run;
pub mod backend_types;
pub mod call_resolve;
pub mod cli_base;
pub mod cli_host;
mod entry;
pub mod event_builders;
pub mod fake_probe;
pub mod gh_wrapper;
pub mod launch_loop;
pub mod local_work_guard;
pub mod model_pool;
pub mod one_shot;
pub mod pool_display;
pub mod runner;
pub mod runs;
pub mod ssh_guard;
pub mod temporary_unavailable;
pub mod transport;

pub use backend_types::build_backend;
pub use entry::register_runtime;
pub use one_shot::{agent_one_shot_text, one_shot_text};
