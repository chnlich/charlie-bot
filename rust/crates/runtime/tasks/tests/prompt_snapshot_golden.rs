//! The managed-instruction snapshots of the recorded Python golden matrix match byte for byte.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use charliebot_core::RepoRoot;
use charliebot_core::config::{
  BackendOption, BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_core::pyfmt::{JsonStyle, json_dumps};
use charliebot_registry::Registry;
use charliebot_registry::RegistryError;
use charliebot_registry::turn::TurnContribution;
use charliebot_store::control_events::sha256_hex;
use charliebot_store::metadata::{ColdReadSpec, SessionMetadata, TaskProfile, TaskSpec, TaskType};
use charliebot_tasks::prompts::{
  ChainLink, OverlayError, PromptBlock, PromptEnv, PromptSnapshot, PromptSource, RuleInputs, RuleSegment,
  assemble_snapshot, build_segments, resolve_launch_overlay,
};
use serde_json::{Value, json};
use tempfile::TempDir;

/// `json.dumps(value, ensure_ascii=False)` with the default separators and insertion order.
const PYTHON_STYLE: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };
const HASH_STYLE: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: true, item_separator: ", ", key_separator: ": ", indent: None };

const DAMAGED_CASE: &str = "non_ascii_rules";
const PYTHON_FIXTURE: &str = include_str!("fixtures/python_prompt_snapshot_golden.json");

/// One manager or worker session of the matrix.
#[derive(Clone, Copy)]
struct Meta {
  id: &'static str,
  name: &'static str,
  profile: &'static str,
  task: Option<Task>,
}

#[derive(Clone, Copy)]
struct Task {
  goal: &'static str,
  repo_path: Option<&'static str>,
  task_type: Option<&'static str>,
}

/// One golden case. Chain entries name rule bodies by `bodies` name; `None` is a null ref.
struct Case {
  name: &'static str,
  kind: &'static str,
  meta: Meta,
  overlay: Option<&'static str>,
  chain: &'static [(&'static str, Option<&'static str>)],
  node_body: Option<&'static str>,
  host: Option<&'static str>,
  bodies: &'static [(&'static str, &'static str)],
}

const REPO: &str = "/synthetic/repo";
const GOAL: &str = "Add the greeter.";

fn manager(id: &'static str, name: &'static str) -> Meta {
  Meta { id, name, profile: "manager", task: None }
}

fn worker(id: &'static str, name: &'static str, repo: Option<&'static str>, task_type: Option<&'static str>) -> Meta {
  Meta { id, name, profile: "worker", task: Some(Task { goal: GOAL, repo_path: repo, task_type }) }
}

