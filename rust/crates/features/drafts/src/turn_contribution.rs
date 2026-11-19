//! The drafts package's chat renderers: the approval card and the two outcome lines.

use charliebot_core::event_types;
use charliebot_registry::turn::{EventRenderer, TurnContribution};

use crate::card;

/// The drafts turn contribution. It adds no instruction segment and renders the three draft events.
pub struct DraftsTurnContribution;

impl TurnContribution for DraftsTurnContribution {
  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![
      (event_types::DRAFT_REGISTERED, card::draft_registered_message),
      (event_types::DRAFT_SENT, card::draft_sent_message),
      (event_types::DRAFT_FAILED, card::draft_failed_message),
    ]
  }
}
