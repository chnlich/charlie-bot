//! The reparent rules of an agent caller and the `task_reparented` fact of every move.
//!
//! An agent rearranges its own subtree only: the moved task sits below the caller's task, and the new parent is the
//! caller's task or below it. The move keeps each moved session's subtree rules word for word, because the user alone
//! sets the rules that a session reads. Only the chain above the moved task changes, so the check compares the old
//! chain with the new one below their shared prefix: a rule the new chain adds refuses the move, and one rule the old
//! chain drops moves onto the moved task (the carry) when that task holds no subtree rule of its own.

use charliebot_core::event_types;
use serde_json::{Value, json};

use crate::control_events;
use crate::task_tree_state::{TreeIndex, ancestor_ids, index_meta};
use crate::{StoreError, StoreResult};

/// The refusal for an agent reparent outside the caller's own subtree.
pub const AGENT_REPARENT_SCOPE_REFUSAL: &str = "an agent may only reparent within its own subtree: the task must be below the caller's task, and the new parent must be the caller's task or below it";

/// A subtree rule that a reparent moves onto the moved task: the rule's reference and the task that held it.
#[derive(Clone, Debug)]
pub(crate) struct CarriedRule {
  pub(crate) reference: String,
  pub(crate) from_session_id: String,
}

/// Refuse an agent reparent unless the moved task sits strictly below the caller's task and the new parent is the
/// caller's task or below it. A null parent is outside every subtree. A missing task or parent answers its 404.
pub(crate) fn check_agent_scope(
  index: &TreeIndex,
  caller_task: &str,
  session_id: &str,
  new_parent: Option<&str>,
) -> StoreResult<()> {
  let Some(parent_id) = new_parent else {
    return Err(StoreError::forbidden(AGENT_REPARENT_SCOPE_REFUSAL));
  };
  let task_below = ancestor_ids(index, session_id)?.iter().any(|id| id == caller_task);
  let parent_within = parent_id == caller_task || ancestor_ids(index, parent_id)?.iter().any(|id| id == caller_task);
  if !(task_below && parent_within) {
    return Err(StoreError::forbidden(AGENT_REPARENT_SCOPE_REFUSAL));
  }
  Ok(())
}

/// The prompt blockers of moving `session_id` under `new_parent`, and the rule the move carries when it drops exactly
/// one rule onto a task without its own. Each subtree rule applies to its holder and every node below it, so the old
/// chain (root to old parent) and the new chain (root to new parent) differ only below their shared prefix.
pub(crate) fn prompt_check(
  index: &TreeIndex,
  session_id: &str,
  new_parent: &str,
) -> StoreResult<(Vec<String>, Option<CarriedRule>)> {
  let mut old_chain = ancestor_ids(index, session_id)?;
  old_chain.reverse();
  let mut new_chain = ancestor_ids(index, new_parent)?;
  new_chain.reverse();
  new_chain.push(new_parent.to_string());
  let shared = old_chain.iter().zip(&new_chain).take_while(|(old, new)| old == new).count();
  let added = rule_holders(index, &new_chain[shared..])?;
  let dropped = rule_holders(index, &old_chain[shared..])?;

  let mut blockers: Vec<String> = added
    .iter()
    .map(|rule| {
      format!("reparent of {session_id} under {new_parent} would add the subtree rule of task {}", rule.from_session_id)
    })
    .collect();
  let holds_own_rule = index_meta(index, session_id)?.subtree_prompt_ref.is_some();
  let carried = match dropped.as_slice() {
    [] => None,
    [rule] if !holds_own_rule => Some(rule.clone()),
    _ => {
      let ids = dropped.iter().map(|rule| rule.from_session_id.as_str()).collect::<Vec<_>>().join(", ");
      blockers.push(format!(
        "reparent of {session_id} under {new_parent} would drop the subtree rules of tasks {ids}; a reparent carries one dropped rule onto a task without its own subtree rule"
      ));
      None
    }
  };
  Ok((blockers, carried))
}

/// The subtree rules that the nodes of one chain segment hold, in chain order.
fn rule_holders(index: &TreeIndex, chain: &[String]) -> StoreResult<Vec<CarriedRule>> {
  let mut rules = Vec::new();
  for id in chain {
    if let Some(reference) = index_meta(index, id)?.subtree_prompt_ref.as_deref() {
      rules.push(CarriedRule { reference: reference.to_string(), from_session_id: id.clone() });
    }
  }
  Ok(rules)
}

