use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::Path;

use charliebot_core::RepoRoot;
use charliebot_core::config::{
  BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_registry::turn::RuleSegment;
use charliebot_registry::{RegisterFn, Registry};
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use flate2::read::GzDecoder;
use serde_json::{Value, json};

const PACKAGES: [(&str, &str, RegisterFn); 3] = [
  ("web_reply", "features.web_reply", charliebot_web_reply::register),
  ("slack", "features.slack", charliebot_slack::register),
  ("discord", "features.discord", charliebot_discord::register),
];

macro_rules! checkout_case_group {
  ($name:ident, $checkout_name:literal, $group:literal) => {
    mod $name {
      #[test]
      fn web_reply() {
        super::assert_checkout_contributions_match_recorded_cases($checkout_name, $group, 0);
      }

      #[test]
      fn slack() {
        super::assert_checkout_contributions_match_recorded_cases($checkout_name, $group, 1);
      }

      #[test]
      fn discord() {
        super::assert_checkout_contributions_match_recorded_cases($checkout_name, $group, 2);
      }
    }
  };
}

checkout_case_group!(real_checkout_group_1, "real", 0);
checkout_case_group!(real_checkout_group_2, "real", 1);
checkout_case_group!(real_checkout_group_3, "real", 2);
checkout_case_group!(real_checkout_group_4, "real", 3);
checkout_case_group!(real_checkout_group_5, "real", 4);
checkout_case_group!(real_checkout_group_6, "real", 5);
checkout_case_group!(real_checkout_group_7, "real", 6);
checkout_case_group!(padded_checkout_group_1, "padded", 0);
checkout_case_group!(padded_checkout_group_2, "padded", 1);
checkout_case_group!(padded_checkout_group_3, "padded", 2);
checkout_case_group!(padded_checkout_group_4, "padded", 3);
checkout_case_group!(padded_checkout_group_5, "padded", 4);
checkout_case_group!(padded_checkout_group_6, "padded", 5);
checkout_case_group!(padded_checkout_group_7, "padded", 6);
checkout_case_group!(missing_web_reply_checkout_group_1, "without-web_reply.md", 0);
checkout_case_group!(missing_web_reply_checkout_group_2, "without-web_reply.md", 1);
checkout_case_group!(missing_web_reply_checkout_group_3, "without-web_reply.md", 2);
checkout_case_group!(missing_web_reply_checkout_group_4, "without-web_reply.md", 3);
checkout_case_group!(missing_web_reply_checkout_group_5, "without-web_reply.md", 4);
checkout_case_group!(missing_web_reply_checkout_group_6, "without-web_reply.md", 5);
checkout_case_group!(missing_web_reply_checkout_group_7, "without-web_reply.md", 6);
checkout_case_group!(missing_slack_scope_checkout_group_1, "without-slack_reply_scope.md", 0);
checkout_case_group!(missing_slack_scope_checkout_group_2, "without-slack_reply_scope.md", 1);
checkout_case_group!(missing_slack_scope_checkout_group_3, "without-slack_reply_scope.md", 2);
checkout_case_group!(missing_slack_scope_checkout_group_4, "without-slack_reply_scope.md", 3);
checkout_case_group!(missing_slack_scope_checkout_group_5, "without-slack_reply_scope.md", 4);
checkout_case_group!(missing_slack_scope_checkout_group_6, "without-slack_reply_scope.md", 5);
checkout_case_group!(missing_slack_scope_checkout_group_7, "without-slack_reply_scope.md", 6);
checkout_case_group!(missing_discord_scope_checkout_group_1, "without-discord_reply_scope.md", 0);
checkout_case_group!(missing_discord_scope_checkout_group_2, "without-discord_reply_scope.md", 1);
checkout_case_group!(missing_discord_scope_checkout_group_3, "without-discord_reply_scope.md", 2);
checkout_case_group!(missing_discord_scope_checkout_group_4, "without-discord_reply_scope.md", 3);
checkout_case_group!(missing_discord_scope_checkout_group_5, "without-discord_reply_scope.md", 4);
checkout_case_group!(missing_discord_scope_checkout_group_6, "without-discord_reply_scope.md", 5);
checkout_case_group!(missing_discord_scope_checkout_group_7, "without-discord_reply_scope.md", 6);
checkout_case_group!(missing_thread_redline_checkout_group_1, "without-thread_reply_redline.md", 0);
checkout_case_group!(missing_thread_redline_checkout_group_2, "without-thread_reply_redline.md", 1);
checkout_case_group!(missing_thread_redline_checkout_group_3, "without-thread_reply_redline.md", 2);
checkout_case_group!(missing_thread_redline_checkout_group_4, "without-thread_reply_redline.md", 3);
checkout_case_group!(missing_thread_redline_checkout_group_5, "without-thread_reply_redline.md", 4);
checkout_case_group!(missing_thread_redline_checkout_group_6, "without-thread_reply_redline.md", 5);
checkout_case_group!(missing_thread_redline_checkout_group_7, "without-thread_reply_redline.md", 6);
checkout_case_group!(missing_thread_format_checkout_group_1, "without-thread_reply_format.md", 0);
checkout_case_group!(missing_thread_format_checkout_group_2, "without-thread_reply_format.md", 1);
checkout_case_group!(missing_thread_format_checkout_group_3, "without-thread_reply_format.md", 2);
checkout_case_group!(missing_thread_format_checkout_group_4, "without-thread_reply_format.md", 3);
checkout_case_group!(missing_thread_format_checkout_group_5, "without-thread_reply_format.md", 4);
checkout_case_group!(missing_thread_format_checkout_group_6, "without-thread_reply_format.md", 5);
checkout_case_group!(missing_thread_format_checkout_group_7, "without-thread_reply_format.md", 6);

fn assert_checkout_contributions_match_recorded_cases(checkout_name: &str, group: usize, package_index: usize) {
  let mut decoder = GzDecoder::new(checkout_fixture(checkout_name));
  let mut fixture_text = String::new();
  decoder.read_to_string(&mut fixture_text).unwrap();
  let fixture: Value = serde_json::from_str(&fixture_text).unwrap();
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  let cases = fixture["cases"].as_array().unwrap();
  assert_eq!(cases.len(), 21);
  let (key, package, register) = PACKAGES[package_index];
  let registry = registered_package(package, register);

  let root = tempfile::tempdir().unwrap();
  let checkouts = fixture["checkouts"].as_object().unwrap();
  assert_eq!(checkouts.len(), 1);
  let documents = checkouts.get(checkout_name).expect("the checkout is recorded");
  let prompts = root.path().join(checkout_name).join("prompts");
  fs::create_dir_all(&prompts).unwrap();
  for (document, text) in documents.as_object().unwrap() {
    fs::write(prompts.join(document), text.as_str().unwrap()).unwrap();
  }

  if checkout_name == "real" && group == 0 {
    let segment_answers = cases
      .iter()
      .filter(|case| case["expected"][key]["segments"].as_array().is_some_and(|segments| !segments.is_empty()))
      .count();
    assert!(segment_answers > 0, "{key} records segment answers");
  }

  let cases_for_checkout =
    cases.iter().filter(|case| case["checkout"].as_str() == Some(checkout_name)).collect::<Vec<_>>();
  assert_eq!(cases_for_checkout.len(), 21);
  let group_start = group * 3;
  let group_end = group_start + 3;
  let root_text = root.path().to_string_lossy();
  if checkout_name == "without-web_reply.md" && key == "web_reply" && group == 0 {
    let expected_failures = cases
      .iter()
      .filter(|case| {
        case["checkout"].as_str() == Some(checkout_name) && case["expected"]["web_reply"].get("error").is_some()
      })
      .count();
    assert_eq!(expected_failures, 3);
  }
  for case in cases_for_checkout[group_start..group_end].iter().copied() {
    let repo_path = root.path().join(checkout_name);
    let actual = rust_answer(case, &repo_path, &registry);
    let expected = &case["expected"][key];
    if checkout_name == "without-web_reply.md" && key == "web_reply" && expected.get("error").is_some() {
      assert!(
        actual.get("error").is_some(),
        "expected the missing prompt to fail on both sides: case={case:?} python={expected} rust={actual}"
      );
    } else if !matches_materialized_paths(expected, &actual, &root_text) {
      panic!(
        "recorded Python contribution differs: checkout={checkout_name} case={case:?} key={key} expected={expected} actual={actual}"
      );
    }
  }
}

fn checkout_fixture(checkout_name: &str) -> &'static [u8] {
  match checkout_name {
    "real" => include_bytes!("fixtures/instruction_segments_golden/real.json.gz").as_slice(),
    "padded" => include_bytes!("fixtures/instruction_segments_golden/padded.json.gz").as_slice(),
    "without-web_reply.md" => {
      include_bytes!("fixtures/instruction_segments_golden/without-web_reply.md.json.gz").as_slice()
    }
    "without-slack_reply_scope.md" => {
      include_bytes!("fixtures/instruction_segments_golden/without-slack_reply_scope.md.json.gz").as_slice()
    }
    "without-discord_reply_scope.md" => {
      include_bytes!("fixtures/instruction_segments_golden/without-discord_reply_scope.md.json.gz").as_slice()
    }
    "without-thread_reply_redline.md" => {
      include_bytes!("fixtures/instruction_segments_golden/without-thread_reply_redline.md.json.gz").as_slice()
    }
    "without-thread_reply_format.md" => {
      include_bytes!("fixtures/instruction_segments_golden/without-thread_reply_format.md.json.gz").as_slice()
    }
    _ => unreachable!(),
  }
}

