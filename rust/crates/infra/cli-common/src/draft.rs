//! The draft commands' shared registration call: the session, the platform target, the body, and the optional draft
//! this one replaces, posted to the server's internal draft route.

use serde_json::{Value, json};

use crate::exit::CliExit;
use crate::{client, json_out, resolve};

/// The internal route that registers a draft.
pub const REGISTER_PATH: &str = "/api/internal/drafts";

/// The registration payload's field names, which this module owns because it builds the payload. The
/// `charliebot-drafts` package's register route declares the same names, and its draft events carry them.
pub const SESSION_ID: &str = "session_id";
pub const PLATFORM: &str = "platform";
pub const TARGET: &str = "target";
pub const BODY: &str = "body";
pub const REPLACES: &str = "replaces";

/// Register one draft for the current session and print the readback (`draft_id` and `card`).
///
/// The caller validates the target and reads the body first: a failed validation never reaches this call, so an
/// unparseable target registers nothing.
pub fn register(platform: &str, target: Value, body: String, replaces: Option<&str>) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(None)?;
  let payload = json!({
    (SESSION_ID): session_id,
    (PLATFORM): platform,
    (TARGET): target,
    (BODY): body,
    (REPLACES): replaces,
  });
  let readback = client::post(REGISTER_PATH, payload, None, &[])?;
  print!("{}", json_out::render(&readback, None));
  Ok(())
}
