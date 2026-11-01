//! The charlie-code backend factory: the option's model and endpoint fields, and the credential's `api_key`.

use anyhow::Context;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::credentials;
use charliebot_registry::backend::{AgentBackend, BackendFactory, LaunchArgs};

use super::charlie_code_cli::CharlieCodeBackend;
use super::options::CharlieCodeOption;
use charliebot_agents::cli_base::CliHost;

/// Builds one charlie-code backend per launch, resolving the binary from the package's host.
pub struct CharlieCodeFactory {
  host: CliHost,
}

impl CharlieCodeFactory {
  pub fn new(host: CliHost) -> Self {
    Self { host }
  }
}

impl BackendFactory for CharlieCodeFactory {
  fn build(
    &self,
    option: Option<&BackendOption>,
    cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let option = option.context("the charlie-code backend type needs its backends.options entry")?;
    let model = option.required_model("backend").map_err(anyhow::Error::msg)?;
    let fields = CharlieCodeOption::from_fields(&option.type_fields).map_err(anyhow::Error::msg)?;
    let api_key = match fields.credential.as_deref().filter(|section| !section.is_empty()) {
      Some(section) => Some(credentials::load_credentials(&cfg.charliebot_home)?.require_api_key(section)?),
      None => None,
    };
    let backend = CharlieCodeBackend::from_launch(&self.host, &model, fields, api_key, launch)?;
    Ok(Box::new(backend))
  }
}

#[cfg(test)]
mod tests {
  use charliebot_core::config::CREDENTIALS_FILENAME;
  use charliebot_registry::Registry;

  use super::*;

  fn option(fields: &str) -> BackendOption {
    BackendOption {
      id: "clc".to_string(),
      label: "CLC".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      backend_type: "charlie-code".to_string(),
      type_fields: serde_yaml::from_str(fields).unwrap(),
    }
  }

  #[test]
  fn an_empty_credential_key_is_refused() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(CREDENTIALS_FILENAME), "endpoint:\n  api_key: \"\"\n").unwrap();
    let cfg = Registry::new().load_config(home.path()).unwrap();
    let host = CliHost { path: String::new(), home: home.path().to_path_buf() };
    let error = CharlieCodeFactory::new(host)
      .build(Some(&option("api_base: http://test.invalid/v1\ncredential: endpoint\n")), &cfg, LaunchArgs::default())
      .err()
      .map(|error| format!("{error:#}"))
      .expect("an empty key is refused");
    assert!(error.contains("credentials.endpoint.api_key is empty"), "{error}");
  }
}
