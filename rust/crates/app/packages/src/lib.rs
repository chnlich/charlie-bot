//! The composition root both binaries share: the runtime registrations and the package list from
//! `src/app/registrations.py:16-46`, in the server's order.
//!
//! Each entry names a Python package and the `register` function that makes the package's contributions. A package
//! the port has not built calls a stub. A stub that carries the Python `register`'s data (its config section, its
//! metadata slots, its backend types) registers that data; a stub with no data makes no registration. The list keeps
//! every package, so the page parts, the static mounts and the template search path follow the Python order.
//!
//! One crate owns both lists, so the server binary and the CLI binary cannot drift: a package added here reaches
//! both.

use charliebot_registry::{RegisterFn, Registry, RegistryError};

/// Runtime registrations run before the Python package list, matching the server's pre-recovery ordering.
pub const RUNTIME: &[(&str, RegisterFn)] = &[
  ("runtime.agents", charliebot_agents::register_runtime),
  ("runtime.usage", charliebot_usage::register),
  ("runtime.session_api", charliebot_session_api::register),
  ("runtime.tasks", charliebot_tasks::register),
  ("runtime.triggers", charliebot_triggers::register),
];

fn register_python_only_package(_registry: &mut Registry) -> Result<(), RegistryError> {
  Ok(())
}

/// The listed packages in registration order.
pub const PACKAGES: &[(&str, RegisterFn)] = &[
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
  ("src.features.ncu", charliebot_ncu::register),
  ("src.features.router", register_python_only_package),
  ("src.features.terminal", charliebot_terminal::register),
  ("src.features.trace", charliebot_trace::register),
  ("src.features.usage", charliebot_usage_page::register),
  ("src.features.voice", charliebot_voice::register),
  ("src.backends.openai_compatible", charliebot_agents::backends::openai_compatible::register),
  ("src.backends.antigravity", charliebot_agents::backends::antigravity::register),
  ("src.backends.charlie_code", charliebot_agents::backends::charlie_code::register),
  ("src.backends.claude_code", charliebot_agents::backends::claude_code::register),
  ("src.backends.codex", charliebot_agents::backends::codex::register),
  ("src.backends.gemini", charliebot_agents::backends::gemini::register),
  ("src.backends.anthropic_compatible", charliebot_agents::backends::anthropic_compatible::register),
  ("src.backends.opencode", charliebot_agents::backends::opencode::register),
  ("src.features.files", charliebot_files::register),
];

/// The packages whose Python module has no Rust crate: `PACKAGES` gives each of them a no-op stub.
const PYTHON_ONLY_PACKAGES: &[&str] = &["src.features.router"];

/// The registry of every listed package, registered in list order. A failing registration fails the server's
/// startup.
pub fn registry() -> Result<Registry, RegistryError> {
  build_registry(|_| true)
}

/// The registry a CLI process composes: [`registry`] without the Python-only stubs.
///
/// A CLI verb loads the config and resolves backend types; it renders no page and serves no route, so a no-op stub
/// registers nothing it uses. This omission is the only difference between the two binaries' registries.
pub fn cli_registry() -> Result<Registry, RegistryError> {
  build_registry(|package| !PYTHON_ONLY_PACKAGES.contains(&package))
}

fn build_registry(include_package: impl Fn(&str) -> bool) -> Result<Registry, RegistryError> {
  let mut registry = Registry::new();
  for &(package, register) in RUNTIME {
    registry.register_package(package, register)?;
  }
  for &(package, register) in PACKAGES {
    if include_package(package) {
      registry.register_package(package, register)?;
    }
  }
  Ok(registry)
}
