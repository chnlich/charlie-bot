//! What every request handler reads: the profile's live config and credentials, the build identity,
//! the template engine and the static mounts.

use std::path::Path;
use std::sync::Arc;

use charliebot_core::config::{self, CharlieBotConfig, ConfigError};
use charliebot_core::credentials::{self, Credentials};
use charliebot_core::hot_reload::HotReload;

use crate::buildinfo::BuildInfo;
use crate::mimetypes::MimeTypes;
use crate::static_files::StaticMount;
use crate::templating::Templating;

pub struct AppState {
  /// `config.yaml`, reloaded when the file moves (`config.get_config`).
  pub config: HotReload<CharlieBotConfig>,
  /// `credentials.yaml`, reloaded when the file moves (`credentials.get_credentials`).
  pub credentials: HotReload<Credentials>,
  pub build_info: BuildInfo,
  pub templating: Templating,
  /// The `/static` mounts in the Python app's route order.
  pub static_mounts: Vec<StaticMount>,
  pub mime_types: MimeTypes,
}

/// The hot-reloaded `config.yaml` of `home`.
pub fn config_cache(home: &Path) -> HotReload<CharlieBotConfig> {
  let home = home.to_path_buf();
  HotReload::new(
    home.join(config::CONFIG_FILENAME),
    "config_reload_failed",
    Box::new(move || config::load_config(&home)),
  )
}

/// The hot-reloaded `credentials.yaml` of `home`.
pub fn credentials_cache(home: &Path) -> HotReload<Credentials> {
  let home = home.to_path_buf();
  HotReload::new(
    home.join(config::CREDENTIALS_FILENAME),
    "credentials_reload_failed",
    Box::new(move || credentials::load_credentials(&home)),
  )
}

impl AppState {
  /// The current config.
  pub fn config(&self) -> Result<Arc<CharlieBotConfig>, ConfigError> {
    self.config.get()
  }

  /// The current credentials.
  pub fn credentials(&self) -> Result<Arc<Credentials>, ConfigError> {
    self.credentials.get()
  }

  /// The configured access key, or `None` when access-key auth is off (`configured_access_key`).
  pub fn access_key(&self) -> Result<Option<String>, ConfigError> {
    Ok(self.credentials.get()?.access_key())
  }
}
