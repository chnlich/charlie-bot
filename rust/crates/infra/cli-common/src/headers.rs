//! The `Authorization` and caller-session headers on internal-API calls.

use charliebot_core::credentials;
use charliebot_core::home;
use charliebot_core::run_token::{RUN_TOKEN_ENV, SESSION_ID_ENV};

use crate::exit::CliExit;

/// `constants.CALLER_SESSION_HEADER`: the calling session on internal-API calls. One spelling everywhere.
pub const CALLER_SESSION_HEADER: &str = "X-CharlieBot-Caller-Session";

/// The `Authorization` and caller-session headers for one internal-API call.
///
/// A run token in the environment (`CHARLIEBOT_RUN_TOKEN`) always wins and is the only credential
/// sent. A supported CLI request running under a Run never falls back to the operator access key.
/// A rejected run token therefore surfaces as the server's 401 instead of silently upgrading to
/// operator identity. Without a run token the operator access key is used: `credentials.yaml`
/// under `charliebot.access_key`. With neither, no header is sent, because the middleware is a
/// no-op for an empty key. When `CHARLIEBOT_SESSION_ID` is non-empty the returned headers also
/// carry `X-CharlieBot-Caller-Session` with that value. The server records that header on operator
/// caller identities only: an agent's session comes from its verified token, never the header.
pub fn internal_api_auth_headers() -> Result<Vec<(String, String)>, CliExit> {
  let token = std::env::var(RUN_TOKEN_ENV).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
  let bearer = match token {
    Some(token) => Some(token),
    None => access_key()?,
  };
  match bearer {
    Some(bearer) => {
      let mut headers = vec![("Authorization".to_string(), format!("Bearer {bearer}"))];
      if let Some(session_id) = caller_session() {
        headers.push((CALLER_SESSION_HEADER.to_string(), session_id));
      }
      Ok(headers)
    }
    None => Ok(Vec::new()),
  }
}

/// The caller-session header's value: the environment variable when it is non-empty.
///
/// The value comes from the environment variable only: it names the process the server started,
/// while the cwd only names a directory.
fn caller_session() -> Option<String> {
  std::env::var(SESSION_ID_ENV).ok().filter(|value| !value.is_empty())
}

/// `configured_access_key()`: the `charliebot.access_key` credential, or `None` when unset. A
/// credentials file that does not load is an error, as Python's `get_credentials()` raises it.
fn access_key() -> Result<Option<String>, CliExit> {
  let home = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let credentials = credentials::load_credentials(&home).map_err(|error| CliExit::error(error.0))?;
  Ok(credentials.access_key())
}
