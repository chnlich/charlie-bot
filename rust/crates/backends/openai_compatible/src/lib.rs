//! The `cc-openai-compatible` backend type and the Anthropic proxy route its calls go through.
//! The Claude CLI runs every call, so the type's usage counts under Claude Code.

mod anthropic_proxy;
mod api;
mod factory;
mod openai_compatible_claude;
mod options;
mod sse;
mod translate;
mod value_ops;

pub use api::{PACKAGE, register};
