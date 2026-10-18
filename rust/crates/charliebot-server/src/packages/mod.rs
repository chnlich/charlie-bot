//! The composition root: the server's package list, in the order of `src/app/registrations.py:16-46`.
//!
//! Each entry names a Python package and the `register` function that makes the package's contributions. A package
//! the port has not built calls a stub. A stub that carries the Python `register`'s data (its config section, its
//! metadata slots, its backend types) registers that data; a stub with no data makes no registration. The list keeps
//! every package, so the page parts, the static mounts and the template search path follow the Python order.

mod backends;
mod features;
mod voice;

use std::sync::Arc;

use charliebot_core::config::SectionValue;
use charliebot_registry::{RegisterFn, Registry, RegistryError};

/// The listed packages in registration order.
pub const PACKAGES: &[(&str, RegisterFn)] = &[
  ("src.features.artifacts", features::artifacts), // stub: src.features.artifacts
  ("src.features.backup", stub),                   // stub: src.features.backup
  ("src.features.backend_pool", features::backend_pool), // stub: src.features.backend_pool
  ("src.features.chat_threads", stub),             // stub: src.features.chat_threads
  ("src.features.code_server", features::code_server), // stub: src.features.code_server
  ("src.features.cron", features::cron),           // stub: src.features.cron
  ("src.features.diag", stub),                     // stub: src.features.diag
  ("src.features.slack", features::slack),         // stub: src.features.slack
  ("src.features.discord", features::discord),     // stub: src.features.discord
  ("src.features.web_reply", features::web_reply), // stub: src.features.web_reply
  ("src.features.host_auth", stub),                // stub: src.features.host_auth
  ("src.features.diff_view", stub),                // stub: src.features.diff_view
  ("src.features.explain", stub),                  // stub: src.features.explain
  ("src.features.improve", stub),                  // stub: src.features.improve
  ("src.features.memory", stub),                   // stub: src.features.memory
  ("src.features.ncu", stub),                      // stub: src.features.ncu
  ("src.features.terminal", stub),                 // stub: src.features.terminal
  ("src.features.trace", stub),                    // stub: src.features.trace
  ("src.features.usage", stub),                    // stub: src.features.usage
  ("src.features.voice", voice::register),         // stub: src.features.voice
  ("src.backends.openai_compatible", backends::openai_compatible), // stub: src.backends.openai_compatible
  ("src.backends.antigravity", backends::antigravity), // stub: src.backends.antigravity
  ("src.backends.charlie_code", backends::charlie_code), // stub: src.backends.charlie_code
  ("src.backends.claude_code", backends::claude_code), // stub: src.backends.claude_code
  ("src.backends.codex", backends::codex),         // stub: src.backends.codex
  ("src.backends.gemini", backends::gemini),       // stub: src.backends.gemini
  ("src.backends.kimi", backends::kimi),           // stub: src.backends.kimi
  ("src.backends.opencode", backends::opencode),   // stub: src.backends.opencode
  ("src.features.files", stub),                    // stub: src.features.files
];

/// The registry of every listed package, registered in list order. A failing registration fails the server's startup.
pub fn registry() -> Result<Registry, RegistryError> {
  let mut registry = Registry::new();
  for &(package, register) in PACKAGES {
    registry.register_package(package, register)?;
  }
  Ok(registry)
}

/// The `register` of a stub that makes no registration.
fn stub(_registry: &mut Registry) -> Result<(), RegistryError> {
  Ok(())
}

/// A config section kept as its YAML value until the owning package's port types it.
fn raw_section(value: &serde_yaml::Value) -> Result<SectionValue, String> {
  Ok(Arc::new(value.clone()))
}
