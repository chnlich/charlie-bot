//! Event wire names that the Discord package writes. `charliebot_core::event_types` owns each name.

/// A reply the master posted to its session's Discord thread through `charliebot discord reply`.
pub const DISCORD_REPLY: &str = charliebot_core::event_types::DISCORD_REPLY;
/// The audit record of a Discord thread ack, which advances the session's read watermark over the messages it names.
pub const DISCORD_ACK: &str = charliebot_core::event_types::DISCORD_ACK;
