//! The `charlie-code` backend type: the CharlieCode CLI against an OpenAI-compatible endpoint. CLC's usage lives only
//! in CharlieBot's own run logs, so its usage source has no log reader.

mod api;
mod charlie_code_cli;
pub mod factory;
pub mod flags;
pub mod options;
mod translate;

pub use api::{PACKAGE, TYPE_NAME, USAGE_SOURCE, register, register_with_host};
pub use charlie_code_cli::{API_KEY_ENV, BINARY_NAME, TASK_FILE_NAME};
