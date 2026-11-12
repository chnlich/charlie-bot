//! The Codex token-usage JSON field names.
//!
//! A `turn.completed` event carries a `usage` object, and a rollout's `token_count` event carries a `last_token_usage`
//! object and a cumulative `total_token_usage` object. All three objects name the same token counts, and the
//! translator, the rollout reader, the usage-log reader, the quota account and the pricing table read them. One module
//! owns the names, so a change reaches every reader.
//!
//! The prompt and completion counts are `charliebot_core::event_types` names, which the Claude usage reader spells from
//! there too; only the names Codex alone uses are defined here.

/// The prompt and completion counts of a usage object, under the names their owner defines.
pub(crate) use charliebot_core::event_types::{
  USAGE_INPUT_TOKENS as INPUT_TOKENS, USAGE_OUTPUT_TOKENS as OUTPUT_TOKENS,
};

/// The share of the input tokens that the provider served from its cache.
pub(crate) const CACHED_INPUT_TOKENS: &str = "cached_input_tokens";

/// The usage object of the latest request in a rollout's `token_count` event.
pub(crate) const LAST_TOKEN_USAGE: &str = "last_token_usage";

/// The cumulative usage of a rollout's session.
pub(crate) const TOTAL_TOKEN_USAGE: &str = "total_token_usage";
