//! Turn contributions (`src/runtime/hooks/turn_contributions.py`): how a feature package takes part in a master
//! turn. A contribution answers each step, and the registry resolves the answers the way Python does: one
//! contribution names the workflow rules file, one gives the input preamble, and one renderer per event type.

use std::sync::Arc;

use anyhow::bail;
use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_store::metadata::SessionMetadata;
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::context::FeatureContext;
use crate::registry::{Registry, RegistryError};

/// The workflow rules file a session gets when no contribution names one (`DEFAULT_WORKFLOW_RULES_FILE`).
pub const DEFAULT_WORKFLOW_RULES_FILE: &str = "manager_workflows.md";

/// Where an instruction segment's text came from, as the runtime's prompt assembly records it (`PromptSource`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSource {
  pub scope: String,
  pub source_ref: String,
  pub source_session_id: Option<String>,
}

/// One ordered instruction unit (`task_prompts.RuleSegment`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSegment {
  pub text: String,
  pub sources: Vec<PromptSource>,
  pub delivery: String,
}

/// Renders one persisted event type for the chat: the event in, the message out.
pub type EventRenderer = fn(&Value) -> Value;

/// One package's part in a master turn. Every step defaults to adding nothing.
pub trait TurnContribution: Send + Sync {
  /// The Python class this contribution ports, named as `type(contribution).__name__` names it. The store names the
  /// contribution's after-turn task by it: `after-turn-<class name>-<session id>`.
  fn class_name(&self) -> &'static str;

  /// Segments for the managed instructions of a run of `kind`. They follow the model overlay segments and precede
  /// the local rule segments, in registration order. A contribution reads its prompt docs through `repo`.
  fn instruction_segments(
    &self,
    _meta: &SessionMetadata,
    _kind: &str,
    _cfg: &CharlieBotConfig,
    _repo: &RepoRoot,
  ) -> anyhow::Result<Vec<RuleSegment>> {
    Ok(Vec::new())
  }

  /// The workflow rules file for this session, or `None` to name none. At most one contribution answers `Some`.
  fn workflow_rules_file(&self, _meta: &SessionMetadata) -> Option<&'static str> {
    None
  }

  /// Runs when a turn's input is queued, before the user event is appended.
  fn before_turn<'a>(
    &'a self,
    _meta: &'a SessionMetadata,
    _cfg: &'a CharlieBotConfig,
    _features: &'a Arc<FeatureContext>,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  /// Runs as its own logged task after the `MASTER_DONE` event is appended and broadcast.
  fn after_turn<'a>(
    &'a self,
    _meta: &'a SessionMetadata,
    _done_event: &'a Value,
    _cfg: &'a CharlieBotConfig,
    _features: &'a Arc<FeatureContext>,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  /// The text that opens a user input in the turn prompt. At most one contribution answers `Some` for an event.
  fn input_preamble(&self, _meta: &SessionMetadata, _input_event: &Value) -> Option<String> {
    None
  }

  /// Chat renderers by event type. A type that two contributions render fails the registration.
  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    Vec::new()
  }

  /// True when this contribution marks the session unread itself after a clean turn, so the runtime skips its mark.
  fn claims_turn_end_unread(&self, _meta: &SessionMetadata) -> bool {
    false
  }
}

impl Registry {
  /// Register a turn contribution (`turn_contributions.register_turn_contribution`). A chat renderer type that an
  /// earlier contribution, or this one, already renders fails here: Python fails at the aggregator's build.
  pub fn turn_contribution(
    &mut self,
    name: &'static str,
    contribution: Arc<dyn TurnContribution>,
  ) -> Result<(), RegistryError> {
    let package = self.owner()?;
    let mut rendered: Vec<&'static str> = self
      .turn_contributions
      .values()
      .flat_map(|known| known.event_renderers().into_iter().map(|(event_type, _)| event_type))
      .collect();
    for (event_type, _) in contribution.event_renderers() {
      if rendered.contains(&event_type) {
        return Err(RegistryError {
          package,
          message: format!("event type {event_type:?} is rendered by more than one turn contribution"),
        });
      }
      rendered.push(event_type);
    }
    self.turn_contributions.insert(package, name, contribution)
  }

  /// The workflow rules file for `meta`: the one a contribution names, else [`DEFAULT_WORKFLOW_RULES_FILE`].
  /// Two contributions that name a file fail.
  pub fn workflow_rules_file(&self, meta: &SessionMetadata) -> anyhow::Result<&'static str> {
    let answers = self.turn_contributions.values().filter_map(|contribution| contribution.workflow_rules_file(meta));
    let named: Vec<&'static str> = answers.collect();
    match named.as_slice() {
      [] => Ok(DEFAULT_WORKFLOW_RULES_FILE),
      [file] => Ok(file),
      _ => bail!("workflow rules file: more than one turn contribution answers: {}", named.join(", ")),
    }
  }

  /// The instruction segments of every contribution, in registration order.
  pub fn instruction_segments(
    &self,
    meta: &SessionMetadata,
    kind: &str,
    cfg: &CharlieBotConfig,
    repo: &RepoRoot,
  ) -> anyhow::Result<Vec<RuleSegment>> {
    let mut segments = Vec::new();
    for contribution in self.turn_contributions.values() {
      segments.extend(contribution.instruction_segments(meta, kind, cfg, repo)?);
    }
    Ok(segments)
  }

  /// The input preamble that a contribution gives `input_event`, or `None`. Two contributions that give one fail.
  pub fn input_preamble(&self, meta: &SessionMetadata, input_event: &Value) -> anyhow::Result<Option<String>> {
    let answers: Vec<String> = self
      .turn_contributions
      .values()
      .filter_map(|contribution| contribution.input_preamble(meta, input_event))
      .collect();
    match answers.as_slice() {
      [] => Ok(None),
      [preamble] => Ok(Some(preamble.clone())),
      _ => bail!("input preamble: more than one turn contribution answers for one event"),
    }
  }

  /// True when a registered contribution marks the turn-end unread flag for `meta` itself.
  pub fn turn_end_unread_claimed(&self, meta: &SessionMetadata) -> bool {
    self.turn_contributions.values().any(|contribution| contribution.claims_turn_end_unread(meta))
  }

  /// The chat renderer of `event_type`, or `None` when no contribution renders it.
  pub fn event_renderer(&self, event_type: &str) -> Option<EventRenderer> {
    self
      .turn_contributions
      .values()
      .flat_map(|contribution| contribution.event_renderers())
      .find_map(|(known, renderer)| (known == event_type).then_some(renderer))
  }

  /// Run every contribution's `before_turn` in registration order.
  pub async fn before_turn(
    &self,
    meta: &SessionMetadata,
    cfg: &CharlieBotConfig,
    features: &Arc<FeatureContext>,
  ) -> anyhow::Result<()> {
    for contribution in self.turn_contributions.values() {
      contribution.before_turn(meta, cfg, features).await?;
    }
    Ok(())
  }
}