/// The `task_reparented` fact on the moved task. `caller_session_id` is the agent session (null for the operator), and
/// `carried_subtree_prompt` names the rule the move put on the moved task (null when it carried none).
pub(crate) fn reparented_event(
  actor: &str,
  session_id: &str,
  previous_parent_id: Option<&str>,
  task_parent_id: Option<&str>,
  caller_session_id: Option<&str>,
  carried: Option<&CarriedRule>,
) -> Value {
  let carried_subtree_prompt =
    carried.map_or(Value::Null, |rule| json!({"ref": rule.reference, "from_session_id": rule.from_session_id}));
  control_events::build_control_event(
    event_types::TASK_REPARENTED,
    actor,
    session_id,
    None,
    None,
    [
      ("previous_parent_id".to_string(), json!(previous_parent_id)),
      ("task_parent_id".to_string(), json!(task_parent_id)),
      ("caller_session_id".to_string(), json!(caller_session_id)),
      ("carried_subtree_prompt".to_string(), carried_subtree_prompt),
    ],
  )
}

#[cfg(test)]
mod tests {
  use serde_json::{Value, json};

  use super::*;
  use crate::metadata::{SessionMetadata, TaskProfile, TaskSpec};
  use crate::task_tree::tests::{archive_node, create_node, make_store, queued_run};
  use crate::{PatchTask, Store, StructuralBlockers, TaskCaller};

  const OPERATOR_REFUSAL: &str = "task metadata mutations require operator credentials";

  struct NoGuards;

  impl StructuralBlockers for NoGuards {
    fn pending_input_blockers(&self, _session_id: &str) -> StoreResult<Vec<String>> {
      Ok(Vec::new())
    }

    fn undelivered_report_blockers(&self, _session_id: &str) -> StoreResult<Vec<String>> {
      Ok(Vec::new())
    }
  }

  /// The caller's subtree `caller -> {a -> g -> gw, b -> bw}` and one root `outside` beyond it.
  struct Tree {
    caller: SessionMetadata,
    a: SessionMetadata,
    g: SessionMetadata,
    gw: SessionMetadata,
    b: SessionMetadata,
    bw: SessionMetadata,
    outside: SessionMetadata,
  }

  async fn tree(store: &Store) -> Tree {
    let caller = create_node(store, "caller", None, TaskProfile::Manager).await;
    let a = create_node(store, "a", Some(&caller.id), TaskProfile::Manager).await;
    let g = create_node(store, "g", Some(&a.id), TaskProfile::Manager).await;
    let gw = create_node(store, "gw", Some(&g.id), TaskProfile::Worker).await;
    let b = create_node(store, "b", Some(&caller.id), TaskProfile::Manager).await;
    let bw = create_node(store, "bw", Some(&b.id), TaskProfile::Worker).await;
    let outside = create_node(store, "outside", None, TaskProfile::Manager).await;
    Tree { caller, a, g, gw, b, bw, outside }
  }

  fn agent(task: &SessionMetadata) -> TaskCaller {
    TaskCaller::Agent { session_id: task.id.clone() }
  }

  fn move_under(parent: Option<&SessionMetadata>) -> PatchTask {
    PatchTask { task_parent_id: Some(parent.map(|meta| meta.id.clone())), ..Default::default() }
  }

  async fn set_subtree_rule(store: &Store, task: &SessionMetadata, body: &str) -> String {
    let edit = PatchTask { subtree_prompt: Some(Some(body.to_string())), ..Default::default() };
    store.patch_task(&task.id, edit, TaskCaller::Operator, &NoGuards).await.unwrap().subtree_prompt_ref.unwrap()
  }

  fn facts(store: &Store, session_id: &str, event_type: &str) -> Vec<Value> {
    store
      .load_chat_events_sync(session_id)
      .unwrap()
      .into_iter()
      .filter(|event| event["type"].as_str() == Some(event_type))
      .map(|event| event.to_value())
      .collect()
  }

  /// Everything a refused move must leave as it was: the moved task's metadata and its whole event log.
  fn moved_state(store: &Store, session_id: &str) -> (Option<String>, Option<String>, Vec<Value>) {
    let meta = store.read_metadata_fresh(session_id).unwrap().unwrap();
    let events = store.load_chat_events_sync(session_id).unwrap().into_iter().map(|event| event.to_value()).collect();
    (meta.task_parent_id, meta.subtree_prompt_ref, events)
  }

  #[tokio::test]
  async fn an_agent_moves_tasks_within_its_subtree_and_each_move_names_it() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let tree = tree(&store).await;

