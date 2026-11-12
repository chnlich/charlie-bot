//! The plug-in seam of the Rust server. The [`Registry`] is what every feature and backend crate fills through its
//! `register` function. The seam also carries the lifespan steps of the crash-recovery task, and the typed startup
//! state that steps, services and route handlers share.
//!
//! The server builds one registry at startup, calling each listed package's `register` in the composition order
//! (`charliebot_packages`), then shares it read-only. Each hook kind has one typed registration method and one
//! reader. `docs/registry.md` lists them.

pub mod backend;
pub mod caller;
pub mod context;
pub mod draft;
mod inlined_literal;
mod inlined_script;
mod inlined_style;
pub mod lifespan;
mod member;
pub mod member_failure;
pub mod midturn_delivery;
pub mod option_model;
pub mod pages;
pub mod registry;
pub mod route_claims;
pub mod sequence;
pub mod sidebar;
mod startup_stop;
pub mod startup_timing;
pub mod store_hooks;
pub mod task_errors;
pub mod templating;
pub mod thread_origin;
pub mod turn;
pub mod usage;
pub mod usage_sweep;
mod watch;
pub mod wiring;

pub use context::{FeatureContext, StartupState, StartupStop, StateError};
pub use registry::{RegisterFn, Registry, RegistryError, registration_step};
pub use wiring::{WebSocketHandler, WebSocketMatch, WebSocketRequest};
