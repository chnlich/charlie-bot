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

#[test]
fn real_checkout_contributions_match_the_recorded_cases() {
  assert_checkout_contributions_match_recorded_cases("real");
}

#[test]
fn padded_checkout_contributions_match_the_recorded_cases() {
  assert_checkout_contributions_match_recorded_cases("padded");
}

#[test]
fn missing_web_reply_checkout_contributions_match_the_recorded_cases() {
  assert_checkout_contributions_match_recorded_cases("without-web_reply.md");
}

#[test]
fn missing_slack_scope_checkout_contributions_match_the_recorded_cases() {
  assert_checkout_contributions_match_recorded_cases("without-slack_reply_scope.md");
}

#[test]
fn missing_discord_scope_checkout_contributions_match_the_recorded_cases() {
  assert_checkout_contributions_match_recorded_cases("without-discord_reply_scope.md");
}

#[test]
fn missing_thread_redline_checkout_contributions_match_the_recorded_cases() {
  assert_checkout_contributions_match_recorded_cases("without-thread_reply_redline.md");
}

#[test]
fn missing_thread_format_checkout_contributions_match_the_recorded_cases() {
  assert_checkout_contributions_match_recorded_cases("without-thread_reply_format.md");
}

fn assert_checkout_contributions_match_recorded_cases(checkout_name: &str) {
  let mut decoder = GzDecoder::new(include_bytes!("fixtures/instruction_segments_golden.json.gz").as_slice());
  let mut fixture_text = String::new();
  decoder.read_to_string(&mut fixture_text).unwrap();
  let fixture: Value = serde_json::from_str(&fixture_text).unwrap();
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  let cases = fixture["cases"].as_array().unwrap();
  assert_eq!(cases.len(), 147);
  let registries = registered_packages();

  let root = tempfile::tempdir().unwrap();
  let mut missing_web_failures = 0;
  let checkouts = fixture["checkouts"].as_object().unwrap();
  assert_eq!(checkouts.len(), 7);
  let documents = checkouts.get(checkout_name).expect("the checkout is recorded");
  let prompts = root.path().join(checkout_name).join("prompts");
  fs::create_dir_all(&prompts).unwrap();
  for (document, text) in documents.as_object().unwrap() {
    fs::write(prompts.join(document), text.as_str().unwrap()).unwrap();
  }

  if checkout_name == "real" {
    let mut segment_answers = [0usize; 3];
    let mut error_answers = [0usize; 3];
    for case in cases {
      for (index, (key, _, _)) in PACKAGES.iter().enumerate() {
        let expected = &case["expected"][*key];
        if expected.get("error").is_some() {
          error_answers[index] += 1;
        } else if expected["segments"].as_array().is_some_and(|segments| !segments.is_empty()) {
          segment_answers[index] += 1;
        }
      }
    }
    assert!(segment_answers.into_iter().all(|count| count > 0));
    assert!(error_answers.into_iter().all(|count| count > 0));
  }

  let cases_for_checkout = cases
    .iter()
    .filter(|case| case["checkout"].as_str() == Some(checkout_name))
    .collect::<Vec<_>>();
  assert_eq!(cases_for_checkout.len(), 21);
  let root_text = root.path().to_string_lossy();
  for case in cases_for_checkout {
    let repo_path = root.path().join(checkout_name);
    let actual = rust_answer(case, &repo_path, &registries);
    let expected = &case["expected"];
    for (key, _, _) in &PACKAGES {
      let python = &expected[*key];
      let rust = &actual[*key];
      if checkout_name == "without-web_reply.md" && *key == "web_reply" && python.get("error").is_some() {
        assert!(
          rust.get("error").is_some(),
          "expected the missing prompt to fail on both sides: case={case:?} python={python} rust={rust}"
        );
        missing_web_failures += 1;
      } else if !matches_materialized_paths(python, rust, &root_text) {
        panic!(
          "recorded Python contribution differs: checkout={checkout_name} case={case:?} key={key} expected={python} actual={rust}"
        );
      }
    }
  }

  assert_eq!(missing_web_failures, if checkout_name == "without-web_reply.md" { 3 } else { 0 });
}

fn registered_packages() -> Vec<Registry> {
  PACKAGES
    .iter()
    .map(|(_, package, register)| {
      let mut registry = Registry::new();
      registry.register_package(package, *register).unwrap();
      registry
    })
    .collect()
}

fn rust_answer(case: &Value, repo_path: &Path, registries: &[Registry]) -> Value {
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
  let mut answer = serde_json::Map::new();
  for ((key, _, _), registry) in PACKAGES.iter().zip(registries) {
    let result = match registry.instruction_segments(&meta, kind, &config, &repo) {
      Ok(segments) => json!({"segments": segments.iter().map(segment_json).collect::<Vec<_>>()}),
      Err(error) => json!({"error": format!("{error}")}),
    };
    answer.insert((*key).to_string(), result);
  }
  Value::Object(answer)
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
    },
    paths: PathsConfig {
      workspace_dirs: Vec::new(),
      worktree_dir: "/synthetic/worktrees".to_string(),
      scratch_dir: "/synthetic/scripts".to_string(),
    },
    backends: BackendsConfig { pool: Vec::new(), routing: None, options: Vec::new(), auto: None },
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
        && expected
          .iter()
            .all(|(key, expected)| {
              actual.get(key).is_some_and(|actual| matches_materialized_paths(expected, actual, root))
            })
    }
    _ => expected == actual,
  }
}
