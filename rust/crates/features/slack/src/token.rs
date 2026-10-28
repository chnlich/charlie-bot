//! The Slack Web API tokens in the profile's `credentials.yaml`.

use std::path::Path;

use charliebot_core::config::ConfigError;
use charliebot_core::credentials;

/// Which Slack token a request authenticates with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
  /// `slack.user_token`: the user's own OAuth token, for one-off reads.
  User,
  /// `slack.bot_token`: the app's token, for posting.
  Bot,
}

impl TokenKind {
  /// The `slack` section key this kind reads.
  pub fn key(self) -> &'static str {
    match self {
      TokenKind::User => "user_token",
      TokenKind::Bot => "bot_token",
    }
  }
}

/// The named token from `<home>/credentials.yaml`.
pub fn token(home: &Path, kind: TokenKind) -> Result<String, ConfigError> {
  let credentials = credentials::load_credentials(home)?;
  credentials.require("slack", kind.key())
}
