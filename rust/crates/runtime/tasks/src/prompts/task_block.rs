//! The task record's one rendered text: the block the system prompt carries at its end and the
//! Task & context dialog shows through the effective-prompt preview.

use charliebot_store::metadata::TaskSpec;

/// The rendered task record (`render_task_block`): the goal verbatim under the heading, then one
/// line per acceptance item and one per context ref. A section with no items is omitted; a record
/// whose goal, acceptance and context refs are all empty renders no block (`None`).
pub fn render_task_block(task: &TaskSpec) -> Option<String> {
  if task.goal.trim().is_empty() && task.acceptance.is_empty() && task.context_refs.is_empty() {
    return None;
  }
  let mut text = String::from("# Task record\n\n");
  text.push_str(&task.goal);
  if !task.acceptance.is_empty() {
    text.push_str("\n\n## Acceptance");
    for item in &task.acceptance {
      text.push_str(&format!("\n- {item}"));
    }
  }
  if !task.context_refs.is_empty() {
    text.push_str("\n\n## Context refs");
    for item in &task.context_refs {
      text.push_str(&format!("\n- {item}"));
    }
  }
  Some(text)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn task(goal: &str, acceptance: &[&str], context_refs: &[&str]) -> TaskSpec {
    TaskSpec {
      goal: goal.to_string(),
      acceptance: acceptance.iter().map(|item| item.to_string()).collect(),
      context_refs: context_refs.iter().map(|item| item.to_string()).collect(),
      ..TaskSpec::default()
    }
  }

  #[test]
  fn the_block_holds_the_goal_the_acceptance_and_the_context_refs() {
    let text = render_task_block(&task("Deliver the page\nby Friday", &["A1 holds", "A2 holds"], &["docs/plan.md"]))
      .expect("a non-empty record renders");
    assert_eq!(
      text,
      "# Task record\n\nDeliver the page\nby Friday\n\n## Acceptance\n- A1 holds\n- A2 holds\n\n## Context \
       refs\n- docs/plan.md"
    );
  }

  #[test]
  fn empty_sections_are_omitted() {
    let text = render_task_block(&task("Ship it", &[], &[])).expect("a goal alone renders");
    assert_eq!(text, "# Task record\n\nShip it");
  }

  #[test]
  fn an_all_empty_record_renders_no_block() {
    assert_eq!(render_task_block(&task("  ", &[], &[])), None);
    assert_eq!(render_task_block(&TaskSpec::default()), None);
  }
}
