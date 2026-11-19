//! The one refusal for a credential a `charliebot` verb cannot read. A verb that answers a missing credential this
//! way formats it here, so the user reads one message whichever verb ran.

use std::path::Path;

use charliebot_core::config::CREDENTIALS_FILENAME;
use charliebot_core::{home, paths};
use serde_json::json;

use crate::exit::CliExit;

/// The failure for a credential the CLI could not read: `{"error": "credentials.<section>.<key> is unavailable in
/// <path>"}`, exit 3.
///
/// `path` is the credentials file the read used. `None` names the filename the default home would hold, for a read
/// that failed before any home resolved.
pub fn unavailable(section: &str, key: &str, path: Option<&Path>) -> CliExit {
  let where_text = match path {
    Some(path) => paths::display(path),
    None => home::display_path(CREDENTIALS_FILENAME),
  };
  CliExit::contract(json!({"error": format!("credentials.{section}.{key} is unavailable in {where_text}")}), 3)
}
