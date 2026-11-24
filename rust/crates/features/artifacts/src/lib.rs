//! `web/features/artifacts` holds the `publish:` config section and the artifact file view. It holds the session
//! plan registry with its routes and its four verbs `present`, `amend`, `approve` and `close`, and its sidebar
//! contribution. It holds the `plan` and `artifact check` subcommands.

mod api;
pub mod artifact_check;
mod artifact_view;
pub mod cli;
mod entry;
mod headless_render;
pub mod plan_diff;
mod plan_paths;
mod plan_verbs;
pub mod plans;
mod sequence_diff;
mod sidebar;
#[cfg(test)]
mod test_support;

pub use entry::{PACKAGE, close_renderer, register};
