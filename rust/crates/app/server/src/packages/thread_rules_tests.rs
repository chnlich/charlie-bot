//! Pins the five thread-rule sentences that a manager turn of a Slack or Discord thread session carries.
//!
//! The sentences sit in `prompts/thread_session.md` and `prompts/thread_reply_format.md`. The golden prompt test
//! compares Rust with Python, so a sentence deleted from the prompts changes both sides and still passes there. This
//! test assembles the turn from the real checkout and the server's registry, so a deleted sentence, or a thread
//! package missing from the registry, fails it.

use std::path::{Path, PathBuf};

use charliebot_core::RepoRoot;
use charliebot_core::config::{
  BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_tasks::prompts::{PromptEnv, RuleInputs, build_segments};
use serde_json::json;

use super::registry;

/// The five sentences, verbatim from the two prompt files.
const THREAD_RULE_SENTENCES: [&str; 5] = [
  "- In this thread session, open your last session message of each turn with the status block:",
  "- The status block stays in the session message.",
  "The thread reply follows `prompts/thread_reply_format.md`.",
  "This rule decides the reply language for every summoner, the operator included.",
  "The summon line of the round and the lines of this contract set no reply language.",
];

/// `rust/crates/app/server` sits four levels below the checkout root.
fn checkout_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("the crate sits under the checkout").to_path_buf()
}

fn config(home: &Path) -> CharlieBotConfig {
  CharlieBotConfig {
    charliebot_home: home.to_path_buf(),
    headless_chrome_bin: "/synthetic/bin/chrome".to_string(),
    server: ServerConfig {
      host: "127.0.0.1".to_string(),
      port: 18650,
      subprocess_buffer_limit_mb: 1,
      session_memory_max_mb: 0,
      session_swap_max_mb: 0,
      session_tasks_max: 0,
      min_free_disk_gib: 0,
    },
    paths: PathsConfig {
      workspace_dirs: Vec::new(),
      worktree_dir: "/synthetic/worktrees".to_string(),
      scratch_dir: "/synthetic/scripts".to_string(),
    },
    backends: BackendsConfig { pool: Vec::new(), routing: None, options: Vec::new() },
    ui: UiConfig { home_services: Vec::new() },
    telegram: TelegramConfig { chat_id: None },
    sections: Sections::default(),
  }
}

fn slack_thread_session() -> SessionMetadata {
  let mut meta = SessionMetadata::new("synthetic-slack-session", "Synthetic Slack thread", TaskProfile::Manager);
  meta.set_extra(
    "slack_origin",
    json!({"team_id": "T0SYNTHETIC", "channel_id": "C0SYNTHETIC", "thread_ts": "1700000000.000100"}),
  );
  meta
}

fn discord_thread_session() -> SessionMetadata {
  let mut meta = SessionMetadata::new("synthetic-discord-session", "Synthetic Discord thread", TaskProfile::Manager);
  meta.set_extra(
    "discord_origin",
    json!({"guild_id": "G0SYNTHETIC", "parent_channel_id": "C0SYNTHETIC", "thread_id": "900000000000000030"}),
  );
  meta
}

/// The rule text of one manager turn of `meta`: every segment, joined in the order the launch assembles them.
fn manager_turn_text(meta: &SessionMetadata) -> String {
  let registry = registry().expect("the listed packages register");
  let checkout = RepoRoot::new(Some(checkout_root().into_os_string())).expect("the checkout path is valid");
  let home = tempfile::tempdir().expect("temp home");
  let table = home.path().join("config.d").join("approval_policy.yaml");
  std::fs::create_dir_all(table.parent().expect("the table path has a parent")).expect("config.d");
  std::fs::write(&table, APPROVAL_POLICY_TABLE).expect("the approval policy table writes");
  let cfg = config(home.path());
  let env = PromptEnv { cfg: &cfg, registry: &registry, checkout: &checkout };
  let inputs = RuleInputs { overlay: None, chain: &[], node_ref: None };
  let (segments, _overlay_error) =
    build_segments(&env, meta, "manager_turn", &inputs).expect("the manager turn assembles from the checkout");
  segments.iter().map(|segment| segment.text.as_str()).collect::<Vec<&str>>().join("\n")
}

/// The approval policy table the synthetic home starts with; the test reads the thread rules, not the table.
const APPROVAL_POLICY_TABLE: &str = "\
consequence_tests:
  - {id: irreversible, rank: 1, text: \"Deletes data with no backup, or sends what cannot be recalled.\"}
  - {id: chao-name, rank: 2, text: \"A message or publication in Chao's name reaches other people.\"}
  - {id: money, rank: 3, text: \"Spends money beyond a budget that Chao set.\"}
  - {id: authority, rank: 4, text: \"Widens what agents may do.\"}
  - {id: rule-named, rank: 4, text: \"A rule in force names the user's explicit approval for this step.\"}
  - {id: reading, rank: 5, text: \"Picks a reading for work over about one worker-day.\"}
standing_policies: []
enable_ask_tier_refusal: false
";

fn assert_thread_rules(platform: &str, meta: &SessionMetadata) {
  let text = manager_turn_text(meta);
  for sentence in THREAD_RULE_SENTENCES {
    assert!(text.contains(sentence), "{platform} thread session: the rule text lacks the sentence {sentence:?}");
  }
}

#[test]
fn slack_thread_session_carries_the_thread_rule_sentences() {
  assert_thread_rules("Slack", &slack_thread_session());
}

#[test]
fn discord_thread_session_carries_the_thread_rule_sentences() {
  assert_thread_rules("Discord", &discord_thread_session());
}
