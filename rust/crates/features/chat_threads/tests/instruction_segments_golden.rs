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
fn web_reply_contributions_match_the_recorded_cases() {
  assert_contributions_match_recorded_cases("web_reply");
}

#[test]
fn slack_contributions_match_the_recorded_cases() {
  assert_contributions_match_recorded_cases("slack");
}

#[test]
fn discord_contributions_match_the_recorded_cases() {
  assert_contributions_match_recorded_cases("discord");
}

fn assert_contributions_match_recorded_cases(package_key: &str) {
  let mut decoder = GzDecoder::new(include_bytes!("fixtures/instruction_segments_golden.json.gz").as_slice());
  let mut fixture_text = String::new();
  decoder.read_to_string(&mut fixture_text).unwrap();
  let fixture: Value = serde_json::from_str(&fixture_text).unwrap();
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  assert_eq!(fixture["cases"].as_array().unwrap().len(), 147);
  let (_, package, register) = PACKAGES.iter().find(|(key, _, _)| *key == package_key).unwrap();
  let registry = registered_package(package, *register);

  let root = tempfile::tempdir().unwrap();
  let mut segment_answers = 0;
  let mut error_answers = 0;
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
    let actual = rust_answer(case, &repo_path, &registry);
    let mut expected = case["expected"][package_key].clone();
    materialize_paths(&mut expected, root.path());
    if expected.get("error").is_some() {
      error_answers += 1;
    } else if expected["segments"].as_array().is_some_and(|segments| !segments.is_empty()) {
      segment_answers += 1;
    }
    if checkout == "without-web_reply.md" && package_key == "web_reply" && expected.get("error").is_some() {
      assert!(
        actual.get("error").is_some(),
        "expected the missing prompt to fail on both sides: case={case:?} python={expected} rust={actual}"
      );
      missing_web_failures += 1;
    } else if expected != actual {
      panic!(
        "recorded Python contribution differs: checkout={checkout} case={case:?} key={package_key} expected={expected} actual={actual}"
      );
    }
  }

  assert!(segment_answers > 0);
  assert!(error_answers > 0);
  assert_eq!(missing_web_failures, if package_key == "web_reply" { 3 } else { 0 });
}

fn registered_package(package: &str, register: RegisterFn) -> Registry {
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
    },
    paths: PathsConfig {
      workspace_dirs: Vec::new(),
      worktree_dir: "/synthetic/worktrees".to_string(),
      scratch_dir: "/synthetic/scripts".to_string(),
    },
    backends: BackendsConfig { pool: Vec::new(), routing: None, options: Vec::new() },
    ui: UiConfig { home_services: Vec::new() },
    telegram: TelegramConfig { chat_id: None },
    remote_run: charliebot_core::config::RemoteRunConfig::default(),
    github: Default::default(),
    sections: Sections::default(),
  }
}

fn materialize_paths(value: &mut Value, root: &Path) {
  match value {
    Value::String(text) => *text = text.replace("$TMPROOT", &root.to_string_lossy()),
    Value::Array(values) => values.iter_mut().for_each(|value| materialize_paths(value, root)),
    Value::Object(values) => values.values_mut().for_each(|value| materialize_paths(value, root)),
    _ => (),
  }
}
