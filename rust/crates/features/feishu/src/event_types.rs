//! Event wire names that the Feishu package writes. `charliebot_core::event_types` owns each name.

/// A reply the master posted to its session's Feishu thread through `charliebot feishu reply`.
pub const FEISHU_REPLY: &str = charliebot_core::event_types::FEISHU_REPLY;
