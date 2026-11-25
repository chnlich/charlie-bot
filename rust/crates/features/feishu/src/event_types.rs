//! Event wire names that the Feishu package writes. `charliebot_core::event_types` owns each name.

/// A reply the master posted to its session's Feishu thread through `charliebot feishu reply`.
pub const FEISHU_REPLY: &str = charliebot_core::event_types::FEISHU_REPLY;
/// The audit record of a Feishu thread ack, which advances the session's read watermark over the messages it names.
pub const FEISHU_ACK: &str = charliebot_core::event_types::FEISHU_ACK;
