//! The channel verbs' shared bodies: reply posts the reply text, read pages a thread, and check probes the bot's
//! setup. Each verb resolves the session it needs, POSTs to the channel's endpoint, and prints the readback as one
//! JSON line.

use clap::Args;
use serde_json::{Map, Value, json};

use crate::client;
use crate::exit::CliExit;
use crate::fields::SESSION_ID;
use crate::files;
use crate::json_out;
use crate::resolve;

/// The smallest page size a read accepts: the CLI flag's range and the internal read route's bound.
pub const READ_LIMIT_MIN: i64 = 1;
/// The largest page size a read accepts: the CLI flag's range and the internal read route's bound.
pub const READ_LIMIT_MAX: i64 = 100;
/// The page size a read takes when the caller names none: the CLI flag's default and the route's fallback.
pub const READ_LIMIT_DEFAULT: i64 = 50;

/// The reply verb's arguments: the file that holds the text, and the session.
#[derive(Args)]
pub struct ReplyArgs {
  /// File holding the reply text. `-` reads stdin.
  #[arg(long)]
  pub file: String,
  /// Session ID (optional). Taken from the CHARLIEBOT_SESSION_ID the server writes into the master environment.
  #[arg(long)]
  pub session: Option<String>,
}

/// The reply POST: the body is the resolved `session_id` and the `text`, then `extra`'s keys when given.
pub fn post_reply(
  endpoint: &str,
  session: Option<&str>,
  file_arg: &str,
  extra: Option<Map<String, Value>>,
) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(session)?;
  let text = files::read_reply_text(file_arg)?;
  let mut payload = Map::new();
  payload.insert(SESSION_ID.to_string(), json!(session_id));
  payload.insert("text".to_string(), json!(text));
  if let Some(extra) = extra {
    payload.extend(extra);
  }
  let readback = client::post(endpoint, Value::Object(payload), None, &[])?;
  print!("{}", json_out::render(&readback, None));
  Ok(())
}

/// The read of one thread: refuse a page size outside the bounds, then post the resolved `session_id`, the `limit`,
/// and `extra`'s keys, and print the readback.
pub fn post_read(
  endpoint: &str,
  session: Option<&str>,
  limit: i64,
  extra: Option<Map<String, Value>>,
) -> Result<(), CliExit> {
  if !(READ_LIMIT_MIN..=READ_LIMIT_MAX).contains(&limit) {
    return Err(CliExit::usage(format!("--limit must be between {READ_LIMIT_MIN} and {READ_LIMIT_MAX}, got: {limit}")));
  }
  let session_id = resolve::resolve_from_env(session)?;
  let mut payload = Map::new();
  payload.insert(SESSION_ID.to_string(), json!(session_id));
  payload.insert("limit".to_string(), json!(limit));
  if let Some(extra) = extra {
    payload.extend(extra);
  }
  let readback = client::post(endpoint, Value::Object(payload), None, &[])?;
  print!("{}", json_out::render(&readback, None));
  Ok(())
}

/// The check of one bot's setup: post an empty body, print the readback, and exit 1 when `ok` is false.
pub fn post_check(endpoint: &str) -> Result<(), CliExit> {
  let result = client::post(endpoint, json!({}), None, &[])?;
  print!("{}", json_out::render(&result, None));
  if result.get("ok").and_then(Value::as_bool) != Some(true) {
    std::process::exit(1);
  }
  Ok(())
}
