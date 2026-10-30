//! The server's composition root. `charliebot_packages` owns the runtime and package registration lists, which the
//! server binary and the CLI binary share, so the two cannot drift. The tests here check the server's registry.

pub use charliebot_packages::registry;

#[cfg(test)]
mod thread_rules_tests;

#[cfg(test)]
mod web_search_gate;

#[cfg(test)]
mod tests {
  use std::env;
  use std::fs;
  use std::path::PathBuf;
  use std::sync::Arc;

  use charliebot_registry::store_hooks::store_hooks;

  use super::registry;

  #[test]
  fn the_listed_packages_render_the_five_chat_lines() {
    // Python: tests/test_scheduled_handlers.py::test_the_packages_register_the_built_in_handlers (lines 43-45)
    let registry = Arc::new(registry().expect("the listed packages register"));
    let hooks = store_hooks(Arc::clone(&registry));
    let renderers =
      hooks.runtime_events.expect("the runtime event hooks are built").event_renderers().expect("the renderers read");
    for event_type in [
      "slack_reply",
      "discord_reply",
      "web_reply",
      "web_reply_notice",
      "web_status",
      "scheduled_run_skipped",
      "claude_account_login_required",
    ] {
      assert!(renderers.iter().any(|(known, _)| known == event_type), "no renderer for {event_type}");
    }
    assert!(renderers.iter().all(|(known, _)| known != "assistant"));
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

  #[test]
  fn registration_inventory_matches_the_committed_file() {
    let registry = registry().expect("the listed packages register");
    let rendered = format!("{}\n", registry.inventory().join("\n"));
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("registration_inventory.txt");
    let update = match env::var("UPDATE_REGISTRATION_INVENTORY") {
      Ok(value) if value == "1" => true,
      Ok(value) if value == "0" => false,
      Ok(value) => panic!("UPDATE_REGISTRATION_INVENTORY must be 0 or 1, got {value:?}"),
      Err(env::VarError::NotPresent) => false,
      Err(error) => panic!("cannot read UPDATE_REGISTRATION_INVENTORY: {error}"),
    };
    if update {
      fs::write(&path, &rendered).expect("write registration inventory");
      panic!("wrote {}; rerun without UPDATE_REGISTRATION_INVENTORY=1", path.display());
    }

    let expected = fs::read_to_string(&path).expect("read committed registration inventory");
    let expected_lines: Vec<&str> = expected.lines().collect();
    let rendered_lines: Vec<&str> = rendered.lines().collect();
    let first_difference =
      expected_lines.iter().zip(&rendered_lines).position(|(expected, actual)| expected != actual).or_else(|| {
        (expected_lines.len() != rendered_lines.len()).then_some(expected_lines.len().min(rendered_lines.len()))
      });
    if let Some(index) = first_difference {
      let expected_line = expected_lines.get(index).copied().unwrap_or("<missing>");
      let rendered_line = rendered_lines.get(index).copied().unwrap_or("<missing>");
      panic!(
        "registration inventory differs at line {}:\n expected: {}\n actual: {}",
        index + 1,
        expected_line,
        rendered_line
      );
    }
  }
}
