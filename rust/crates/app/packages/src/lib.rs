//! The composition root both binaries share: the runtime registrations and the server's package list, in the
//! server's order.
//!
//! Each entry names a package by its `src.*` label and the `register` function that makes the package's
//! contributions. This crate owns the list. It began as the package list of `src/app/registrations.py` (that file's
//! lines 16-46 at the port's start); the Python list now serves only the remaining Python scripts and no longer
//! matches, so a package added to the server is added here. The list keeps every package, so the page parts, the
//! static mounts and the template search path keep their registration order.
//!
//! One crate owns both lists, so the server binary and the CLI binary cannot drift: a package added here reaches
//! both.

use charliebot_registry::{RegisterFn, Registry, RegistryError};

/// Runtime registrations run before the package list, matching the server's pre-recovery ordering.
pub const RUNTIME: &[(&str, RegisterFn)] = &[
  ("runtime.agents", charliebot_agents::register_runtime),
  ("runtime.usage", charliebot_usage::register),
  ("runtime.session_api", charliebot_session_api::register),
  ("runtime.tasks", charliebot_tasks::register),
  ("runtime.triggers", charliebot_triggers::register),
];

/// The listed packages in registration order.
pub const PACKAGES: &[(&str, RegisterFn)] = &[
  ("src.features.user_text_checks", charliebot_user_text_checks::register),
  ("src.features.artifacts", charliebot_artifacts::register),
  ("src.features.backup", charliebot_backup::register),
  ("src.features.backend_pool", charliebot_agents::backend_pool::register),
  ("src.features.chat_threads", charliebot_chat_threads::register),
  ("src.features.code_server", charliebot_code_server::register),
  ("src.features.cron", charliebot_cron::register),
  ("src.features.diag", charliebot_diag::register),
  ("src.features.slack", charliebot_slack::register),
  ("src.features.discord", charliebot_discord::register),
  ("src.features.disk_sweep", charliebot_disk_sweep::register),
  ("src.features.web_reply", charliebot_web_reply::register),
  ("src.features.host_auth", charliebot_host_auth::register),
  ("src.features.diff_view", charliebot_diff_view::register),
  ("src.features.explain", charliebot_explain::register),
  ("src.features.improve", charliebot_improve::register),
  ("src.features.memory", charliebot_memory::register),
  ("src.features.terminal", charliebot_terminal::register),
  ("src.features.trace", charliebot_trace::register),
  ("src.features.usage", charliebot_usage_page::register),
  ("src.features.voice", charliebot_voice::register),
  ("src.features.web_search", charliebot_web_search::register),
  ("src.backends.openai_compatible", charliebot_openai_compatible::register),
  ("src.backends.antigravity", charliebot_antigravity::register),
  ("src.backends.charlie_code", charliebot_charlie_code::register),
  ("src.backends.claude_code", charliebot_claude_code::register),
  ("src.backends.codex", charliebot_codex::register),
  ("src.backends.gemini", charliebot_gemini::register),
  ("src.backends.anthropic_compatible", charliebot_anthropic_compatible::register),
  ("src.backends.opencode", charliebot_opencode::register),
  ("src.features.files", charliebot_files::register),
];

/// The registry of every listed package, registered in list order. A failing registration fails the server's
/// startup.
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
