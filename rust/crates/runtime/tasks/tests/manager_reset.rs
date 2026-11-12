//! A manager's first turn after a context reset: the reset note opens the launch prompt with the reason and the
//! orientation order, and names no Goal. The session's Goal lines sit at the end of the system prompt the run saves.

use std::fs;
use std::time::{Duration, Instant};

use charliebot_core::event_types;
use charliebot_registry::RegisterFn;
use charliebot_store::metadata::{TaskProfile, TaskSpec, TaskType};
use charliebot_tasks::prompts::{LAUNCH_TEXT_FILENAME, SNAPSHOT_FILENAME};
use charliebot_tasks::test_support::recovery_support::{BACKEND_ID, Stack, result_script, script_option};
use charliebot_web_reply::status::set_status;
use serde_json::{Value, json};

const STATUS_GOAL: &str = "ship the synthetic release";
const REVISED_GOAL: &str = "ship the revised synthetic release";
const TASK_GOAL: &str = "Deliver the synthetic release";

/// The bound every wait in this file uses. The integration runner shares its CPUs with the build and the other
/// selected tests, so a state that takes milliseconds locally can take seconds there; a wait reaches this bound only
/// when the state never arrives, and a wait that reaches it names the state it awaited.
const TEST_DEADLINE: Duration = Duration::from_secs(120);

fn manager_task() -> TaskSpec {
  TaskSpec { goal: TASK_GOAL.to_string(), task_type: Some(TaskType::Implement), ..TaskSpec::default() }
}

/// Hold a native id recorded under `prompt_hash`, so a turn whose prompt hashes differently starts a fresh native
/// conversation.
fn hold_native_anchor(stack: &Stack, session_id: &str, prompt_hash: &str) -> anyhow::Result<()> {
  let mut meta = stack.store().read_metadata_fresh(session_id)?.expect("the manager session is recorded");
  meta.cc_session_id = Some("native-old".to_string());
  meta.native_prompt_hash = Some(prompt_hash.to_string());
  meta.native_backend = Some(BACKEND_ID.to_string());
  meta.native_model = Some(format!("{BACKEND_ID}-model"));
  std::fs::write(
    stack.store().metadata_path(session_id),
    meta.to_json_string(&stack.features.registry.metadata_slots(), true),
  )?;
  stack.store().invalidate_cache(session_id);
  Ok(())
}

/// The id and launch text of the turn's manager run, once the run has written it. A run in `known` is an earlier turn.
async fn wait_for_launch_text(stack: &Stack, session_id: &str, known: &[String]) -> anyhow::Result<(String, String)> {
  let deadline = Instant::now() + TEST_DEADLINE;
  loop {
    for run in stack.run_records(session_id)? {
      if known.contains(&run.id) {
        continue;
      }
      let path = stack.store().run_dir(session_id, &run.id).join(LAUNCH_TEXT_FILENAME);
      if path.is_file() {
        return Ok((run.id.clone(), std::fs::read_to_string(path)?));
      }
    }
    anyhow::ensure!(Instant::now() < deadline, "the manager turn never wrote its launch text");
    tokio::time::sleep(Duration::from_millis(5)).await;
  }
}

/// Admit one user message, run the turn, and return the run's id and launch text once the run ends.
async fn run_turn(stack: &Stack, session_id: &str, text: &str, known: &[String]) -> anyhow::Result<(String, String)> {
  stack.dispatch.admit_input(session_id, event_types::USER, text, "user", json!({})).await?;
  stack.dispatch.dispatch_pending(session_id).await?;
  let (run_id, prompt) = wait_for_launch_text(stack, session_id, known).await?;
  stack.wait_terminal(session_id, &run_id).await?;
  Ok((run_id, prompt))
}

/// The saved system prompt of a run: its snapshot's hash, and the text of its last block.
fn saved_prompt(stack: &Stack, session_id: &str, run_id: &str) -> anyhow::Result<Value> {
  let path = stack.store().run_dir(session_id, run_id).join(SNAPSHOT_FILENAME);
  Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

#[tokio::test]
#[ignore = "integration"]
async fn a_changed_own_goal_starts_a_fresh_conversation_whose_reset_note_names_no_goal() -> anyhow::Result<()> {
  let web_reply: (&'static str, RegisterFn) = ("charliebot.web_reply", charliebot_web_reply::register);
  let stack = Stack::start_with_package(
    vec![script_option(BACKEND_ID, &result_script("continued"))],
    vec![BACKEND_ID.to_string()],
    web_reply,
  )
  .await?;
  stack.write_access_key()?;
  let manager = stack.node("manager", None, TaskProfile::Manager, Some(manager_task())).await?;
  set_status(&stack.features, &manager.id, &json!({"goal": STATUS_GOAL}).to_string())
    .await
    .expect("the manager's status Goal is stored");

  let (first_run, _) = run_turn(&stack, &manager.id, "start the work", &[]).await?;
  let first_hash = saved_prompt(&stack, &manager.id, &first_run)?["prompt_hash"].as_str().expect("a hash").to_string();

  set_status(&stack.features, &manager.id, &json!({"goal": REVISED_GOAL}).to_string())
    .await
    .expect("the manager's status Goal is revised");
  hold_native_anchor(&stack, &manager.id, &first_hash)?;
  let (run_id, prompt) = run_turn(&stack, &manager.id, "continue the work", std::slice::from_ref(&first_run)).await?;

  assert!(prompt.starts_with("[Context reset: "), "{prompt}");
  let positions = [
    prompt.find("managed instructions or sources changed"),
    prompt.find("Before you respond, get oriented from your session status"),
  ];
  let positions = positions.map(|position| position.unwrap_or_else(|| panic!("a note line is missing from {prompt}")));
  assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{positions:?}\n{prompt}");
  assert!(
    !prompt.contains(STATUS_GOAL) && !prompt.contains(REVISED_GOAL) && !prompt.contains(TASK_GOAL),
    "the reset note names no goal: {prompt}"
  );

  let saved = saved_prompt(&stack, &manager.id, &run_id)?;
  let blocks = saved["blocks"].as_array().expect("the snapshot lists its blocks");
  assert_eq!(blocks.last().expect("the snapshot has blocks")["text"], format!("This session's Goal: {REVISED_GOAL}"));
  Ok(())
}
