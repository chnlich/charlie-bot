//! The plug-in seam of the Rust server. The [`Registry`] is what every feature and backend crate fills through its
//! `register` function. The seam also carries the lifespan steps of the crash-recovery task, and the typed startup
//! state that steps, services and route handlers share.
//!
//! The server builds one registry at startup, calling each listed package's `register` in the order of the
//! composition root (`charliebot_packages`, whose list began as `src/app/registrations.py:16-46`), then shares it
//! read-only. Each Python hook kind has one
//! typed registration method and one reader. `rust/docs/registry.md` lists them with their Python sources.

pub mod backend;
pub mod caller;
pub mod context;
pub mod lifespan;
pub mod midturn_delivery;
pub mod pages;
pub mod registry;
pub mod route_claims;
pub mod sequence;
pub mod sidebar;
mod startup_stop;
pub mod store_hooks;
pub mod task_errors;
pub mod templating;
pub mod turn;
pub mod usage;
mod watch;
pub mod wiring;

pub use context::{FeatureContext, RunningTurnInputs, StartupState, StartupStop, StateError};
pub use registry::{RegisterFn, Registry, RegistryError, registration_step};
pub use wiring::{WebSocketHandler, WebSocketMatch, WebSocketRequest};
