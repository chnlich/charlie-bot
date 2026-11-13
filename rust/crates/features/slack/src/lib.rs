//! `web/features/slack`: the Socket Mode summon listener, thread reply routes, service, session fields, and turn hooks.

mod api;
pub mod cli;
mod config;
mod download;
mod draft;
mod draft_target;
mod event_types;
pub mod metadata;
mod service;
mod slack_client;
pub mod slack_listener;
pub mod thread_contract;
pub mod token;
mod turn_contribution;

use std::sync::Arc;

use anyhow::Context;
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField, ThreadSlot, validate_thread_slot};
use charliebot_registry::{FeatureContext, Registry, RegistryError};
use charliebot_store::metadata::SUCCESSOR_SESSION_ID;
use futures_util::future::BoxFuture;
use service::SlackService;
use turn_contribution::SlackTurnContribution;

/// The thread fields of a Slack session's metadata slot.
const THREAD_SLOT: ThreadSlot = ThreadSlot {
  origin_field: "slack_origin",
  origin_type: "SlackOrigin",
  origin_keys: &["team_id", "channel_id", "thread_ts"],
  watermark_field: "slack_watermark_ts",
};

/// `register`: the service, the routes and the package's other registrations, in server order.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.service(Arc::new(SlackService::default()))?;
  registry.router(api::routes, api::matches)?;
  registry.draft_platform("slack", draft::platform_factory())?;
  registry.config_section(config::section())?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: "slack",
      on: MetadataFile::Session,
      after: Some(SUCCESSOR_SESSION_ID),
      fields: [THREAD_SLOT.origin_field, THREAD_SLOT.watermark_field]
        .into_iter()
        .map(|name| SlotField { name, default: serde_json::Value::Null })
        .collect(),
    },
    |values| validate_thread_slot(values, &THREAD_SLOT),
  )?;
  registry.thread_origin_field(THREAD_SLOT.origin_field)?;
  registry.turn_contribution("slack", Arc::new(SlackTurnContribution))?;
  registry.user_dm(post_user_dm)?;
  registry.owner_dm(post_owner_dm)
}

fn post_user_dm<'a>(
  features: &'a FeatureContext,
  user: &'a str,
  text: &'a str,
) -> BoxFuture<'a, anyhow::Result<String>> {
  Box::pin(async move {
    let client = slack_client::SlackClient::from_features(features)?;
    client.post_dm(user, text).await
  })
}

/// The host owner is the first id in `slack.allowed_user_ids`; a host that serves several people lists only the owner
/// there.
fn post_owner_dm<'a>(features: &'a FeatureContext, text: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
  Box::pin(async move {
    let cfg = features.config.get()?;
    let owner = cfg
      .section::<config::SlackConfig>(config::NAME)
      .allowed_user_ids
      .first()
      .cloned()
      .context("the slack section names no allowed user to DM")?;
    let client = slack_client::SlackClient::from_features(features)?;
    client.post_dm(&owner, text).await
  })
}

#[cfg(test)]
mod test_support;
