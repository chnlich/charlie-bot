#[cfg(test)]
mod tests {
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;
  use std::time::Duration;

  use charliebot_core::config::CharlieBotConfig;
  use charliebot_registry::Registry;
  use charliebot_registry::backend::{Env, LaunchArgs};
  use charliebot_store::metadata::UtcDateTime;
  use charliebot_store::run_records::RunKind;
  use serde_json::{Map, json};

  use crate::backends::claude_code::{claude_config, claude_lifecycle::EXCLUDE_DYNAMIC_SECTIONS_FLAG};

  use super::*;

  const IDLE_TOKENS: u64 = 100_000;
  const SESSION_ID: &str = "synthetic-idle-session";
  const CC_SESSION_ID: &str = "synthetic-claude-session";

  fn at(value: &str) -> UtcDateTime {
    UtcDateTime::parse(value).unwrap()
  }

  fn base_facts() -> IdleFacts {
    let request = at("2026-09-06T19:05:00Z");
    IdleFacts {
      archived: false,
      backend_unchanged: true,
      turn_busy: false,
      armed_at: Some(request),
      last_request_at: Some(request),
      compacted_after_request: false,
      context_tokens: Some(IDLE_TOKENS as i64),
      resume_id: Some(CC_SESSION_ID.to_string()),
      transcript_present: true,
    }
  }

  fn input(
    facts: IdleFacts,
    busy: bool,
    runs: Arc<AtomicUsize>,
    started: Option<Arc<tokio::sync::Notify>>,
    release: Option<Arc<tokio::sync::Notify>>,
  ) -> IdleTimerInput {
    let armed_at = facts.armed_at.expect("test facts have an armed request");
    IdleTimerInput {
      session_id: SESSION_ID.to_string(),
      armed_at,
      inspect: Arc::new(move |_| {
        let runs = Arc::clone(&runs);
        let started = started.clone();
        let release = release.clone();
        Ok(Some(IdleCandidate {
          facts: facts.clone(),
          idle_tokens: IDLE_TOKENS,
          run: Arc::new(move || {
            let runs = Arc::clone(&runs);
            let started = started.clone();
            let release = release.clone();
            Box::pin(async move {
              runs.fetch_add(1, Ordering::SeqCst);
              if let Some(started) = started {
                started.notify_one();
              }
              if let Some(release) = release {
                release.notified().await;
              }
              Ok(())
            })
          }),
        }))
      }),
      is_busy: Arc::new(move |_| Ok(busy)),
    }
  }

  fn config(root: &std::path::Path, pooled: bool) -> (Arc<Registry>, CharlieBotConfig) {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let body = if pooled {
      "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n      account_pool: alpha\naccounts:\n  claude:\n    - label: acct-a\n      config_dir: /synthetic/account-a\n  claude_pools:\n    alpha: [acct-a]\n"
    } else {
      "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n"
    };
    std::fs::write(home.join("config.yaml"), body).unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", crate::backends::claude_code::register).unwrap();
    let cfg = registry.load_config(&home).unwrap();
    (Arc::new(registry), cfg)
  }

