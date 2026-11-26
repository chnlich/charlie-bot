//! The composition root both binaries share: the runtime registrations and the server's package list, in the
//! server's order.
//!
//! Each entry names a package by its `features.*` or `backends.*` label and the `register` function that makes the package's
//! contributions. This crate owns the list: a package added to the server is added here. The list keeps every
//! package, so the page parts, the static mounts and the template search path keep their registration order.
//!
//! One crate owns both lists, so the server binary and the CLI binary cannot drift: a package added here reaches
//! both. `helper_entry` owns the argv entries a binary serves before it registers, the OpenCode relay included.

mod entry;
pub mod helper_entry;
mod ui_text_config;

pub use entry::{registry, seed_default_cron_tasks};