fn matrix() -> Vec<Case> {
  const NO_CHAIN: &[(&str, Option<&str>)] = &[];
  const THREE_NODES: &[(&str, Option<&str>)] =
    &[("root-session", Some("root_rule")), ("mid-session", None), ("leaf-session", Some("leaf_rule"))];
  const SHARED: &[(&str, Option<&str>)] = &[("root-session", Some("shared_rule")), ("leaf-session", None)];
  const NON_ASCII: &[(&str, Option<&str>)] = &[("root-session", Some("subtree_non_ascii"))];
  const THREE_BODIES: &[(&str, &str)] = &[
    ("root_rule", "Root subtree rule for the whole tree."),
    ("leaf_rule", "Leaf subtree rule."),
    ("node_rule", "This node's own rule."),
  ];
  const SHARED_BODY: &[(&str, &str)] = &[("shared_rule", "Shared rule text.")];
  const NON_ASCII_BODIES: &[(&str, &str)] =
    &[("subtree_non_ascii", "子规则：naïve — 測試 🚀"), ("node_non_ascii", "节点规则 café ✓")];
  vec![
    Case {
      name: "manager_turn_without_host",
      kind: "manager_turn",
      meta: manager("root-session", "Root"),
      overlay: None,
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "manager_turn_with_host",
      kind: "manager_turn",
      meta: manager("root-session", "Root"),
      overlay: None,
      chain: NO_CHAIN,
      node_body: None,
      host: Some("Host rules for YOUR_SESSION_UUID.\nKeep the house style."),
      bodies: &[],
    },
    Case {
      name: "manager_turn_workflow_file",
      kind: "manager_turn",
      meta: manager("thread-session", "Thread Brief"),
      overlay: None,
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "overlay_declared_present",
      kind: "manager_turn",
      meta: manager("root-session", "Scoped Contribution"),
      overlay: Some("Kimi-K3"),
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "overlay_declared_missing",
      kind: "manager_turn",
      meta: manager("root-session", "Root"),
      overlay: Some("no-such-overlay"),
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "overlay_none",
      kind: "work",
      meta: worker("worker-session", "Worker", Some(REPO), Some("implement")),
      overlay: Some("none"),
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "overlay_undeclared",
      kind: "work",
      meta: worker("worker-session", "Worker", Some(REPO), Some("implement")),
      overlay: None,
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "work_implement_with_repo",
      kind: "work",
      meta: worker("worker-session", "Worker", Some(REPO), Some("implement")),
      overlay: None,
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "work_repo_less_implement",
      kind: "work",
      meta: worker("worker-session", "Worker", None, Some("implement")),
      overlay: None,
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "work_type_less",
      kind: "work",
      meta: worker("worker-session", "Worker", Some(REPO), None),
      overlay: None,
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "review_implement_with_repo",
      kind: "review",
      meta: worker("worker-session", "Worker", Some(REPO), Some("implement")),
      overlay: None,
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "verify_task",
      kind: "work",
      meta: worker("worker-session", "Verify", Some(REPO), Some("verify")),
      overlay: None,
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "cold_read_task",
      kind: "work",
      meta: worker("worker-session", "Cold Read", None, Some("cold-read")),
      overlay: None,
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "iteration_implement",
      kind: "iteration",
      meta: worker("worker-session", "Worker", Some(REPO), Some("implement")),
      overlay: None,
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "scheduled_step_quick_edit",
      kind: "scheduled_step",
      meta: worker("worker-session", "Worker", Some(REPO), Some("quick-edit")),
      overlay: None,
      chain: NO_CHAIN,
      node_body: None,
      host: None,
      bodies: &[],
    },
    Case {
      name: "subtree_chain_three_nodes",
      kind: "work",
      meta: worker("leaf-session", "Leaf", Some(REPO), Some("implement")),
      overlay: None,
      chain: THREE_NODES,
      node_body: Some("node_rule"),
      host: None,
      bodies: THREE_BODIES,
    },
    Case {
      name: "node_rule_equals_subtree_rule",
      kind: "work",
      meta: worker("leaf-session", "Leaf", Some(REPO), Some("implement")),
      overlay: None,
      chain: SHARED,
      node_body: Some("shared_rule"),
      host: None,
      bodies: SHARED_BODY,
    },
    Case {
      name: DAMAGED_CASE,
      kind: "manager_turn",
      meta: manager("root-session", "Root"),
      overlay: None,
      chain: NON_ASCII,
      node_body: Some("node_non_ascii"),
      host: Some("主机规则 for YOUR_SESSION_UUID — café ✓ 🚀\r\nsecond line"),
      bodies: NON_ASCII_BODIES,
    },
  ]
}

/// The synthetic turn contribution: it names a workflow file for one session name and adds one segment for another.
struct GoldenContribution;

impl TurnContribution for GoldenContribution {
  fn class_name(&self) -> &'static str {
    "GoldenContribution"
  }

  fn instruction_segments(
    &self,
    meta: &SessionMetadata,
    kind: &str,
    _cfg: &CharlieBotConfig,
    _repo: &RepoRoot,
  ) -> anyhow::Result<Vec<RuleSegment>> {
    if meta.name != "Scoped Contribution" {
      return Ok(Vec::new());
    }
    Ok(vec![RuleSegment {
      text: format!("Synthetic contribution for {kind}."),
      sources: vec![PromptSource {
        scope: "base".to_string(),
        source_ref: "synthetic/contribution".to_string(),
        source_session_id: None,
      }],
      delivery: "full".to_string(),
    }])
  }

  fn workflow_rules_file(&self, meta: &SessionMetadata) -> Option<&'static str> {
    (meta.name == "Thread Brief").then_some("thread_session.md")
  }
}

fn register_golden(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.turn_contribution("golden_synthetic", Arc::new(GoldenContribution))
}

fn registry() -> Registry {
  let mut registry = Registry::new();
  registry.register_package("test.golden_contribution", register_golden).unwrap();
  registry
}

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap().to_path_buf()
}

