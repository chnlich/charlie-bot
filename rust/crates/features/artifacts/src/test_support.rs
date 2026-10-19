//! Fixtures for the artifacts tests: a store-backed feature context over a temporary home, with this package registered,
//! and the fake headless Chrome that the renderer and the assertions launch.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use charliebot_core::config::CREDENTIALS_FILENAME;
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::context::StartupState;
use charliebot_registry::templating::Templating;
use charliebot_registry::{FeatureContext, Registry, store_hooks};
use charliebot_store::Store;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use serde_json::{Value, json};

use crate::plans::PLANS_FILE;

/// A fake headless Chrome that plays `steps`, the `chrome-stub` helper's steps (`src/bin/chrome_stub.rs`). The fake is
/// a new `stub` directory under `directory`, holding a symlink named `chrome` to the helper and a `probe.json` with the
/// steps. Returns the symlink's path. Nothing is written as an executable, so no test thread's fork can hold one open.
pub(crate) fn chrome_program(directory: &Path, steps: Value) -> PathBuf {
  let stub = directory.join("stub");
  fs::create_dir(&stub).expect("the stub directory is created");
  let program = stub.join("chrome");
  symlink(chrome_stub_helper(), &program).expect("the fake links to the helper");
  fs::write(stub.join("probe.json"), json!({"chrome": steps}).to_string()).expect("the fake's steps are written");
  program
}

/// The `chrome-stub` helper that cargo builds beside this test binary. Unit tests cannot name it with `CARGO_BIN_EXE_*`,
/// so the path comes from the target directory.
fn chrome_stub_helper() -> PathBuf {
  let exe = std::env::current_exe().expect("the test binary path");
  exe
    .ancestors()
    .skip(1)
    .map(|directory| directory.join("chrome-stub"))
    .find(|candidate| candidate.is_file())
    .expect("cargo builds the chrome-stub helper into the target directory that holds the test binary")
}

/// A feature context over a new temporary home, with the artifacts package registered. The home lives as long as the
/// returned directory.
pub(crate) fn features() -> (tempfile::TempDir, Arc<FeatureContext>) {
  let home = tempfile::tempdir().expect("a temporary home is available");
  let mut registry = Registry::new();
  registry.register_package("src.features.artifacts", crate::register).expect("the artifacts package registers");
  let registry = Arc::new(registry);
  let config_value = registry.load_config(home.path()).expect("the default config loads");
  let store = Arc::new(
    Store::new(config_value.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry)))
      .expect("the store opens over the temporary home"),
  );
  let config_registry = Arc::clone(&registry);
  let config_home = home.path().to_path_buf();
  let config = Arc::new(HotReload::new(
    config_value.config_file(),
    "artifacts_test_config_reload_failed",
    Box::new(move || config_registry.load_config(&config_home)),
  ));
  let credentials_home = home.path().to_path_buf();
  let credentials = Arc::new(HotReload::new(
    home.path().join(CREDENTIALS_FILENAME),
    "artifacts_test_credentials_reload_failed",
    Box::new(move || load_credentials(&credentials_home)),
  ));
  let startup = StartupState::default();
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../");
  startup
    .insert(Templating::new(&repo_root, &["src.features.artifacts"]))
    .expect("the template engine enters startup state");
  let features = Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(startup) });
  (home, features)
}

/// Writes a root session's metadata through the registered slots, so the store reads it back as the store writes it.
pub(crate) fn write_session(features: &FeatureContext, session_id: &str) {
  let slots = features.registry.metadata_slots();
  let meta = SessionMetadata::new(session_id, session_id, TaskProfile::Manager);
  let path = features.store.metadata_path(session_id);
  fs::create_dir_all(path.parent().expect("a session file has a directory")).expect("the session directory is created");
  fs::write(path, meta.to_json_string(&slots, true)).expect("the metadata is written");
}

/// Plan 1 awaiting approval at its version 1.
pub(crate) const AWAITING_REGISTRY: &str = r#"{"plans": [{"id": 1, "title": "P1", "versions": [{"v": 1, "file": "artifacts/plan_01.html",
    "created_at": "2026-07-20T00:00:00+00:00", "trigger": "initial", "base": null, "note": null}],
    "takeoff": null, "closed": null}]}"#;

/// Plans 1 and 2, both awaiting approval.
pub(crate) const TWO_AWAITING_REGISTRY: &str = r#"{"plans": [
  {"id": 1, "title": "P1", "versions": [{"v": 1, "file": "artifacts/plan_01.html"}], "takeoff": null, "closed": null},
  {"id": 2, "title": "P2", "versions": [{"v": 1, "file": "artifacts/plan_02.html"}], "takeoff": null, "closed": null}
]}"#;

/// Plan 1, closed as completed.
pub(crate) const CLOSED_REGISTRY: &str = r#"{"plans": [{"id": 1, "title": "P1", "versions": [{"v": 1, "file": "artifacts/plan_01.html"}],
  "takeoff": null, "closed": {"as": "completed", "at": "2026-07-20T01:00:00+00:00"}}]}"#;

/// Plan 1, approved at its version 1.
pub(crate) const APPROVED_REGISTRY: &str = r#"{"plans": [{"id": 1, "title": "P1", "versions": [{"v": 1, "file": "artifacts/plan_01.html"}],
  "takeoff": {"v": 1, "at": "2026-07-20T01:00:00+00:00"}, "closed": null}]}"#;

/// A root session whose `plans.json` holds `registry`, and its feature context. The home lives as long as the returned
/// directory.
pub(crate) fn session_with_registry(session_id: &str, registry: &str) -> (tempfile::TempDir, Arc<FeatureContext>) {
  let (home, features) = features();
  write_session(&features, session_id);
  fs::write(features.store.session_dir(session_id).join(PLANS_FILE), registry).expect("the registry is written");
  (home, features)
}
