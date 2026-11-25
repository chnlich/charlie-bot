//! Event wire names that the Slack package writes. `charliebot_core::event_types` owns each name.

/// A reply the master posted to its session's Slack thread through `charliebot slack reply`.
pub const SLACK_REPLY: &str = charliebot_core::event_types::SLACK_REPLY;