fn config(home: &Path) -> CharlieBotConfig {
  CharlieBotConfig {
    charliebot_home: home.to_path_buf(),
    headless_chrome_bin: "/bin/false".to_string(),
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

/// One case's home. The home lives as long as the returned guard.
struct Prepared {
  home: TempDir,
}

fn prepare(case: &Case) -> Prepared {
  let home = tempfile::tempdir().expect("temp home");
  if let Some(host) = case.host {
    fs::write(home.path().join("MASTER_AGENT_PROMPT.md"), host).expect("host supplement");
  }
  let bodies_dir = home.path().join("prompt_bodies");
  fs::create_dir_all(&bodies_dir).expect("prompt_bodies");
  for (_name, text) in case.bodies {
    let reference = sha256_hex(text);
    fs::write(bodies_dir.join(format!("{reference}.md")), text).expect("rule body");
  }
  Prepared { home }
}

/// The Rust side of one case: the snapshot the same inputs assemble to, and its `to_json_dict` entry.
fn rust_case(case: &Case, home: &Path, registry: &Registry, checkout: &RepoRoot) -> (Value, PromptSnapshot) {
  let cfg = config(home);
  let mut meta = SessionMetadata::new(case.meta.id, case.meta.name, profile(case.meta.profile));
  meta.task = case.meta.task.map(|task| TaskSpec {
    goal: task.goal.to_string(),
    repo_path: task.repo_path.map(String::from),
    task_type: task.task_type.map(|name| serde_json::from_value::<TaskType>(json!(name)).expect("known task type")),
    cold_read: cold_read_spec(task.task_type),
    ..TaskSpec::default()
  });
  let option = BackendOption {
    id: "golden".to_string(),
    label: "Golden".to_string(),
    model: "golden-model".to_string(),
    prompt_overlay: case.overlay.map(String::from),
    backend_type: "codex".to_string(),
    type_fields: serde_yaml::Mapping::new(),
  };
  let (overlay, declared) = resolve_launch_overlay(&option);
  let chain: Vec<ChainLink> = case
    .chain
    .iter()
    .map(|(session_id, body)| ChainLink {
      session_id: session_id.to_string(),
      subtree_ref: body.map(|name| sha256_hex(text_of(case.bodies, name))),
    })
    .collect();
  let node_ref = case.node_body.map(|name| sha256_hex(text_of(case.bodies, name)));
  let inputs = RuleInputs { overlay: overlay.as_deref(), chain: &chain, node_ref: node_ref.as_deref() };
  let env = PromptEnv { cfg: &cfg, registry, checkout };
  let (segments, overlay_error): (Vec<RuleSegment>, Option<OverlayError>) =
    build_segments(&env, &meta, case.kind, &inputs).expect("the matrix assembles");
  let snapshot = assemble_snapshot(&segments);
  let rendered = json!({
    "name": case.name,
    "declared": declared,
    "overlay_error": overlay_error.as_ref().map(OverlayError::python_class_name),
    "snapshot": snapshot.to_json_dict(),
  });
  (rendered, snapshot)
}

/// The cold-read payload a cold-read task carries; every other task type carries none.
fn cold_read_spec(task_type: Option<&str>) -> Option<ColdReadSpec> {
  (task_type == Some("cold-read")).then(|| ColdReadSpec {
    page_path: "/synthetic/page.html".to_string(),
    genre: "synthetic".to_string(),
    trigger: "synthetic trigger".to_string(),
  })
}

fn profile(name: &str) -> TaskProfile {
  serde_json::from_value(json!(name)).expect("known profile")
}

fn text_of(bodies: &'static [(&'static str, &'static str)], name: &str) -> &'static str {
  bodies.iter().find(|(known, _)| *known == name).map(|(_, text)| *text).expect("body is declared")
}

fn with_field(value: &Value, key: &str, replacement: Value) -> Value {
  let mut changed = value.clone();
  changed[key] = replacement;
  changed
}

fn load_error(value: &Value) -> String {
  PromptSnapshot::from_json_dict(value).expect_err("the damaged snapshot must not load").to_string()
}

/// The three damaged copies of the non-ASCII snapshot and their load errors. The body-ref copy recomputes its hash,
/// so only the body-ref check can fail.
fn damaged_errors(snapshot: &PromptSnapshot) -> Value {
  let good = snapshot.to_json_dict();
  let count = good["char_count"].as_u64().expect("char_count is a number") + 1;
  let mut body_damaged = snapshot.clone();
  body_damaged.blocks[0] = PromptBlock { body_ref: "1".repeat(64), ..body_damaged.blocks[0].clone() };
  json!({
    "hash": load_error(&with_field(&good, "prompt_hash", json!("0".repeat(64)))),
    "char_count": load_error(&with_field(&good, "char_count", json!(count))),
    "body_ref": load_error(&body_damaged.to_json_dict()),
  })
}

#[test]
fn snapshots_match_the_recorded_python_server_output_byte_for_byte() {
  let cases = matrix();
  let prepared: Vec<Prepared> = cases.iter().map(prepare).collect();
  let fixture: Value = serde_json::from_str(PYTHON_FIXTURE).unwrap();
  assert_eq!(fixture["source"], "rust/crates/runtime/tasks/tests/python_prompt_snapshot_golden.py");
  assert_eq!(fixture["base_commit"], "2b212ebd8549c0d3bc86885ae9c30ac648039a0d");
  assert_eq!(fixture["placeholders"]["temp_root"], "$TMPROOT");
  assert_eq!(fixture["placeholders"]["repo_root"], "$REPO");
  assert_eq!(fixture["placeholders"]["prompt_hash"], "$PROMPT_HASH");
  assert_eq!(fixture["placeholders"]["damaged_hash"], "$DAMAGED_HASH");
  let checkout_root = repo_root();
  let mut expected = fixture["expected"].clone();
  for (expected_case, prepared_case) in expected["cases"].as_array_mut().unwrap().iter_mut().zip(&prepared) {
    materialize_paths(expected_case, prepared_case.home.path(), &checkout_root);
    // A block whose text names the checkout hashes that path, so its hash is computed from the materialized text.
    if let Some(blocks) = expected_case.pointer_mut("/snapshot/blocks").and_then(Value::as_array_mut) {
      for block in blocks.iter_mut().filter(|block| block["body_ref"] == "$BODY_REF") {
        block["body_ref"] = json!(sha256_hex(block["text"].as_str().unwrap()));
      }
    }
    if expected_case["snapshot"]["prompt_hash"] == "$PROMPT_HASH" {
      let hash = recorded_prompt_hash(&expected_case["snapshot"]["blocks"]);
      expected_case["snapshot"]["prompt_hash"] = json!(hash);
    }
  }
  let damaged_hash = expected["cases"]
    .as_array()
    .unwrap()
    .iter()
    .find(|case| case["name"] == DAMAGED_CASE)
    .unwrap()["snapshot"]["prompt_hash"]
    .as_str()
    .unwrap();
  let hash_error = expected["from_json_errors"]["hash"].as_str().unwrap().replace("$DAMAGED_HASH", damaged_hash);
  expected["from_json_errors"]["hash"] = json!(hash_error);
  assert_eq!(expected["cases"].as_array().unwrap().len(), cases.len());
  assert!(expected["from_json_errors"]["body_ref"].is_string());

  let registry = registry();
  let checkout = RepoRoot::new(Some(repo_root().into_os_string())).expect("repo root is valid");
  let mut results = Vec::new();
  let mut damaged = None;
  for (case, prepared) in cases.iter().zip(&prepared) {
    let home = prepared.home.path();
    let (rendered, snapshot) = rust_case(case, home, &registry, &checkout);
    if case.name == DAMAGED_CASE {
      damaged = Some(snapshot);
    }
    results.push(rendered);
  }
  let damaged = damaged.expect("the damaged case is in the matrix");
  let rust = json!({"cases": results, "from_json_errors": damaged_errors(&damaged)});

  let rust_text = json_dumps(&rust, &PYTHON_STYLE);
  let expected_text = json_dumps(&expected, &PYTHON_STYLE);
  assert!(rust_text == expected_text, "{}", first_difference(&rust_text, &expected_text));
}

/// Where two renderings first part ways, with the text around that byte on each side. The whole rendering is too long
/// for a CI log to keep, so the window stands in for it.
fn first_difference(actual: &str, recorded: &str) -> String {
  let at = actual.bytes().zip(recorded.bytes()).position(|(left, right)| left != right);
  let at = at.unwrap_or(actual.len().min(recorded.len()));
  let window = |text: &str| {
    let bytes = text.as_bytes();
    String::from_utf8_lossy(&bytes[at.saturating_sub(160)..(at + 160).min(bytes.len())]).into_owned()
  };
  format!(
    "renderings differ at byte {at} (lengths {} and {}):\nactual:   {}\nrecorded: {}",
    actual.len(),
    recorded.len(),
    window(actual),
    window(recorded)
  )
}

fn materialize_paths(value: &mut Value, root: &Path, checkout_root: &Path) {
  match value {
    Value::String(text) => {
      *text = text.replace("$TMPROOT", &root.to_string_lossy()).replace("$REPO", &checkout_root.to_string_lossy());
    }
    Value::Array(values) => values.iter_mut().for_each(|value| materialize_paths(value, root, checkout_root)),
    Value::Object(fields) => fields.values_mut().for_each(|value| materialize_paths(value, root, checkout_root)),
    _ => {}
  }
}

fn recorded_prompt_hash(blocks: &Value) -> String {
  let payload = json!({"blocks": blocks});
  sha256_hex(&json_dumps(&payload, &HASH_STYLE))
}
