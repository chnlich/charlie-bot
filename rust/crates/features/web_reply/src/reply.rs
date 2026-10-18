//! The web session test and the reply rules file (`src/features/web_reply/reply.py`). The thread test is chat_threads'
//! `thread_sessions.is_thread_session`, restated over the same fields: this package does not depend on chat_threads.

use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use serde_json::Value;

/// The web reply rules file under `prompts/` (`reply.RULES_FILE`).
pub const RULES_FILE: &str = "web_reply.md";

/// True for a manager session that no Slack or Discord thread owns (`reply.is_web_session`).
pub fn is_web_session(meta: &SessionMetadata) -> bool {
  meta.profile == TaskProfile::Manager
    && meta.extra.get("slack_origin").is_none_or(Value::is_null)
    && meta.extra.get("discord_origin").is_none_or(Value::is_null)
}
