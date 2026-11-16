//! The deterministic ids of a channel thread.
//!
//! A summon creates one session per channel thread, and a Feishu reply's post nonce derives the same way: a
//! version-5 UUID of the channel's fixed namespace and a key. The session directory carries the id, so a namespace
//! and a key scheme never change.

/// Slack's UUID namespace for its thread session ids. Changing it would orphan every existing Slack-backed session.
pub const SLACK_NAMESPACE: u128 = 0x1b4e28ba_2fa1_4d7a_9f0c_8d5e7a3b6c11;
/// Discord's UUID namespace for its thread session ids. Changing it would orphan every existing Discord-backed session.
pub const DISCORD_NAMESPACE: u128 = 0x13667dae_7551_489f_9cf1_d0819d0978e6;
/// Feishu's UUID namespace for its thread session ids. Changing it would orphan every existing Feishu-backed session.
pub const FEISHU_NAMESPACE: u128 = 0x5b1e0c2a_7f4d_4a7e_9d3b_6c8f1a2e4d70;

/// The text of the version-5 UUID that `namespace` and `key` derive.
pub fn namespaced_uuid(namespace: u128, key: &str) -> String {
  uuid::Uuid::new_v5(&uuid::Uuid::from_u128(namespace), key.as_bytes()).to_string()
}
