//! `web/features/taste`: the user's choice precedents. The crate owns the append-only store at
//! `$CHARLIEBOT_HOME/taste/rows.jsonl`, the `taste` CLI, and the turn contribution that puts the table's index into
//! every manager and worker run.

pub mod candidates;
pub mod cli;
pub mod store;
mod turn_contribution;

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};
use turn_contribution::TasteTurn;

/// The taste package's registration: its turn contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.turn_contribution("taste", Arc::new(TasteTurn))
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeSet;
  use std::fs;
  use std::path::{Path, PathBuf};

  use charliebot_core::config::sessions_dir_in;
  use charliebot_core::metadata_slot::METADATA_FILE_NAME;
  use charliebot_core::test_support::synthetic_config;
  use charliebot_store::layout::chat_events_path;
  use serde_json::json;
  use time::OffsetDateTime;
  use time::format_description::well_known::Rfc3339;

  use crate::candidates::{self, CandidateSource};
  use crate::store::{self, RowKind};

  const PICK_ANSWER: &str = "This should not be a question → Deploy config change";

  fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
      .ancestors()
      .nth(4)
      .expect("crate sits four levels under repo root")
      .to_path_buf()
  }

  #[test]
  fn taste_extract_prompt_carries_not_a_question_approval_rule_in_place() {
    let prompt =
      fs::read_to_string(repo_root().join("prompts/cron/taste_extract/prompt.md")).expect("taste_extract prompt reads");
    let start = prompt.find("## Which candidates become rows").expect("candidates section exists");
    let end = prompt.find("## Rows").expect("rows section exists");
    let approval_section = &prompt[start..end];
    let step_start = approval_section.find("A take-off or a go-ahead answers the step.").expect("step rule exists");
    let approval_block = &approval_section[..step_start];

    for expected in [
      "A pick entry whose answer starts with `This should not be a question` approves the recommendation of the fork that the entry names. It yields one `choice` row for that fork:",
      "- `options`: the fork's named options;",
      "- `recommended` and `chosen`: the recommendation;",
      "- `reason`: the entry's answer verbatim.",
      "When that recommendation names a high-consequence step, the entry also takes off that step, and the step yields no row.",
    ] {
      assert!(approval_block.contains(expected), "approval block lacks {expected:?}:\n{approval_block}");
    }
  }

  #[test]
  fn not_a_question_pick_yields_one_choice_row_and_no_extra_take_off_row() {
    let home = tempfile::tempdir().expect("temp CHARLIEBOT_HOME");
    let cfg = synthetic_config(home.path());
    let session_id = "sess-not-a-question";
    let session_dir = sessions_dir_in(&cfg.charliebot_home).join(session_id);
    let events_path = chat_events_path(&session_dir);
    fs::create_dir_all(events_path.parent().expect("events parent")).expect("data dir");
    fs::write(session_dir.join(METADATA_FILE_NAME), r#"{"profile":"manager"}"#).expect("metadata");

    let web_reply = json!({
      "type": "web_reply",
      "id": "w1",
      "content": "Q1: Deploy config change or Hold change? Recommended: Deploy config change. Say take off to deploy.",
      "timestamp": "2026-11-22T10:00:00+00:00",
    });
    let pick_comment = json!({
      "type": "user",
      "id": "u1",
      "content": format!("[Artifact comments · /sessions/{session_id}/artifacts/plan_v1.html] (1)\n\n▸ Q1: Which rollout action?\n↳ {PICK_ANSWER}"),
      "timestamp": "2026-11-22T10:05:00+00:00",
    });
    fs::write(&events_path, format!("{web_reply}\n{pick_comment}\n")).expect("chat events");

    let since = OffsetDateTime::parse("2026-11-22T00:00:00+00:00", &Rfc3339).expect("valid since");
    let sources: BTreeSet<CandidateSource> = [CandidateSource::Comments].into_iter().collect();
    let collected = candidates::collect(&sessions_dir_in(&cfg.charliebot_home), since, &sources, &|_| Ok(None))
      .expect("collect candidates");
    assert_eq!(collected.len(), 1);
    assert!(collected[0].text.contains(PICK_ANSWER));
    assert!(collected[0].previous_web_reply.as_deref().is_some_and(|reply| reply.contains("Say take off to deploy.")));

    let taste_dir = store::taste_dir(&cfg);
    let extracted_row = json!({
      "type": "row",
      "decided_at": "2026-11-22T10:05:00+00:00",
      "domain": "charliebot/approval-flow",
      "kind": "choice",
      "situation": "whether to deploy a reversible config change or hold it",
      "options": ["Deploy config change", "Hold change"],
      "recommended": "Deploy config change",
      "chosen": "Deploy config change",
      "reason": PICK_ANSWER,
      "source": {
        "session_id": session_id,
        "url": format!("/?session={session_id}"),
        "event_id": "u1",
        "comment_item": 1,
      },
      "supersedes": null,
    });
    assert_eq!(store::add_row(&taste_dir, &extracted_row).expect("add extracted row"), "T1");

    let loaded = store::load_store(&taste_dir).expect("load store");
    let rows = loaded.query_domain("charliebot/approval-flow").expect("query domain");
    assert_eq!(rows.len(), 1, "take-off step yields no extra row");
    let row = rows[0];
    assert_eq!(row.kind, RowKind::Choice);
    assert_eq!(row.options, ["Deploy config change", "Hold change"]);
    assert_eq!(row.recommended.as_deref(), Some("Deploy config change"));
    assert_eq!(row.chosen, "Deploy config change");
    assert_eq!(row.reason, PICK_ANSWER);
    assert!(row.reason.starts_with("This should not be a question"));
  }
}
