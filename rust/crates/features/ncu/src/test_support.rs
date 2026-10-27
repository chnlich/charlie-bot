//! Fixtures for the viewer's tests: a synthetic Nsight install, report files, and a feature context whose templates are
//! the checkout's own `ncu.html`.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::Registry;
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::templating::Templating;
use charliebot_store::Store;

/// The checkout that holds this crate, three directories above `rust/`.
pub fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..").canonicalize().unwrap()
}

/// This crate's fake Nsight install: `bin/ncu` and `extras/python/ncu_report.py`.
pub fn fake_install() -> PathBuf {
  repo_root().join("rust/crates/features/ncu/tests/fixtures/nsight")
}

/// A synthetic install under a temporary directory, with an `ncu` that prints the two-kernel CSV.
pub struct Install {
  root: tempfile::TempDir,
}

impl Install {
  /// An install with no `extras/python`: the CSV path is the only one it offers.
  pub fn without_module() -> Install {
    let install = Install { root: tempfile::tempdir().unwrap() };
    install.write_ncu();
    install
  }

  /// An install whose `ncu_report.py` is `module`.
  pub fn with_module(module: &str) -> Install {
    let install = Install::without_module();
    let python = install.root.path().join("extras").join("python");
    fs::create_dir_all(&python).unwrap();
    fs::write(python.join("ncu_report.py"), module).unwrap();
    install
  }

  /// An install whose module appends a line to `imports.log` in the install root each time Python imports it, then
  /// fails the import.
  pub fn with_failing_module() -> Install {
    Install::with_module(
      "import pathlib\n\
       with pathlib.Path(__file__).parents[2].joinpath('imports.log').open('a') as log:\n\
       \x20 log.write('import\\n')\n\
       raise ImportError('synthetic ncu_report import failure')\n",
    )
  }

  pub fn dir(&self) -> &Path {
    self.root.path()
  }

  /// The search path that finds this install's `ncu`, with the system directories for `cat` and `sh`.
  pub fn path_env(&self) -> OsString {
    OsString::from(format!("{}:/usr/bin:/bin", self.root.path().join("bin").display()))
  }

  /// The number of times Python imported the module.
  pub fn imports(&self) -> usize {
    fs::read_to_string(self.root.path().join("imports.log")).map_or(0, |log| log.lines().count())
  }

  fn write_ncu(&self) {
    let bin = self.root.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let ncu = bin.join("ncu");
    let csv = repo_root().join("rust/crates/features/ncu/tests/canned/two_kernels.csv");
    fs::write(&ncu, format!("#!/bin/sh\ncat '{}'\n", csv.display())).unwrap();
    fs::set_permissions(&ncu, fs::Permissions::from_mode(0o755)).unwrap();
  }
}

/// Write a report file named `name` under `dir` and return its absolute path.
pub fn write_report(dir: &Path, name: &str, contents: &str) -> String {
  fs::create_dir_all(dir).unwrap();
  let path = dir.join(name);
  fs::write(&path, contents).unwrap();
  path.display().to_string()
}

/// A report the fake module loads: one kernel with a duration, and the CSV the fake `ncu` prints.
pub fn module_report(kernel: &str) -> String {
  serde_json::json!({
    "kernels": [{
      "name": kernel,
      "metrics": [{"name": "gpu__time_duration.sum", "value": 1500, "unit": "nsecond"}],
      "rules": [],
      "sass": [],
      "ptx": [],
      "source_files": {},
    }],
    "csv": "",
  })
  .to_string()
}

/// A feature context whose `RepoRoot` is `repo` and whose page templates are `template_root`'s. Its config and store
/// live in a temporary home.
pub fn feature_context(repo: &Path, template_root: &Path) -> (tempfile::TempDir, Arc<FeatureContext>) {
  let home = tempfile::tempdir().unwrap();
  let registry = Arc::new(Registry::new());
  let config_home = home.path().to_path_buf();
  let config_registry = Arc::clone(&registry);
  let config = Arc::new(HotReload::new(
    home.path().join(CONFIG_FILENAME),
    "ncu_test_config_reload_failed",
    Box::new(move || config_registry.load_config(&config_home)),
  ));
  let credentials_home = home.path().to_path_buf();
  let credentials = Arc::new(HotReload::new(
    home.path().join(CREDENTIALS_FILENAME),
    "ncu_test_credentials_reload_failed",
    Box::new(move || load_credentials(&credentials_home)),
  ));
  let cfg: CharlieBotConfig = registry.load_config(home.path()).unwrap();
  let store = Store::new(cfg, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).unwrap();
  let startup = StartupState::default();
  startup.insert(RepoRoot::new(Some(OsString::from(repo))).unwrap()).unwrap();
  startup.insert(Templating::new(template_root, &["src.features.ncu"])).unwrap();
  let features = FeatureContext { config, credentials, store: Arc::new(store), registry, startup: Arc::new(startup) };
  (home, Arc::new(features))
}
