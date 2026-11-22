//! The server's composition root. `charliebot_packages` owns the runtime and package registration lists, which the
//! server binary and the CLI binary share, so the two cannot drift. It also owns the boot-time seeding a package runs
//! before the store opens. The tests here check the server's registry.

pub use charliebot_packages::{registry, seed_default_cron_tasks};

#[cfg(test)]
mod thread_rules_tests;

#[cfg(test)]
mod web_search_gate;

/// The composed app over `home`, whose access key is the one its `credentials.yaml` names, with the repository's
/// templates rendering the pages. Returns the features beside the service, so a test seeds the store the app reads.
#[cfg(test)]
pub(crate) fn composed_app(
  home: &std::path::Path,
) -> (
  tower::util::BoxCloneSyncService<
    axum::http::Request<axum::body::Body>,
    axum::response::Response,
    std::convert::Infallible,
  >,
  std::sync::Arc<charliebot_registry::FeatureContext>,
) {
  use std::sync::Arc;

  use charliebot_compat::mimetypes::MimeTypes;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::templating::Templating;
  use charliebot_registry::{FeatureContext, StartupState};
  use charliebot_store::Store;

  use crate::app;
  use crate::buildinfo::BuildInfo;
  use crate::state::AppState;

  let registry = Arc::new(registry().expect("the listed packages register"));
  let config = registry.load_config(home).expect("the test config loads");
  let store = Arc::new(
    Store::new(config, registry.metadata_slots(), charliebot_registry::store_hooks::store_hooks(Arc::clone(&registry)))
      .expect("the test store"),
  );
  let config_registry = Arc::clone(&registry);
  let config_home = home.to_path_buf();
  let credentials_home = home.to_path_buf();
  let features = Arc::new(FeatureContext {
    config: Arc::new(HotReload::new(
      home.join("config.yaml"),
      "test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    )),
    credentials: Arc::new(HotReload::new(
      home.join("credentials.yaml"),
      "test_credentials_reload_failed",
      Box::new(move || charliebot_core::credentials::load_credentials(&credentials_home)),
    )),
    store,
    registry,
    startup: Arc::new(StartupState::default()),
  });
  let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
  features.startup.insert(Templating::new(&repo_root, &[])).expect("no other Templating was inserted");
  let state = Arc::new(AppState {
    features: Arc::clone(&features),
    build_info: BuildInfo { sha: "synthetic".to_string(), started_at: "synthetic".to_string() },
    static_mounts: Vec::new(),
    mime_types: MimeTypes::builtin(),
  });
  (app::build(state), features)
}

#[cfg(test)]
mod tests {
  use std::env;
  use std::fs;
  use std::path::{Path, PathBuf};
  use std::sync::Arc;

  use charliebot_registry::store_hooks::store_hooks;

  use super::registry;

  /// The page packages that carry frontend files. The other registered packages ship no template or static
  /// directory, so `package_directory` may resolve them to a path no checkout has.
  const PAGE_PACKAGES_WITH_FRONTEND_FILES: &[&str] = &[
    "features.artifacts",
    "features.chat_threads",
    "features.code_server",
    "features.cron",
    "features.diag",
    "features.diff_view",
    "features.explain",
    "features.host_auth",
    "features.terminal",
    "features.trace",
    "features.usage",
    "features.voice",
    "backends.claude_code",
    "backends.codex",
  ];

  #[test]
  #[ignore = "integration"]
  fn frontend_page_packages_resolve_to_directories_with_files() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    for package in PAGE_PACKAGES_WITH_FRONTEND_FILES {
      let directory = charliebot_registry::pages::package_directory(&repo_root, package);
      assert!(directory.is_dir(), "{package} resolves to {} which is not a directory", directory.display());
      let carries_files = ["templates", "static"]
        .iter()
        .any(|part| fs::read_dir(directory.join(part)).map(|mut entries| entries.next().is_some()).unwrap_or(false));
      assert!(carries_files, "{package} resolves to {} which holds no templates or static files", directory.display());
    }
  }

  #[test]
  fn the_listed_packages_render_the_five_chat_lines() {
    let registry = Arc::new(registry().expect("the listed packages register"));
    let hooks = store_hooks(Arc::clone(&registry));
    let renderers =
      hooks.runtime_events.expect("the runtime event hooks are built").event_renderers().expect("the renderers read");
    for event_type in [
      "slack_reply",
      "discord_reply",
      "feishu_reply",
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
    let registry = registry().expect("the listed packages register");
    assert!(registry.scheduled_handler_named("disk_sweep").is_some());
  }

  #[test]
  #[ignore = "integration"]
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
