//! The Feishu fields of a session.

use charliebot_registry::thread_origin;
use charliebot_store::metadata::SessionMetadata;
use serde::{Deserialize, Serialize};

use crate::fields::{CHAT_ID, ROOT_ID, TENANT_KEY, THREAD_ID};

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
  thread_origin::read_origin(meta, FEISHU_ORIGIN, "a Feishu origin")
}

/// The `feishu_origin` slot's required fields: `tenant_key`, `chat_id` and `root_id`.
pub const ORIGIN_REQUIRED_FIELDS: [&str; 3] = [TENANT_KEY, CHAT_ID, ROOT_ID];

/// The `feishu_origin` slot's optional field: `thread_id`, which is absent until the bot's first reply creates the
/// thread.
pub const ORIGIN_OPTIONAL_FIELDS: [&str; 1] = [THREAD_ID];