  // Python: tests/test_claude_idle_compaction.py:148.
  // Python: tests/test_claude_idle_compaction.py:149.
  // Python: tests/test_claude_idle_compaction.py:150.
  // Python: tests/test_claude_idle_compaction.py:151.
  // Python: tests/test_claude_idle_compaction.py:152.
  // Python: tests/test_claude_idle_compaction.py:156.
  // Python: tests/test_claude_idle_compaction.py:163.
  // Python: tests/test_claude_idle_compaction.py:164.
  // Python: tests/test_claude_idle_compaction.py:165.
  // Python: tests/test_claude_idle_compaction.py:166.
  // Python: tests/test_claude_idle_compaction.py:167.
  #[test]
  fn every_idle_compaction_gate_rejects_its_python_case() {
    let now = at("2026-09-06T20:00:00Z");
    let cases: [(&str, fn(&mut IdleFacts)); 11] = [
      ("archived", |facts| facts.archived = true),
      ("backend_switched", |facts| facts.backend_unchanged = false),
      ("turn_running_or_queued", |facts| facts.turn_busy = true),
      ("armed_for_another_request", |facts| {
        facts.armed_at = Some(at("2026-09-06T19:04:00Z"));
      }),
      ("no_request", |facts| {
        facts.last_request_at = None;
        facts.armed_at = None;
      }),
      ("cache_expired", |facts| {
        let expired = at("2026-09-06T19:00:00Z");
        facts.last_request_at = Some(expired);
        facts.armed_at = Some(expired);
      }),
      ("compacted_after_request", |facts| facts.compacted_after_request = true),
      ("context_below_floor", |facts| facts.context_tokens = Some(IDLE_TOKENS as i64 - 1)),
      ("no_context_reading", |facts| facts.context_tokens = None),
      ("no_resume_id", |facts| facts.resume_id = None),
      ("transcript_missing", |facts| facts.transcript_present = false),
    ];

    for (name, change) in cases {
      let mut facts = base_facts();
      change(&mut facts);
      assert!(!should_compact(&facts, IDLE_TOKENS, now), "{name}");
    }
  }

  // Python: tests/test_claude_idle_compaction.py:176.
  #[test]
  fn every_idle_compaction_gate_true_starts_a_qualifying_candidate() {
    assert!(should_compact(
      &base_facts(),
      IDLE_TOKENS,
      at("2026-09-06T20:00:00Z"),
    ));
  }

  // Python: tests/test_claude_idle_compaction.py:186.
  #[test]
  fn idle_command_matches_the_turn_command_for_the_same_account() {
    let root = tempfile::tempdir().unwrap();
    let (registry, cfg) = config(root.path(), true);
    let option = cfg.backends.options[0].clone();
    let account = claude_config::ClaudeAccount { label: "acct-a".to_string(), config_dir: "/synthetic/account-a".to_string() };
    let base_env = Env::from([
      ("HOME".to_string(), "/synthetic/home".to_string()),
      ("PATH".to_string(), "/synthetic/bin:/usr/bin:/bin".to_string()),
    ]);
    let (argv, _) = build_idle_process(
      Arc::clone(&registry),
      &cfg,
      &option,
      &account,
      SESSION_ID,
      CC_SESSION_ID,
      RunKind::ManagerTurn,
      &base_env,
    )
    .unwrap();

    let mut backend_kwargs = Map::new();
    backend_kwargs.insert(
      "claude_account".to_string(),
      json!({"label": account.label, "config_dir": account.config_dir}),
    );
    backend_kwargs.insert("run_kind".to_string(), json!(RunKind::ManagerTurn));
    let expected = crate::backend_types::build_backend(
      Arc::clone(&registry),
      &option,
      &cfg,
      LaunchArgs {
        extra_flags: vec![
          "--resume".to_string(),
          CC_SESSION_ID.to_string(),
          EXCLUDE_DYNAMIC_SECTIONS_FLAG.to_string(),
        ],
        cgroup_session_id: Some(SESSION_ID.to_string()),
        backend_kwargs,
        ..LaunchArgs::default()
      },
    )
    .unwrap()
    .backend
    .build_command("/compact\n")
    .unwrap();

    assert_eq!(argv, expected);
  }

  // Python: tests/test_claude_idle_compaction.py:222.
  #[tokio::test]
  async fn a_second_arm_keeps_one_timer_and_replaces_the_first() {
    let idle = IdleCompaction::new();
    let runs = Arc::new(AtomicUsize::new(0));
    idle
      .arm_for_test(
        input(base_facts(), false, Arc::clone(&runs), None, None),
        Duration::from_millis(10),
      )
      .await;
    idle
      .arm_for_test(
        input(base_facts(), false, runs.clone(), None, None),
        Duration::from_secs(3600),
      )
      .await;
    tokio::time::sleep(Duration::from_millis(25)).await;

    assert_eq!(idle.timer_count(), 1);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
  }

