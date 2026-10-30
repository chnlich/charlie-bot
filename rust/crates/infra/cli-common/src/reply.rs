//! The reply verbs' shared body (`src/runtime/cli/common.py::post_reply`). The body resolves the session,
//! reads the reply's `--file` text, POSTs it to the platform's endpoint and prints the readback as one JSON line.

use serde_json::{Map, Value, json};

use crate::client;
use crate::exit::CliExit;
use crate::files;
use crate::json_out;
use crate::resolve;

/// `post_reply`: the body is the resolved `session_id` and the `text`, then `extra`'s keys when given.
pub fn post_reply(
  endpoint: &str,
  session: Option<&str>,
  file_arg: &str,
  extra: Option<Map<String, Value>>,
) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(session)?;
  let text = files::read_reply_text(file_arg)?;
  let mut payload = Map::new();
  payload.insert("session_id".to_string(), json!(session_id));
  payload.insert("text".to_string(), json!(text));
  if let Some(extra) = extra {
    payload.extend(extra);
  }
  let readback = client::post(endpoint, Value::Object(payload), None, &[])?;
  print!("{}", json_out::render(&readback, None));
  Ok(())
}
