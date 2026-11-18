//! The persisted host-auth state and the process-wide slot that serializes its writes.

use std::fs;
use std::io;
use std::path::Path;

use charliebot_core::fsutil;
use charliebot_core::text_repr::{self, JsonStyle};
use serde_json::{Map, Value, json};
use tokio::sync::{Semaphore, SemaphorePermit};

/// The standing probe period. A held host is probed at its own backoff period.
pub const PROBE_INTERVAL_SEC: i64 = 1620;

/// The state file's object, with its keys in file order.
pub type State = Map<String, Value>;

/// The state file's layout: JSON with two-space indent, non-ASCII text literal and a trailing newline, written atomically.
const STATE_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ",", key_separator: ": ", indent: Some(2) };

/// `empty_state`: the state a fresh install serves.
pub fn empty_state() -> State {
  let Value::Object(state) = json!({
    "probed_at": null,
    "probe_running": false,
    "interval_sec": PROBE_INTERVAL_SEC,
    "hosts": [],
  }) else {
    unreachable!("an object literal builds an object");
  };
  state
}

/// `read_state`: the state file, or the empty state when it is absent. A file that is unreadable, not UTF-8 JSON, or
/// not a state object is an error. A reader that decides on the hosts' verdicts must not take such a file for no hosts.
pub fn read_state(path: &Path) -> anyhow::Result<State> {
  match fs::read(path) {
    Ok(bytes) => parse_state(&bytes).map_err(|error| anyhow::anyhow!("{}: {error}", path.display())),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(empty_state()),
    Err(error) => Err(anyhow::Error::from(error).context(format!("reading {}", path.display()))),
  }
}

/// `load_state`: the state the page and the poller serve. A state file that `read_state` refuses is logged and answered
/// with the empty state.
pub fn load_state(path: &Path) -> State {
  read_state(path).unwrap_or_else(|error| {
    tracing::warn!(path = %path.display(), error = %format!("{error:#}"), "host_auth_state_read_failed");
    empty_state()
  })
}

fn parse_state(bytes: &[u8]) -> Result<State, String> {
  let text = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
  match serde_json::from_str::<Value>(text).map_err(|error| error.to_string())? {
    Value::Object(state) if state.get("hosts").is_some_and(Value::is_array) => Ok(state),
    _ => Err("state file does not hold a host_auth state object".to_string()),
  }
}

/// `save_state`: publish the state through a temporary file and a rename. A failed write is logged and returned.
pub fn save_state(state: &State, path: &Path) -> anyhow::Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)?;
  }
  let text = format!("{}\n", text_repr::json_dumps(&Value::Object(state.clone()), &STATE_JSON));
  if let Err(error) = fsutil::atomic_write_text(path, &text) {
    tracing::error!(path = %path.display(), error = %error, "host_auth_state_write_failed");
    return Err(error.into());
  }
  Ok(())
}

/// The process-wide round slot: a manual round, the poller's tick and the cron gate's write-back never change the state
/// file together. The slot is released when the guard drops, on every path.
static ROUND_SLOT: Semaphore = Semaphore::const_new(1);

/// A held round slot. Dropping it releases the slot.
pub struct RoundSlot {
  _permit: SemaphorePermit<'static>,
}

impl RoundSlot {
  /// The slot, or `None` while a round holds it.
  pub fn claim() -> Option<RoundSlot> {
    ROUND_SLOT.try_acquire().ok().map(|permit| RoundSlot { _permit: permit })
  }

  /// The slot, once the round that holds it, if any, has released it.
  pub async fn wait() -> RoundSlot {
    RoundSlot { _permit: ROUND_SLOT.acquire().await.expect("the round slot is never closed") }
  }
}