  // Python: tests/test_claude_idle_compaction.py:237.
  #[tokio::test]
  async fn a_turn_start_cancels_the_pending_idle_timer() {
    let idle = IdleCompaction::new();
    let runs = Arc::new(AtomicUsize::new(0));
    idle
      .arm_for_test(input(base_facts(), false, Arc::clone(&runs), None, None), Duration::from_secs(3600))
      .await;

    idle.settle_for_turn(SESSION_ID).await.unwrap();
    tokio::task::yield_now().await;

    assert_eq!(idle.timer_count(), 0);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
  }

  // Python: tests/test_claude_idle_compaction.py:256.
  #[tokio::test]
  async fn a_turn_waits_until_the_running_idle_compaction_finishes() {
    let idle = IdleCompaction::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    idle
      .arm_for_test(
        input(base_facts(), false, runs, Some(Arc::clone(&started)), Some(Arc::clone(&release))),
        Duration::from_secs(3600),
      )
      .await;
    let compaction = idle
      .fire_at(SESSION_ID, at("2026-09-06T19:55:00Z"))
      .unwrap()
      .expect("all idle conditions hold");
    started.notified().await;
    let waiting_idle = idle.clone();
    let turn = tokio::spawn(async move { waiting_idle.settle_for_turn(SESSION_ID).await.unwrap() });
    tokio::task::yield_now().await;
    assert!(!turn.is_finished());

    release.notify_one();
    turn.await.unwrap();
    compaction.await.unwrap();
  }

  // Python: tests/test_claude_idle_compaction.py:280.
  #[tokio::test]
  async fn a_queued_turn_makes_the_idle_trigger_skip() {
    let idle = IdleCompaction::new();
    let runs = Arc::new(AtomicUsize::new(0));
    idle
      .arm_for_test(input(base_facts(), true, Arc::clone(&runs), None, None), Duration::from_secs(3600))
      .await;

    assert!(idle.fire_at(SESSION_ID, at("2026-09-06T19:55:00Z")).unwrap().is_none());
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(idle.running_count(), 0);
  }

  // Python: tests/test_claude_idle_compaction.py:309.
  #[test]
  fn idle_run_options_keep_the_session_model_and_idle_event_triggers() {
    let root = tempfile::tempdir().unwrap();
    let (_registry, cfg) = config(root.path(), true);
    let option = cfg.backends.options[0].clone();
    let account = claude_config::ClaudeAccount { label: "acct-a".to_string(), config_dir: "/synthetic/account-a".to_string() };
    let run = idle_run_options(
      &option,
      &account,
      SESSION_ID,
      CC_SESSION_ID,
      std::path::Path::new("/synthetic/session"),
      Some(120_000),
      vec!["claude".to_string()],
      Env::new(),
    );

    assert_eq!(run.config_dir, "/synthetic/account-a");
    assert_eq!(run.cwd, std::path::Path::new("/synthetic/session"));
    assert_eq!(run.pre_tokens, Some(120_000));
    assert_eq!((run.trigger.as_str(), run.failure_trigger.as_deref()), ("idle", Some("idle")));
    assert_eq!((run.model.as_str(), run.family.as_str()), ("claude-fable-5-1", "fable"));
  }

  // Python: tests/test_claude_idle_compaction.py:341.
  #[test]
  fn an_unpooled_session_uses_the_default_login_directory() {
    let root = tempfile::tempdir().unwrap();
    let (registry, cfg) = config(root.path(), false);
    let option = cfg.backends.options[0].clone();
    let default_dir = root.path().join("default-claude");
    let account = account_for_session(&cfg, &option, None, &default_dir).unwrap().unwrap();
    let base_env = Env::from([
      ("HOME".to_string(), root.path().to_string_lossy().into_owned()),
      ("PATH".to_string(), "/synthetic/bin:/usr/bin:/bin".to_string()),
    ]);
    let (_, env) = build_idle_process(
      registry,
      &cfg,
      &option,
      &account,
      SESSION_ID,
      CC_SESSION_ID,
      RunKind::ManagerTurn,
      &base_env,
    )
    .unwrap();

    assert_eq!(account.label, "default");
    assert_eq!(env.get("CLAUDE_CONFIG_DIR").unwrap(), &default_dir.to_string_lossy());
  }
}
