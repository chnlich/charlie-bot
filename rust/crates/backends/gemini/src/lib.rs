//! The `gemini` backend type: the gemini CLI in stream-json mode, and the two legacy credential keys
//! (`gemini_api_key` and `gemini_model`) that the config migrates to `gemini.api_key` and `gemini.model`.

mod api;
pub mod factory;
mod gemini_cli;
mod options;
mod translate;

pub use api::{PACKAGE, register, register_with_host};
