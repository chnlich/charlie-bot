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
  ("web_reply", "src.features.web_reply", charliebot_web_reply::register),
  ("slack", "src.features.slack", charliebot_slack::register),
  ("discord", "src.features.discord", charliebot_discord::register),
];

#[test]
fn contributions_match_the_recorded_python_cases() {
  let mut decoder = GzDecoder::new(include_bytes!("fixtures/instruction_segments_golden.json.gz").as_slice());
  let mut fixture_text = String::new();
  decoder.read_to_string(&mut fixture_text).unwrap();
  let fixture: Value = serde_json::from_str(&fixture_text).unwrap();
  assert_eq!(fixture["source"], "rust/crates/features/chat_threads/tests/differential.py");
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  assert_eq!(fixture["cases"].as_array().unwrap().len(), 147);

  let root = tempfile::tempdir().unwrap();
  let mut segment_answers = [0usize; 3];
  let mut error_answers = [0usize; 3];
  let mut missing_web_failures = 0;
  let checkouts = fixture["checkouts"].as_object().unwrap();
  for (name, documents) in checkouts {
    let prompts = root.path().join(name).join("prompts");
    fs::create_dir_all(&prompts).unwrap();
    for (document, text) in documents.as_object().unwrap() {
      fs::write(prompts.join(document), text.as_str().unwrap()).unwrap();
    }
  }

  for case in fixture["cases"].as_array().unwrap() {
    let checkout = case["checkout"].as_str().unwrap();
    let repo_path = root.path().join(checkout);
    let actual = rust_answer(case, &repo_path);
    let expected = materialize_paths(case["expected"].clone(), root.path());
    for (index, (key, _, _)) in PACKAGES.iter().enumerate() {
      let python = &expected[*key];
      let rust = &actual[*key];
      if python.get("error").is_some() {
        error_answers[index] += 1;
      } else if python["segments"].as_array().is_some_and(|segments| !segments.is_empty()) {
        segment_answers[index] += 1;
      }
      if checkout == "without-web_reply.md" && *key == "web_reply" && python.get("error").is_some() {
        assert!(
          rust.get("error").is_some(),
          "expected the missing prompt to fail on both sides: case={case:?} python={python} rust={rust}"
        );
        missing_web_failures += 1;
      } else if python != rust {
        panic!(
          "recorded Python contribution differs: checkout={checkout} case={case:?} key={key} expected={python} actual={rust}"
        );
      }
    }
  }

  assert!(segment_answers.into_iter().all(|count| count > 0));
  assert!(error_answers.into_iter().all(|count| count > 0));
  assert_eq!(missing_web_failures, 3);
}

fn rust_answer(case: &Value, repo_path: &Path) -> Value {
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
  for (key, package, register) in PACKAGES {
    let mut registry = Registry::new();
    registry.register_package(package, register).unwrap();
    let result = match registry.instruction_segments(&meta, kind, &config, &repo) {
      Ok(segments) => json!({"segments": segments.iter().map(segment_json).collect::<Vec<_>>()}),
      Err(error) => json!({"error": format!("{error}")}),
    };
    answer.insert(key.to_string(), result);
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

fn materialize_paths(mut value: Value, root: &Path) -> Value {
  match &mut value {
    Value::String(text) => *text = text.replace("$TMPROOT", &root.to_string_lossy()),
    Value::Array(values) => values.iter_mut().for_each(|value| *value = materialize_paths(value.clone(), root)),
    Value::Object(values) => values.values_mut().for_each(|value| *value = materialize_paths(value.clone(), root)),
    _ => {}
  }
  value
}
