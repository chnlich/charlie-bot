//! A manager's first turn after a context reset: the reset note opens the launch prompt with the
//! reason, the reanchor sentence, and where the state lives. The task record rides the system
//! prompt's last block, so the note names no goal and no reading steps.

use std::time::{Duration, Instant};

use charliebot_core::event_types;
use charliebot_store::metadata::{TaskProfile, TaskSpec, TaskType};
use charliebot_tasks::prompts::LAUNCH_TEXT_FILENAME;
use charliebot_tasks::test_support::recovery_support::{BACKEND_ID, Stack, result_script, script_option};
use serde_json::json;

fn manager_task() -> TaskSpec {
  TaskSpec { goal: "Deliver the synthetic release".to_string(), task_type: Some(TaskType::Implement), ..TaskSpec::default() }
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
  let deadline = Instant::now() + Duration::from_secs(30);
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
async fn a_manager_reset_note_carries_the_reason_the_reanchor_sentence_and_the_state_lines() -> anyhow::Result<()> {
  let stack =
    Stack::start_with(vec![script_option(BACKEND_ID, &result_script("continued"))], vec![BACKEND_ID.to_string()])
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
    prompt.find("Your earlier turns ran under the previous rules."),
    prompt.find("State lives in `charliebot web status show`"),
  ];
  let positions = positions.map(|position| position.unwrap_or_else(|| panic!("a note line is missing from {prompt}")));
  assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{positions:?}\n{prompt}");
  assert!(!prompt.contains("The task record's goal is:"), "{prompt}");
  assert!(!prompt.contains("status Goal"), "{prompt}");
  Ok(())
}
