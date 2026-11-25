//! Event wire names that the Slack package writes. `charliebot_core::event_types` owns each name.

/// A reply the master posted to its session's Slack thread through `charliebot slack reply`.
pub const SLACK_REPLY: &str = charliebot_core::event_types::SLACK_REPLY;
/// The audit record of a Slack thread ack, which advances the session's read watermark over the messages it names.
pub const SLACK_ACK: &str = charliebot_core::event_types::SLACK_ACK;