fn registered_package(package: &'static str, register: RegisterFn) -> Registry {
  for field in ["slack_origin", "discord_origin", "feishu_origin"] {
    charliebot_registry::thread_origin::register_origin_field(field);
  }
  let mut registry = Registry::new();
  registry.register_package(package, register).unwrap();
  registry
}

fn rust_answer(case: &Value, repo_path: &Path, registry: &Registry) -> Value {
  let session = &case["session"];
  let profile = match session["profile"].as_str().unwrap() {
    "manager" => TaskProfile::Manager,
    "worker" => TaskProfile::Worker,
    other => panic!("unknown recorded profile {other:?}"),
  };
  let id = session["id"].as_str().unwrap();
  let mut meta = SessionMetadata::new(id, id, profile);
  meta.parent_session_id = session["parent"].as_str().map(str::to_string);
  meta.extra.extend(session["extra"].as_object().unwrap().clone());
  let repo = RepoRoot::new(Some(OsString::from(repo_path))).unwrap();
  let kind = case["kind"].as_str().unwrap();
  let config = config(id);
  match registry.instruction_segments(&meta, kind, &config, &repo) {
    Ok(segments) => json!({"segments": segments.iter().map(segment_json).collect::<Vec<_>>()}),
    Err(error) => json!({"error": format!("{error}")}),
  }
}

