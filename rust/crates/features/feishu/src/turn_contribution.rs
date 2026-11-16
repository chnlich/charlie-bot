//! The Feishu package's round-end audit, thread reply contract, and `feishu_reply` chat renderer.

use std::sync::Arc;

use charliebot_chat_threads::reply_event::reply_line;
use charliebot_chat_threads::thread_entry::{thread_reply_contract, thread_reply_segment};
use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::run_kind::RunKind;
use charliebot_registry::FeatureContext;
use charliebot_registry::turn::{EventRenderer, RuleSegment, TurnContribution};
use charliebot_store::metadata::SessionMetadata;
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::adapter;
use crate::event_types::FEISHU_REPLY;
use crate::metadata::feishu_origin;
use crate::thread_contract::{FEISHU, thread_contract_part};

/// The Feishu turn contribution.
pub struct FeishuTurnContribution;

impl TurnContribution for FeishuTurnContribution {
  fn class_name(&self) -> &'static str {
    "FeishuTurnContribution"
  }

  fn instruction_segments(
    &self,
    meta: &SessionMetadata,
    kind: &str,
    cfg: &CharlieBotConfig,
    repo: &RepoRoot,
  ) -> anyhow::Result<Vec<RuleSegment>> {
    if kind != RunKind::ManagerTurn.as_str() || feishu_origin(meta)?.is_none() {
      return Ok(Vec::new());
    }
    let contract = thread_reply_contract(repo, &cfg.charliebot_home, &FEISHU, &thread_contract_part())?;
    Ok(vec![thread_reply_segment(contract)])
  }

  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![(FEISHU_REPLY, feishu_reply_message)]
  }

  fn after_turn<'a>(
    &'a self,
    meta: &'a SessionMetadata,
    done_event: &'a Value,
    _cfg: &'a CharlieBotConfig,
    features: &'a Arc<FeatureContext>,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      adapter::deliver_done(features, &meta.id, done_event).await?;
      Ok(())
    })
  }
}

/// The chat line of a `feishu_reply` event.
fn feishu_reply_message(event: &Value) -> Value {
  reply_line(&FEISHU, event)
}

#[cfg(test)]
mod tests {
  use std::ffi::OsString;
  use std::fs;

  use charliebot_core::test_support::synthetic_config;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use serde_json::json;
  use tempfile::TempDir;

  use super::*;

  /// A synthetic checkout whose contract docs hold short marker texts, and which holds `docs` only.
  fn checkout(docs: &[(&str, &str)]) -> (TempDir, RepoRoot) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("prompts")).unwrap();
    for (name, text) in docs {
      fs::write(dir.path().join("prompts").join(name), text).unwrap();
    }
    let repo = RepoRoot::new(Some(OsString::from(dir.path()))).unwrap();
    (dir, repo)
  }

  fn full_checkout() -> (TempDir, RepoRoot) {
    checkout(&[
      ("feishu_reply_scope.md", "SCOPE"),
      ("thread_reply_redline.md", "RED"),
      ("thread_reply_format.md", "FORMAT"),
    ])
  }

  /// A manager session with the given `feishu_origin` field value.
  fn manager_with(origin: Value) -> SessionMetadata {
    let mut meta = SessionMetadata::new("synthetic-session", "Session 1", TaskProfile::Manager);
    meta.extra.insert("feishu_origin".to_string(), origin);
    meta
  }

  fn synthetic_origin() -> Value {
    json!({"tenant_key": "synthetic-tenant", "chat_id": "oc_synthetic", "root_id": "om_synthetic_root"})
  }

  #[test]
  fn a_manager_run_of_a_feishu_thread_gets_the_contract_as_one_full_segment() {
    let (_dir, repo) = full_checkout();
    let segments = FeishuTurnContribution
      .instruction_segments(
        &manager_with(synthetic_origin()),
        "manager_turn",
        &synthetic_config("/synthetic/home"),
        &repo,
      )
      .unwrap();

    assert_eq!(segments.len(), 1);
    assert!(segments[0].text.starts_with(&thread_contract_part()));
    assert_eq!(segments[0].delivery, "full");
    let refs: Vec<&str> = segments[0].sources.iter().map(|source| source.source_ref.as_str()).collect();
    assert_eq!(
      refs,
      ["prompts/feishu_reply_scope.md", "prompts/thread_reply_redline.md", "prompts/thread_reply_format.md"]
    );
  }

  #[test]
  fn only_a_manager_run_of_a_feishu_thread_gets_the_contract() {
    let (_dir, repo) = full_checkout();
    let contribution = FeishuTurnContribution;
    for kind in ["work", "review"] {
      let segments = contribution.instruction_segments(
        &manager_with(synthetic_origin()),
        kind,
        &synthetic_config("/synthetic/home"),
        &repo,
      );
      assert!(segments.unwrap().is_empty(), "{kind}");
    }
    let no_origin = SessionMetadata::new("synthetic-session", "Session 1", TaskProfile::Manager);
    assert!(
      contribution
        .instruction_segments(&no_origin, "manager_turn", &synthetic_config("/synthetic/home"), &repo)
        .unwrap()
        .is_empty()
    );
    let null_origin = manager_with(Value::Null);
    assert!(
      contribution
        .instruction_segments(&null_origin, "manager_turn", &synthetic_config("/synthetic/home"), &repo)
        .unwrap()
        .is_empty()
    );
  }

  #[test]
  fn a_stored_origin_missing_a_field_fails_the_build() {
    let (_dir, repo) = full_checkout();
    let meta = manager_with(json!({"tenant_key": "synthetic-tenant", "chat_id": "oc_synthetic"}));
    let error = FeishuTurnContribution
      .instruction_segments(&meta, "manager_turn", &synthetic_config("/synthetic/home"), &repo)
      .unwrap_err();
    assert!(error.to_string().starts_with("feishu_origin is not a Feishu origin"), "{error}");
  }
}
