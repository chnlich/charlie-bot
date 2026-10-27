//! What every request handler reads: the feature context (the profile's live config and credentials, the store, the
//! registry, the startup state with the template engine), the build identity and the static mounts.

use std::path::Path;
use std::sync::Arc;

use charliebot_compat::mimetypes::MimeTypes;
use charliebot_compat::static_files::StaticDirectory;
use charliebot_core::config::{self, CharlieBotConfig, ConfigError};
use charliebot_core::credentials::{self, Credentials};
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::pages::package_directory;
use charliebot_registry::{FeatureContext, Registry};

use crate::buildinfo::BuildInfo;

/// One `app.mount(prefix, StaticFiles(directory=...))`.
pub struct StaticMount {
  /// The URL prefix without a trailing slash: `/static/features/voice` or `/static`.
  pub prefix: String,
  pub directory: StaticDirectory,
}

impl StaticMount {
  /// The Python app's `/static` mounts in its route order: one per page package in registration order, then
  /// `web/static` when it exists at startup.
  pub fn python_app_mounts(repo_root: &Path, packages: &[&str]) -> Vec<StaticMount> {
    let mut mounts: Vec<StaticMount> = packages
      .iter()
      .map(|package| StaticMount {
        prefix: format!("/static/{}", package.trim_start_matches("src.").replace('.', "/")),
        directory: StaticDirectory::new(package_directory(repo_root, package).join("static")),
      })
      .collect();
    let web_static = repo_root.join("web").join("static");
    if web_static.exists() {
      mounts.push(StaticMount { prefix: "/static".to_string(), directory: StaticDirectory::new(web_static) });
    }
    mounts
  }

  /// The path below this mount when `path` falls under it (starlette `Mount` matching), else `None`.
  pub fn route_path<'a>(&self, path: &'a str) -> Option<&'a str> {
    path.strip_prefix(&self.prefix).filter(|rest| rest.starts_with('/'))
  }
}

pub struct AppState {
  /// The handles the registered features share: the live config and credentials, the store, the registry.
  pub features: Arc<FeatureContext>,
  pub build_info: BuildInfo,
  /// The `/static` mounts in the Python app's route order.
  pub static_mounts: Vec<StaticMount>,
  pub mime_types: MimeTypes,
}

/// The hot-reloaded `config.yaml` of `home`, loaded with the registry's config schema and checks.
pub fn config_cache(home: &Path, registry: Arc<Registry>) -> Arc<HotReload<CharlieBotConfig>> {
  let home = home.to_path_buf();
  Arc::new(HotReload::new(
    home.join(config::CONFIG_FILENAME),
    "config_reload_failed",
    Box::new(move || registry.load_config(&home)),
  ))
}

/// The hot-reloaded `credentials.yaml` of `home`.
pub fn credentials_cache(home: &Path) -> Arc<HotReload<Credentials>> {
  let home = home.to_path_buf();
  Arc::new(HotReload::new(
    home.join(config::CREDENTIALS_FILENAME),
    "credentials_reload_failed",
    Box::new(move || credentials::load_credentials(&home)),
  ))
}

impl AppState {
  /// The current config.
  pub fn config(&self) -> Result<Arc<CharlieBotConfig>, ConfigError> {
    self.features.config.get()
  }

  /// The current credentials.
  pub fn credentials(&self) -> Result<Arc<Credentials>, ConfigError> {
    self.features.credentials.get()
  }

  /// The configured access key, or `None` when access-key auth is off (`configured_access_key`).
  pub fn access_key(&self) -> Result<Option<String>, ConfigError> {
    Ok(self.features.credentials.get()?.access_key())
  }
}
