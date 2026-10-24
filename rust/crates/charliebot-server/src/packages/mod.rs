//! The composition root: runtime registrations precede the package list from `src/app/registrations.py:16-46`.
//!
//! Each entry names a Python package and the `register` function that makes the package's contributions. A package
//! the port has not built calls a stub. A stub that carries the Python `register`'s data (its config section, its
//! metadata slots, its backend types) registers that data; a stub with no data makes no registration. The list keeps
//! every package, so the page parts, the static mounts and the template search path follow the Python order.

use charliebot_registry::{RegisterFn, Registry, RegistryError};

/// Runtime registrations run before the Python package list, matching the server's pre-recovery ordering.
pub const RUNTIME: &[(&str, RegisterFn)] = &[
  ("runtime.agents", charliebot_agents::register_runtime),
  ("runtime.usage", charliebot_usage::register),
  ("runtime.session_api", charliebot_session_api::register),
  ("runtime.tasks", charliebot_tasks::register),
  ("runtime.triggers", charliebot_sched::register),
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
  ("src.features.cron", charliebot_sched::cron::register),
  ("src.features.diag", charliebot_diag::register),
  ("src.features.slack", charliebot_slack::register),
  ("src.features.discord", charliebot_discord::register),
  ("src.features.disk_sweep", charliebot_disk_sweep::register),
  ("src.features.web_reply", charliebot_web_reply::register),
  ("src.features.host_auth", charliebot_host_auth::register),
  ("src.features.diff_view", charliebot_diff_view::register),
  ("src.features.explain", charliebot_explain::register),
  ("src.features.improve", charliebot_sched::improve::register),
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

#[cfg(test)]
mod tests {
  use super::registry;

  #[test]
  fn the_listed_packages_render_the_five_chat_lines() {
    // Python: tests/test_scheduled_handlers.py::test_the_packages_register_the_built_in_handlers (lines 43-45)
    let registry = registry().expect("the listed packages register");
    for event_type in [
      "slack_reply",
      "discord_reply",
      "web_reply",
      "web_reply_notice",
      "web_status",
      "scheduled_run_skipped",
      "claude_account_login_required",
    ] {
      assert!(registry.event_renderer(event_type).is_some(), "no renderer for {event_type}");
    }
    assert!(registry.event_renderer("assistant").is_none());
    assert!(registry.websocket_match("/ws/sessions/synthetic").is_some());
    for name in ["backup", "usage_ledger", "cool_storage", "disk_sweep"] {
      assert!(registry.scheduled_handler_named(name).is_some(), "no scheduled handler for {name}");
    }
  }

  #[test]
  fn test_feature_registers_cli_and_scheduled_handler() {
    // Python: tests/test_disk_sweep.py::test_feature_registers_cli_and_scheduled_handler
    let registry = registry().expect("the listed packages register");
    assert!(registry.scheduled_handler_named("disk_sweep").is_some());
  }
}
