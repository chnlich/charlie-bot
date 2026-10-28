//! Fixtures for the artifacts tests: a store-backed feature context over a temporary home, with this package registered,
//! the fake headless Chrome that the renderer and the assertions launch, and the plan page the assertions accept.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use charliebot_core::RepoRoot;
use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::context::StartupState;
use charliebot_registry::templating::Templating;
use charliebot_registry::{FeatureContext, Registry, store_hooks};
use charliebot_store::Store;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use serde_json::{Value, json};

use crate::headless_render;
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

/// The charlie-bot checkout, which holds `prompts/plan_template.html` and the page packages.
pub(crate) fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("the crate lies under the repository").to_path_buf()
}

/// A plan page that passes every plan assertion except the ones its tests break: the genre template's style block
/// verbatim, six numbered sections, and a footer, with `goal` as the Problem / Goal body.
pub(crate) fn plan_page(goal: &str) -> String {
  let template = fs::read_to_string(repo_root().join("prompts/plan_template.html")).unwrap();
  let style_start = template.find("<style>").unwrap();
  let style_end = template.find("</style>").unwrap() + "</style>".len();
  let style = &template[style_start..style_end];
  let titles = ["Problem / Goal", "Context", "High Level Solution", "Detailed Design", "Trade-offs", "Other Details"];
  let sections = titles
    .iter()
    .enumerate()
    .map(|(index, title)| {
      let body = if index == 0 { goal } else { *title };
      format!(
        "<section class=\"plan-section\"><h2><span class=\"n\">{}</span> {title}</h2><p>{body}</p></section>",
        index + 1
      )
    })
    .collect::<String>();
  format!("<html><head>{style}</head><body>{sections}<div class=\"foot\"><p>How to respond.</p></div></body></html>")
}

/// Writes `source` as `artifacts/<name>` under the session directory and returns that plan-relative path.
pub(crate) fn write_plan_artifact(features: &FeatureContext, session_id: &str, name: &str, source: &str) -> String {
  let directory = features.store.session_dir(session_id).join("artifacts");
  fs::create_dir_all(&directory).expect("the artifacts directory is created");
  fs::write(directory.join(name), source).expect("the artifact is written");
  format!("artifacts/{name}")
}

/// A feature context over a new temporary home, with the artifacts package registered. The home lives as long as the
/// returned directory.
pub(crate) fn features() -> (tempfile::TempDir, Arc<FeatureContext>) {
  let home = tempfile::tempdir().expect("a temporary home is available");
  let features = features_in(home.path());
  (home, features)
}

/// A feature context over the home at `home`, with the artifacts package registered and the checkout as the repo root.
fn features_in(home: &Path) -> Arc<FeatureContext> {
  let mut registry = Registry::new();
  registry
    .register_package("src.features.user_text_checks", charliebot_user_text_checks::register)
    .expect("the user text checks package registers");
  registry.register_package("src.features.artifacts", crate::register).expect("the artifacts package registers");
  let registry = Arc::new(registry);
  let config_value = registry.load_config(home).expect("the default config loads");
  let store = Arc::new(
    Store::new(config_value.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry)))
      .expect("the store opens over the temporary home"),
  );
  let config_registry = Arc::clone(&registry);
  let config_home = home.to_path_buf();
  let config = Arc::new(HotReload::new(
    config_value.config_file(),
    "artifacts_test_config_reload_failed",
    Box::new(move || config_registry.load_config(&config_home)),
  ));
  let credentials_home = home.to_path_buf();
  let credentials = Arc::new(HotReload::new(
    home.join(CREDENTIALS_FILENAME),
    "artifacts_test_credentials_reload_failed",
    Box::new(move || load_credentials(&credentials_home)),
  ));
  let repo_root = repo_root();
  let startup = StartupState::default();
  startup
    .insert(Templating::new(&repo_root, &["src.features.artifacts"]))
    .expect("the template engine enters startup state");
  startup
    .insert(RepoRoot::new(Some(repo_root.clone().into_os_string())).expect("the checkout is a non-empty path"))
    .expect("the repo root enters startup state");
  Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(startup) })
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

/// A root session, its `plans.json` when `registry` is given, and a feature context whose config names a fake headless
/// Chrome. The fake measures every page at 800 px, so the plan assertions run without a renderer. The home lives as long
/// as the returned directory.
pub(crate) fn plan_session(session_id: &str, registry: Option<&str>) -> (tempfile::TempDir, Arc<FeatureContext>) {
  let home = tempfile::tempdir().expect("a temporary home is available");
  let chrome = chrome_program(home.path(), json!([{"exit": 0}]));
  headless_render::set_test_height(&chrome, 800);
  let config = format!("headless_chrome_bin: {}\n", json!(chrome.display().to_string()));
  fs::write(home.path().join(CONFIG_FILENAME), config).expect("the config is written");
  let features = features_in(home.path());
  write_session(&features, session_id);
  if let Some(registry) = registry {
    fs::write(features.store.session_dir(session_id).join(PLANS_FILE), registry).expect("the registry is written");
  }
  (home, features)
}
