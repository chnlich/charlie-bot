//! The MCP endpoint's mount name and the state each request reads.

use std::sync::Arc;

use charliebot_compat::http_client;
use charliebot_core::credentials::Credentials;
use charliebot_core::hot_reload::HotReload;

use crate::search;

/// The MCP server name the endpoint backends mount.
pub const MCP_SERVER_NAME: &str = "charliebot-web";

/// The endpoint's state: the profile's credentials, the Google base URL and the outbound client.
///
/// The client is built on the first search, as the Anthropic proxy builds its own, so a bad TLS setting fails that
/// search rather than the server's start. A test supplies its own client and base URL.
#[derive(Clone)]
pub struct WebSearch {
  pub(crate) credentials: Arc<HotReload<Credentials>>,
  pub(crate) endpoint: String,
  client: Option<reqwest::Client>,
}

impl WebSearch {
  pub(crate) fn new(credentials: Arc<HotReload<Credentials>>) -> Self {
    Self { credentials, endpoint: search::GEMINI_ENDPOINT.to_string(), client: None }
  }

  #[cfg(test)]
  pub(crate) fn against(mut self, endpoint: &str, client: reqwest::Client) -> Self {
    self.endpoint = endpoint.to_string();
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