fn segment_json(segment: &RuleSegment) -> Value {
  json!({
    "text": segment.text,
    "sources": segment.sources.iter().map(|source| json!({
      "scope": source.scope,
      "source_ref": source.source_ref,
      "source_session_id": source.source_session_id,
    })).collect::<Vec<_>>(),
    "delivery": segment.delivery,
  })
}

fn config(session_id: &str) -> CharlieBotConfig {
  CharlieBotConfig {
    charliebot_home: format!("/synthetic/home/{session_id}").into(),
    headless_chrome_bin: "/synthetic/bin/chrome".to_string(),
    server: ServerConfig {
      host: "127.0.0.1".to_string(),
      port: 18650,
      subprocess_buffer_limit_mb: 1,
      session_memory_max_mb: 0,
      session_swap_max_mb: 0,
      public_url: None,
      session_tasks_max: 0,
      min_free_disk_gib: 0,
      enable_session_cpu_weight: true,
      enable_session_io_weight: true,
      enable_local_work_guard: false,
      enable_offline_shell: false,
    },
    paths: PathsConfig {
      workspace_dirs: Vec::new(),
      worktree_dir: "/synthetic/worktrees".to_string(),
      scratch_dir: "/synthetic/scripts".to_string(),
    },
    backends: BackendsConfig {
      routing: None,
      options: Vec::new(),
      pools: Vec::new(),
      calls: charliebot_core::config::CallsConfig::default(),
    },
    ui: UiConfig { home_services: Vec::new() },
    telegram: TelegramConfig { chat_id: None },
    remote_run: charliebot_core::config::RemoteRunConfig::default(),
    github: Default::default(),
    sections: Sections::default(),
  }
}

fn matches_materialized_paths(expected: &Value, actual: &Value, root: &str) -> bool {
  match (expected, actual) {
    (Value::String(expected), Value::String(actual)) => {
      if expected.contains("$TMPROOT") {
        actual == &expected.replace("$TMPROOT", root)
      } else {
        actual == expected
      }
    }
    (Value::Array(expected), Value::Array(actual)) => {
      expected.len() == actual.len()
        && expected.iter().zip(actual).all(|(expected, actual)| matches_materialized_paths(expected, actual, root))
    }
    (Value::Object(expected), Value::Object(actual)) => {
      expected.len() == actual.len()
        && expected.iter().all(|(key, expected)| {
          actual.get(key).is_some_and(|actual| matches_materialized_paths(expected, actual, root))
        })
    }
    _ => expected == actual,
  }
}
