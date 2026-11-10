//! A manager's first turn after a context reset: the reset note opens the launch prompt with the reason, the task
//! record's goal, the session's status Goal and the orientation order.

use std::sync::Arc;
use std::time::{Duration, Instant};

use charliebot_core::event_types;
use charliebot_registry::turn::TurnContribution;
use charliebot_registry::{RegisterFn, Registry, RegistryError};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec, TaskType};
use charliebot_tasks::prompts::LAUNCH_TEXT_FILENAME;
use charliebot_tasks::test_support::recovery_support::{BACKEND_ID, Stack, result_script, script_option};
use serde_json::json;

const STATUS_GOAL: &str = "ship the synthetic release";
const TASK_GOAL: &str = "Deliver the synthetic release";

/// The bound every wait in this file uses. The integration runner shares its CPUs with the build and the other
/// selected tests, so a state that takes milliseconds locally can take seconds there; a wait reaches this bound only
/// when the state never arrives, and a wait that reaches it names the state it awaited.
const TEST_DEADLINE: Duration = Duration::from_secs(120);

/// A turn contribution that stands in for web_reply: a manager session's status Goal line.
struct SyntheticStatusGoal;

impl TurnContribution for SyntheticStatusGoal {
  fn class_name(&self) -> &'static str {
    "SyntheticStatusGoal"
  }

  fn status_goal_line(&self, meta: &SessionMetadata) -> anyhow::Result<Option<String>> {
    Ok(
      (meta.profile == TaskProfile::Manager)
        .then(|| format!("This session's status Goal, as this session last set it, is: {STATUS_GOAL}")),
    )
  }
}

fn register_synthetic_status_goal(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.turn_contribution("synthetic_status_goal", Arc::new(SyntheticStatusGoal))
}

fn manager_task() -> TaskSpec {
  TaskSpec { goal: TASK_GOAL.to_string(), task_type: Some(TaskType::Implement), ..TaskSpec::default() }
}

/// Hold a native id whose recorded prompt hash no longer matches, so the next turn starts a fresh native conversation.
fn hold_stale_native_anchor(stack: &Stack, session_id: &str) -> anyhow::Result<()> {
  let mut meta = stack.store().read_metadata_fresh(session_id)?.expect("the manager session is recorded");
  meta.cc_session_id = Some("native-old".to_string());
  meta.native_prompt_hash = Some("stale-prompt-hash".to_string());
  meta.native_backend = Some(BACKEND_ID.to_string());
  meta.native_model = Some(format!("{BACKEND_ID}-model"));
  std::fs::write(
    stack.store().metadata_path(session_id),
    meta.to_json_string(&stack.features.registry.metadata_slots(), true),
  )?;
  stack.store().invalidate_cache(session_id);
  Ok(())
}

/// The id and launch text of the turn's manager run, once the run has written it.
async fn wait_for_launch_text(stack: &Stack, session_id: &str) -> anyhow::Result<(String, String)> {
  let deadline = Instant::now() + TEST_DEADLINE;
  loop {
    for run in stack.run_records(session_id)? {
      let path = stack.store().run_dir(session_id, &run.id).join(LAUNCH_TEXT_FILENAME);
      if path.is_file() {
        return Ok((run.id.clone(), std::fs::read_to_string(path)?));
      }
    }
    anyhow::ensure!(Instant::now() < deadline, "the manager turn never wrote its launch text");
    tokio::time::sleep(Duration::from_millis(5)).await;
  }
}

#[tokio::test]
#[ignore = "integration"]
async fn a_manager_reset_note_carries_the_reason_the_goals_and_the_orientation_order() -> anyhow::Result<()> {
  let extra: (&'static str, RegisterFn) = ("test.synthetic_status_goal", register_synthetic_status_goal);
  let stack = Stack::start_with_package(
    vec![script_option(BACKEND_ID, &result_script("continued"))],
    vec![BACKEND_ID.to_string()],
    extra,
  )
  .await?;
  stack.write_access_key()?;
  let manager = stack.node("manager", None, TaskProfile::Manager, Some(manager_task())).await?;
  hold_stale_native_anchor(&stack, &manager.id)?;

  stack.dispatch.admit_input(&manager.id, event_types::USER, "continue the work", "user", json!({})).await?;
  stack.dispatch.dispatch_pending(&manager.id).await?;

  let (run_id, prompt) = wait_for_launch_text(&stack, &manager.id).await?;
  stack.wait_terminal(&manager.id, &run_id).await?;
  assert!(prompt.starts_with("[Context reset: "), "{prompt}");

  let positions = [
    prompt.find("managed instructions or sources changed"),
    prompt.find(&format!("The task record's goal is: {TASK_GOAL}")),
    prompt.find(&format!("This session's status Goal, as this session last set it, is: {STATUS_GOAL}")),
    prompt.find("Before you respond, get oriented from your session status"),
  ];
  let positions = positions.map(|position| position.unwrap_or_else(|| panic!("a note line is missing from {prompt}")));
  assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{positions:?}\n{prompt}");
  Ok(())
}
