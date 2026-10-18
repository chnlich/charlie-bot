//! The composition root: runtime registrations precede the package list from `src/app/registrations.py:16-46`.
//!
//! Each entry names a Python package and the `register` function that makes the package's contributions. A package
//! the port has not built calls a stub. A stub that carries the Python `register`'s data (its config section, its
//! metadata slots, its backend types) registers that data; a stub with no data makes no registration. The list keeps
//! every package, so the page parts, the static mounts and the template search path follow the Python order.

mod backends;
mod features;

use charliebot_registry::{RegisterFn, Registry, RegistryError};

/// Runtime registrations run before the Python package list, matching the server's pre-recovery ordering.
pub const RUNTIME: &[(&str, RegisterFn)] = &[("runtime.agents", charliebot_agents::register_runtime)];

/// The listed packages in registration order.
pub const PACKAGES: &[(&str, RegisterFn)] = &[
  ("src.features.artifacts", charliebot_artifacts::register), // stub: src.features.artifacts
  ("src.features.backup", charliebot_backup::register),       // stub: src.features.backup
  ("src.features.backend_pool", features::backend_pool),      // stub: src.features.backend_pool
  ("src.features.chat_threads", charliebot_chat_threads::register), // stub: src.features.chat_threads
  ("src.features.code_server", charliebot_code_server::register), // stub: src.features.code_server
  ("src.features.cron", features::cron),                      // stub: src.features.cron
  ("src.features.diag", charliebot_diag::register),           // stub: src.features.diag
  ("src.features.slack", charliebot_slack::register),         // stub: src.features.slack
  ("src.features.discord", charliebot_discord::register),     // stub: src.features.discord
  ("src.features.web_reply", charliebot_web_reply::register), // stub: src.features.web_reply
  ("src.features.host_auth", charliebot_host_auth::register), // stub: src.features.host_auth
  ("src.features.diff_view", charliebot_diff_view::register), // stub: src.features.diff_view
  ("src.features.explain", charliebot_explain::register),     // stub: src.features.explain
  ("src.features.improve", stub),                             // stub: src.features.improve
  ("src.features.memory", charliebot_memory::register),       // stub: src.features.memory
  ("src.features.ncu", charliebot_ncu::register),             // stub: src.features.ncu
  ("src.features.terminal", charliebot_terminal::register),   // stub: src.features.terminal
  ("src.features.trace", charliebot_trace::register),         // stub: src.features.trace
  ("src.features.usage", charliebot_usage::register),         // stub: src.features.usage
  ("src.features.voice", charliebot_voice::register),         // stub: src.features.voice
  ("src.backends.openai_compatible", backends::openai_compatible), // stub: src.backends.openai_compatible
  ("src.backends.antigravity", backends::antigravity),        // stub: src.backends.antigravity
  ("src.backends.charlie_code", backends::charlie_code),      // stub: src.backends.charlie_code
  ("src.backends.claude_code", backends::claude_code),        // stub: src.backends.claude_code
  ("src.backends.codex", backends::codex),                    // stub: src.backends.codex
  ("src.backends.gemini", backends::gemini),                  // stub: src.backends.gemini
  ("src.backends.kimi", backends::kimi),                      // stub: src.backends.kimi
  ("src.backends.opencode", backends::opencode),              // stub: src.backends.opencode
  ("src.features.files", charliebot_files::register),         // stub: src.features.files
];

/// The registry of every listed package, registered in list order. A failing registration fails the server's startup.
pub fn registry() -> Result<Registry, RegistryError> {
  let mut registry = Registry::new();
  for &(package, register) in RUNTIME {
    registry.register_package(package, register)?;
  }
  for &(package, register) in PACKAGES {
    registry.register_package(package, register)?;
  }
  Ok(registry)
}

/// The `register` of a stub that makes no registration.
fn stub(_registry: &mut Registry) -> Result<(), RegistryError> {
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::registry;

  #[test]
  fn the_listed_packages_render_the_five_chat_lines() {
    let registry = registry().expect("the listed packages register");
    for event_type in ["slack_reply", "discord_reply", "web_reply", "web_reply_notice", "web_status"] {
      assert!(registry.event_renderer(event_type).is_some(), "no renderer for {event_type}");
    }
    assert!(registry.event_renderer("assistant").is_none());
  }
}
