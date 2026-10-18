//! `src/features/web_reply`: registers the turn contribution that carries the reply rules into a web session's manager
//! runs and renders the reply chat lines, and the web-reply status field of each session. The rest of the package is
//! not ported yet.

mod reply;
mod turn_contribution;

use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};
use turn_contribution::WebReplyTurnContribution;

/// `src/features/web_reply`: the turn contribution, then the web-reply status of each session.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.turn_contribution("web_reply", Arc::new(WebReplyTurnContribution))?;
  registry.metadata_slot(MetadataSlot {
    owner: "web_reply",
    on: MetadataFile::Session,
    after: None,
    fields: vec![SlotField { name: "web_status", default: serde_json::Value::Null }],
  })
}
