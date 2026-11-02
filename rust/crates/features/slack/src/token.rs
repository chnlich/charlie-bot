//! The Slack Web API tokens in the profile's `credentials.yaml`, and which methods each kind may call.

use std::path::Path;

use charliebot_cli_common::exit::CliExit;
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

/// The Slack Web API methods the user token may call through the CLI: the six read methods agents
/// use today. The user token is read-only in this CLI; a message in the user's own name goes through
/// `charliebot outbound draft` and a bot post through `--token bot`.
pub const USER_READ_METHODS: [&str; 6] = [
  "auth.test",
  "users.list",
  "conversations.list",
  "conversations.history",
  "conversations.replies",
  "search.messages",
];

/// Admit one `slack request` call. The user token may only address one read method at a Slack Web API
/// method URL; the bot token keeps every method. The refusal returns before any network call.
pub fn admit(kind: TokenKind, url: &str) -> Result<(), CliExit> {
  if kind == TokenKind::Bot {
    return Ok(());
  }
  let refused = |subject: String| {
    CliExit::error(format!(
      "{subject}: the user token is read-only in this CLI. For a message in the user's own name use `charliebot outbound draft`; for a bot post use `--token bot`."
    ))
  };
  let method = slack_api_method(url).ok_or_else(|| refused(format!("{url} is not a Slack Web API method URL")))?;
  if !USER_READ_METHODS.contains(&method.as_str()) {
    return Err(refused(format!("{method} is not a Slack read method")));
  }
  Ok(())
}

/// The Slack Web API method a request URL addresses: the path segment after `/api/`.
fn slack_api_method(url: &str) -> Option<String> {
  let parsed = reqwest::Url::parse(url).ok()?;
  let segments: Vec<&str> = parsed.path().split('/').filter(|segment| !segment.is_empty()).collect();
  match segments.as_slice() {
    ["api", method] => Some((*method).to_string()),
    _ => None,
  }
}
