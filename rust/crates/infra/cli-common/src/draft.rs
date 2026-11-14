//! The draft commands' shared registration call: the session, the platform target, the body, and the optional draft
//! this one replaces, posted to the server's internal draft route.

use serde_json::{Value, json};

use crate::exit::CliExit;
use crate::fields::SESSION_ID;
use crate::{client, json_out, resolve};

/// The internal route that registers a draft.
pub const REGISTER_PATH: &str = "/api/internal/drafts";

/// The registration payload's field names that belong to the draft alone. The session field comes from `crate::fields`.
/// The `charliebot-drafts` package's register route declares the same names, and its draft events carry them.
pub const PLATFORM: &str = "platform";
pub const TARGET: &str = "target";
pub const BODY: &str = "body";
pub const REPLACES: &str = "replaces";

/// The registration of one draft for the current session, with the readback (`draft_id` and `card`) printed.
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