    let moved = store.patch_task(&tree.g.id, move_under(Some(&tree.b)), agent(&tree.caller), &NoGuards).await.unwrap();
    assert_eq!(moved.task_parent_id.as_deref(), Some(tree.b.id.as_str()));
    let fact = facts(&store, &tree.g.id, event_types::TASK_REPARENTED).pop().unwrap();
    assert_eq!(fact["actor"], "agent");
    assert_eq!(fact["source_session_id"], json!(tree.g.id));
    assert_eq!(fact["previous_parent_id"], json!(tree.a.id));
    assert_eq!(fact["task_parent_id"], json!(tree.b.id));
    assert_eq!(fact["caller_session_id"], json!(tree.caller.id));
    assert_eq!(fact["carried_subtree_prompt"], Value::Null);

    // A child's child moves under the caller's own task.
    let moved = store.patch_task(&tree.bw.id, move_under(Some(&tree.caller)), agent(&tree.caller), &NoGuards).await;
    assert_eq!(moved.unwrap().task_parent_id.as_deref(), Some(tree.caller.id.as_str()));
  }

  #[tokio::test]
  async fn operator_moves_record_the_user_and_a_same_parent_request_records_nothing() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let tree = tree(&store).await;

    store.patch_task(&tree.g.id, move_under(Some(&tree.outside)), TaskCaller::Operator, &NoGuards).await.unwrap();
    let fact = facts(&store, &tree.g.id, event_types::TASK_REPARENTED).pop().unwrap();
    assert_eq!(fact["actor"], "user");
    assert_eq!(fact["caller_session_id"], Value::Null);
    assert_eq!(fact["previous_parent_id"], json!(tree.a.id));
    assert_eq!(fact["task_parent_id"], json!(tree.outside.id));

    store.patch_task(&tree.bw.id, move_under(Some(&tree.b)), agent(&tree.caller), &NoGuards).await.unwrap();
    assert!(facts(&store, &tree.bw.id, event_types::TASK_REPARENTED).is_empty());
  }

  #[tokio::test]
  async fn agent_moves_outside_its_subtree_are_refused_with_the_scope_text() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let tree = tree(&store).await;

    let refusals = [
      (&tree.caller, move_under(Some(&tree.b)), agent(&tree.caller)),
      (&tree.outside, move_under(Some(&tree.b)), agent(&tree.caller)),
      (&tree.g, move_under(Some(&tree.outside)), agent(&tree.caller)),
      (&tree.g, move_under(None), agent(&tree.caller)),
      (&tree.g, move_under(Some(&tree.b)), agent(&tree.gw)),
    ];
    for (task, patch, caller) in refusals {
      let before = moved_state(&store, &task.id);
      let error = store.patch_task(&task.id, patch, caller, &NoGuards).await.unwrap_err();
      assert_eq!(error.status_code(), Some(403));
      assert_eq!(error.to_string(), AGENT_REPARENT_SCOPE_REFUSAL);
      assert_eq!(moved_state(&store, &task.id), before);
    }

    let ghost = PatchTask { task_parent_id: Some(Some("ghost".to_string())), ..Default::default() };
    let error = store.patch_task(&tree.g.id, ghost, agent(&tree.caller), &NoGuards).await.unwrap_err();
    assert_eq!(error.status_code(), Some(404));
  }

  #[tokio::test]
  async fn an_agent_body_with_any_other_field_keeps_the_operator_refusal() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let tree = tree(&store).await;
    let parent = Some(Some(tree.b.id.clone()));

    let bodies = [
      PatchTask { task_parent_id: parent.clone(), name: Some("renamed".to_string()), ..Default::default() },
      PatchTask { task_parent_id: parent.clone(), profile: Some(Some(TaskProfile::Manager)), ..Default::default() },
      PatchTask { task_parent_id: parent.clone(), task: Some(Some(TaskSpec::default())), ..Default::default() },
      PatchTask {
        task_parent_id: parent.clone(),
        subtree_prompt: Some(Some("rule\n".to_string())),
        ..Default::default()
      },
      PatchTask { task_parent_id: parent.clone(), node_prompt: Some(None), ..Default::default() },
      PatchTask { name: Some("renamed".to_string()), ..Default::default() },
    ];
    for patch in bodies {
      let error = store.patch_task(&tree.g.id, patch, agent(&tree.caller), &NoGuards).await.unwrap_err();
      assert_eq!(error.status_code(), Some(403));
      assert_eq!(error.to_string(), OPERATOR_REFUSAL);
    }
    assert_eq!(moved_state(&store, &tree.g.id).0.as_deref(), Some(tree.a.id.as_str()));
  }

  #[tokio::test]
  async fn structural_refusals_answer_an_agent_as_they_answer_the_operator() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let tree = tree(&store).await;
    let guard = store.control_lock().await;
    store.register_run_locked(&guard, &tree.gw.id, queued_run(&tree.gw.id, "unfinished"), None).await.unwrap();
    drop(guard);
    let closed = create_node(&store, "closed", Some(&tree.caller.id), TaskProfile::Manager).await;
    archive_node(&store, &closed.id).await;

    let moves = [(&tree.g, &tree.b), (&tree.a, &tree.g), (&tree.b, &tree.gw), (&tree.b, &closed)];
    for (task, parent) in moves {
      let before = moved_state(&store, &task.id);
      let operator = store.patch_task(&task.id, move_under(Some(parent)), TaskCaller::Operator, &NoGuards).await;
      let refused = store.patch_task(&task.id, move_under(Some(parent)), agent(&tree.caller), &NoGuards).await;
      let (operator, refused) = (operator.unwrap_err(), refused.unwrap_err());
      assert_eq!(refused.status_code(), Some(409));
      assert!(!refused.blockers().unwrap().is_empty());
      assert_eq!(refused.blockers(), operator.blockers());
      assert_eq!(moved_state(&store, &task.id), before);
    }
  }

  #[tokio::test]
  async fn a_move_that_drops_one_rule_carries_it_onto_the_moved_task() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let tree = tree(&store).await;
    let rule = set_subtree_rule(&store, &tree.a, "rule of a\n").await;

    let moved = store.patch_task(&tree.g.id, move_under(Some(&tree.b)), agent(&tree.caller), &NoGuards).await.unwrap();

    assert_eq!(moved.task_parent_id.as_deref(), Some(tree.b.id.as_str()));
    assert_eq!(moved.subtree_prompt_ref.as_deref(), Some(rule.as_str()));
    let prompt_fact = facts(&store, &tree.g.id, event_types::PROMPT_CHANGED).pop().unwrap();
    assert_eq!((&prompt_fact["scope"], &prompt_fact["previous_ref"]), (&json!("subtree"), &Value::Null));
    assert_eq!(prompt_fact["new_ref"], json!(rule));
    let fact = facts(&store, &tree.g.id, event_types::TASK_REPARENTED).pop().unwrap();
    assert_eq!(fact["carried_subtree_prompt"], json!({"ref": rule, "from_session_id": tree.a.id}));
  }

  #[tokio::test]
  async fn a_move_that_would_change_a_rule_is_refused_and_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let tree = tree(&store).await;
    set_subtree_rule(&store, &tree.b, "rule of b\n").await;
    let (g, b) = (&tree.g.id, &tree.b.id);
    let added = format!("reparent of {g} under {b} would add the subtree rule of task {b}");
    let before = moved_state(&store, g);
    let error = store.patch_task(g, move_under(Some(&tree.b)), agent(&tree.caller), &NoGuards).await.unwrap_err();
    assert_eq!(error.status_code(), Some(409));
    assert_eq!(error.blockers(), Some(std::slice::from_ref(&added)));
    assert_eq!(moved_state(&store, g), before);

    // Two dropped rules: the rules of a and g both sit above gw.
    set_subtree_rule(&store, &tree.a, "rule of a\n").await;
    set_subtree_rule(&store, &tree.g, "rule of g\n").await;
    let (a, gw, caller) = (&tree.a.id, &tree.gw.id, &tree.caller.id);
    let tail = "a reparent carries one dropped rule onto a task without its own subtree rule";
    let two = format!("reparent of {gw} under {caller} would drop the subtree rules of tasks {a}, {g}; {tail}");
    let before = moved_state(&store, gw);
    let error = store.patch_task(gw, move_under(Some(&tree.caller)), agent(&tree.caller), &NoGuards).await.unwrap_err();
    assert_eq!(error.blockers(), Some(std::slice::from_ref(&two)));
    assert_eq!(moved_state(&store, gw), before);

    // One dropped rule onto a task that holds its own subtree rule.
    let own = format!("reparent of {g} under {caller} would drop the subtree rules of tasks {a}; {tail}");
    let before = moved_state(&store, g);
    let error = store.patch_task(g, move_under(Some(&tree.caller)), agent(&tree.caller), &NoGuards).await.unwrap_err();
    assert_eq!(error.blockers(), Some(std::slice::from_ref(&own)));
    assert_eq!(moved_state(&store, g), before);
  }
}
