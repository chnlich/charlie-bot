//! The scheduled handler end to end: a failed run DMs the host owner the reason, and a successful run sends nothing.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use charliebot_backup::config::ResticSettings;
use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{FeatureContext, Registry, StartupState};
use charliebot_store::Store;
use futures_util::future::BoxFuture;

/// Every DM the synthetic owner sender carries, in order.
static DMS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record_owner_dm<'a>(_features: &'a FeatureContext, text: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
  Box::pin(async move {
    DMS.lock().expect("the synthetic DM record is never poisoned").push(text.to_string());
    Ok("synthetic-ts".to_string())
  })
}

/// A feature context over `home` holding the backup package and a synthetic owner-DM sender.
fn features_over(home: &Path) -> Arc<FeatureContext> {
  let mut registry = Registry::new();
  registry.register_package("features.backup", charliebot_backup::register).expect("the backup package registers");
  registry
    .register_package("features.synthetic", |registry| registry.owner_dm(record_owner_dm))
    .expect("the synthetic owner DM registers");
  let registry = Arc::new(registry);

  let config_registry = Arc::clone(&registry);
  let config_home = home.to_path_buf();
  let credentials_home = home.to_path_buf();
  let cfg = registry.load_config(home).expect("the synthetic config loads");
  let store = Store::new(cfg, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("the store builds");
  Arc::new(FeatureContext {
    config: Arc::new(HotReload::new(
      home.join(CONFIG_FILENAME),
      "config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    )),
    credentials: Arc::new(HotReload::new(
      home.join(CREDENTIALS_FILENAME),
      "credentials_reload_failed",
      Box::new(move || load_credentials(&credentials_home)),
    )),
    store: Arc::new(store),
    registry,
    startup: Arc::new(StartupState::default()),
  })
}

/// The home's `config.yaml`, written with the given backup section body.
fn write_backup_config(home: &Path, body: &str) {
  fs::write(home.join(CONFIG_FILENAME), format!("backup:\n{body}\n")).unwrap();
}

#[test]
#[ignore = "integration"]
fn the_handler_dms_the_owner_only_when_it_fails() {
  let root = tempfile::tempdir().unwrap();
  let home = root.path().join("home");
  fs::create_dir(&home).unwrap();

  // A run without backup.repository fails with its reason before restic is called, and DMs that reason.
  write_backup_config(&home, "  password_file: $CHARLIEBOT_HOME/secrets/backup-restic.key");
  let features = features_over(&home);
  let handler = features.registry.scheduled_handler_named("backup").expect("the backup handler registers");
  let error = tokio::runtime::Runtime::new()
    .unwrap()
    .block_on(handler(Arc::clone(&features)))
    .expect_err("the handler needs backup.repository");
  assert!(error.to_string().contains("backup.repository is not set"), "{error:#}");
  let dms = DMS.lock().expect("the synthetic DM record is never poisoned").clone();
  assert_eq!(dms.len(), 1, "a failed run sends exactly one DM");
  assert!(dms[0].contains("backup.repository is not set"), "{}", dms[0]);

  // A successful run against a local repository sends nothing.
  DMS.lock().expect("the synthetic DM record is never poisoned").clear();
  let key = root.path().join("key");
  fs::write(&key, "synthetic-backup-password\n").unwrap();
  let settings = ResticSettings { repository: root.path().join("repo").display().to_string(), password_file: key };
  let mut init = std::process::Command::new(charliebot_backup::backup::restic_program().unwrap());
  init.arg("--repo").arg(&settings.repository).arg("--password-file").arg(&settings.password_file).arg("init");
  assert!(init.status().unwrap().success(), "the local repository initializes");
  write_backup_config(
    &home,
    &format!("  repository: {}\n  password_file: {}", settings.repository, settings.password_file.display()),
  );
  let features = features_over(&home);
  let result = tokio::runtime::Runtime::new().unwrap().block_on(handler(Arc::clone(&features)));
  assert!(result.is_ok(), "{result:?}");
  assert!(DMS.lock().expect("the synthetic DM record is never poisoned").is_empty(), "a successful run sends no DM");
}
