//! The Feishu fields of a session.

use anyhow::Context;
use charliebot_store::metadata::SessionMetadata;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The session-metadata field holding the Feishu thread a session was summoned from.
pub const FEISHU_ORIGIN: &str = "feishu_origin";
/// The session-metadata field holding the Feishu id the session's turn has read up to.
pub const FEISHU_WATERMARK_ID: &str = "feishu_watermark_id";

/// The Feishu thread a session was summoned from. `root_id` is set at creation and never changes. `thread_id` is
/// absent until the bot's first reply creates the thread.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FeishuOrigin {
  pub tenant_key: String,
  pub chat_id: String,
  pub root_id: String,
  #[serde(default)]
  pub thread_id: Option<String>,
}

/// The session's Feishu origin, or `None` when the field is absent or null
/// (`fields_of(meta, "feishu").feishu_origin`). A stored value that is not a Feishu origin fails.
pub fn feishu_origin(meta: &SessionMetadata) -> anyhow::Result<Option<FeishuOrigin>> {
  match meta.extra.get(FEISHU_ORIGIN) {
    None | Some(Value::Null) => Ok(None),
    Some(value) => FeishuOrigin::deserialize(value).map(Some).context("feishu_origin is not a Feishu origin"),
  }
}

/// Validate a `feishu_origin` value in place (the slot validator): null, or an object whose `tenant_key`, `chat_id`
/// and `root_id` are strings and whose `thread_id` is a string or absent. Other keys are dropped.
///
/// `valid_origin` in core requires every listed field, which cannot express the optional `thread_id`.
pub fn valid_origin(value: &mut Value) -> bool {
  if value.is_null() {
    return true;
  }
  let Some(object) = value.as_object_mut() else { return false };
  let required = ["tenant_key", "chat_id", "root_id"];
  let valid = required.iter().all(|name| object.get(*name).is_some_and(Value::is_string))
    && object.get("thread_id").is_none_or(|thread_id| thread_id.is_null() || thread_id.is_string());
  if valid {
    object.retain(|name, _| required.contains(&name.as_str()) || name == "thread_id");
  }
  valid
}
