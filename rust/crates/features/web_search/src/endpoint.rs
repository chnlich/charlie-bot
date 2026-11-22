//! The MCP endpoint's state, which each request reads.

use std::sync::Arc;

use charliebot_compat::http_client;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::credentials::Credentials;
use charliebot_core::hot_reload::HotReload;

/// The endpoint's state: the config whose `backends.calls.web_search` option names the search, the profile's
/// credentials and the outbound client.
///
/// The client is built on the first search, as the Anthropic proxy builds its own, so a bad TLS setting fails that
/// search rather than the server's start. A test supplies its own client.
#[derive(Clone)]
pub struct WebSearch {
  pub(crate) config: Arc<HotReload<CharlieBotConfig>>,
  pub(crate) credentials: Arc<HotReload<Credentials>>,
  client: Option<reqwest::Client>,
}

impl WebSearch {
  pub(crate) fn new(config: Arc<HotReload<CharlieBotConfig>>, credentials: Arc<HotReload<Credentials>>) -> Self {
    Self { config, credentials, client: None }
  }

  #[cfg(test)]
  pub(crate) fn with_client(mut self, client: reqwest::Client) -> Self {
    self.client = Some(client);
    self
  }

  pub(crate) fn client(&self) -> Result<reqwest::Client, String> {
    match &self.client {
      Some(client) => Ok(client.clone()),
      None => http_client::shared_client().map_err(|error| format!("the web search HTTP client failed: {error}")),
    }
  }
}
