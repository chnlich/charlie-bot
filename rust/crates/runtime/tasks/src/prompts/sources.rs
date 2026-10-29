//! The managed rule sources: the templates, the host supplement, the model overlay, the turn contributions and the local
//! rule bodies, each read fresh, and the per-kind rule segments built from them (`src/runtime/task_prompts.py:218-270`,
//! `:278-395` and `:436-463`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use charliebot_core::pyfmt::str_repr;
use charliebot_store::control_events::sha256_hex;
use charliebot_store::metadata::{SessionMetadata, TaskSpec, TaskType};

use super::{
  DELIVERY_FULL, HOST_SUPPLEMENT_FILENAME, MANAGER_KINDS, MAX_AGENT_MESSAGE_WEIGHTED_CHARS, PROMPT_BODIES_DIR,
  PromptEnv, PromptSource, RuleSegment, SCOPE_BASE, SCOPE_NODE, SCOPE_SUBTREE, TaskPromptError, WORKER_KINDS,
};
use crate::spawner_prompt::{self, TextReadError};
use crate::{cold_read, review_text};

/// The base sections every managed contract starts from (`prompts/task_base.md`).
const COMMON_SECTIONS: [&str; 4] = ["coding_principles", "skills_discovery", "profile_home", "remote_scratch"];

/// The base sections of a worker contract. Workers carry the text-writing rule here; managers carry it as a subsection
/// of `prompts/master.md` instead.
const WORKER_BASE_SECTIONS: [&str; 5] =
  ["coding_principles", "skills_discovery", "profile_home", "remote_scratch", "text_model_reads"];

/// One chain link of the subtree scope: a node from the root down to this node, with its subtree rule ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainLink {
  pub session_id: String,
  pub subtree_ref: Option<String>,
}

/// The launch-time inputs of one assembly pass: the overlay name, the captured chain, and this node's node rule ref.
#[derive(Clone, Copy, Debug)]
pub struct RuleInputs<'a> {
  pub overlay: Option<&'a str>,
  pub chain: &'a [ChainLink],
  pub node_ref: Option<&'a str>,
}

/// A declared model overlay that could not be read. The launch raises an alert with [`Self::python_class_name`] and
/// runs without the overlay.
#[derive(Debug)]
pub struct OverlayError {
  pub path: PathBuf,
  cause: TextReadError,
}

impl OverlayError {
  /// The Python exception class the read raised, by name (`type(error).__name__`).
  pub fn python_class_name(&self) -> &'static str {
    self.cause.python_class_name()
  }
}

/// The task type whose workflow contract a worker prompt renders. A type-less task renders the implement contract.
pub fn prompt_task_type(task: Option<&TaskSpec>) -> TaskType {
  task.and_then(|task| task.task_type).unwrap_or(TaskType::Implement)
}

/// The managed instruction segments of one run, in the order the contract pins (`build_segments`). A cold-read run
/// yields only the cold-reader contract. An undeclared or unreadable overlay returns its error with the segments.
pub fn build_segments(
  env: &PromptEnv<'_>,
  meta: &SessionMetadata,
  kind: &str,
  inputs: &RuleInputs<'_>,
) -> anyhow::Result<(Vec<RuleSegment>, Option<OverlayError>)> {
  let repo = env.checkout.path();
  let mut segments = if MANAGER_KINDS.contains(&kind) {
    manager_rule_segments(env, meta)?
  } else if WORKER_KINDS.contains(&kind) {
    let task_type = prompt_task_type(meta.task.as_ref());
    if task_type == TaskType::ColdRead {
      // The cold-read exception: the cold-reader instructions alone. No overlay, contribution or local rule joins it.
      let (system_prompt, _questions) = cold_read::load_texts(repo)?;
      return Ok((vec![segment(system_prompt, vec![base_source("prompts/cold_read.md")])], None));
    }
    worker_kind_rule_segments(env, kind, meta.task.as_ref())?
  } else {
    return Err(
      TaskPromptError::new(format!("unknown run kind {}: no managed instruction contract", str_repr(kind))).into(),
    );
  };
  let (overlay_segments, overlay_error) = overlay_rule_segments(repo, inputs.overlay);
  segments.extend(overlay_segments);
  segments.extend(env.registry.instruction_segments(meta, kind, env.cfg, env.checkout)?);
  let bodies_dir = env.cfg.charliebot_home.join(PROMPT_BODIES_DIR);
  segments.extend(local_rule_segments(&bodies_dir, inputs.chain, inputs.node_ref, &meta.id)?);
  Ok((segments, overlay_error))
}

/// The path of the host supplement under the home.
pub fn host_supplement_path(env: &PromptEnv<'_>) -> PathBuf {
  env.cfg.charliebot_home.join(HOST_SUPPLEMENT_FILENAME)
}

/// Reads one immutable local rule body and checks that it hashes back to its ref. A missing or corrupt body fails
/// the launch, as a named error.
pub fn read_local_rule_body(
  prompt_bodies_dir: &Path,
  reference: &str,
  owner: &str,
  scope: &str,
) -> anyhow::Result<String> {
  let path = prompt_bodies_dir.join(format!("{reference}.md"));
  let body = read_source_file(&path, &format!("{scope} rule of task {owner}"))?;
  let actual = sha256_hex(&body);
  if actual != reference {
    return Err(
      TaskPromptError::new(format!(
        "{scope} rule of task {owner} is corrupt: prompt_bodies/{reference}.md hashes to {actual}"
      ))
      .into(),
    );
  }
  Ok(body)
}

/// Reads one managed source file. A missing or unreadable file is a named error that says why.
fn read_source_file(path: &Path, what: &str) -> anyhow::Result<String> {
  spawner_prompt::read_text(path).map_err(|error| {
    TaskPromptError::new(format!("{what} is unavailable: {} ({})", path.display(), error.python_message(path))).into()
  })
}

/// The host supplement's text with this task's id filled in, or `None` when the home has no supplement file.
fn host_supplement(env: &PromptEnv<'_>, meta: &SessionMetadata) -> anyhow::Result<Option<String>> {
  let path = host_supplement_path(env);
  // `Path.exists()` answers False for a missing path, or one whose parent is not a directory.
  match fs::metadata(&path) {
    Ok(_) => {}
    Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => return Ok(None),
    Err(error) => return Err(error.into()),
  }
  Ok(Some(read_source_file(&path, "host supplement")?.replace("YOUR_SESSION_UUID", &meta.id)))
}

/// The joined marker sections of one template, in the order the ids list them.
fn sections_text(repo: &Path, filename: &str, section_ids: &[&str]) -> anyhow::Result<String> {
  let sections = spawner_prompt::load_marker_sections(
    &repo.join("prompts").join(filename),
    section_ids,
    &format!("{filename}-sections"),
  )?;
  Ok(section_ids.iter().map(|id| sections[*id].as_str()).collect::<Vec<&str>>().join("\n"))
}

/// The manager contract at any depth: the shared base, the shared manager rules, the workflow rules file a contribution
/// names (default `manager_workflows.md`), the manager template, and the host supplement.
fn manager_rule_segments(env: &PromptEnv<'_>, meta: &SessionMetadata) -> anyhow::Result<Vec<RuleSegment>> {
  let repo = env.checkout.path();
  let base = sections_text(repo, "task_base.md", &COMMON_SECTIONS)?;
  let shared =
    read_source_file(&repo.join("prompts/master.md"), "shared manager rules")?.replace("{{session_id}}", &meta.id);
  let workflows_file = env.registry.workflow_rules_file(meta)?;
  let workflows = read_source_file(&repo.join("prompts").join(workflows_file), "manager workflow rules")?
    .replace("{{session_id}}", &meta.id);
  let contract = sections_text(repo, "task_manager.md", &["manager_role", "manager_boundaries"])?
    .replace("{{agent_message_limit}}", &MAX_AGENT_MESSAGE_WEIGHTED_CHARS.to_string());
  let mut segments = vec![
    segment(base, vec![base_source("prompts/task_base.md")]),
    segment(shared, vec![base_source("prompts/master.md")]),
    segment(workflows, vec![base_source(&format!("prompts/{workflows_file}"))]),
    segment(contract, vec![base_source("prompts/task_manager.md")]),
  ];
  if let Some(host) = host_supplement(env, meta)? {
    let path = host_supplement_path(env);
    segments.push(segment(host, vec![base_source(&path.display().to_string())]));
  }
  Ok(segments)
}

/// The worker contract: the review rules, the verify contract, or the role and the task's workflow.
fn worker_kind_rule_segments(
  env: &PromptEnv<'_>,
  kind: &str,
  task: Option<&TaskSpec>,
) -> anyhow::Result<Vec<RuleSegment>> {
  let repo = env.checkout.path();
  let base = sections_text(repo, "task_base.md", &WORKER_BASE_SECTIONS)?;
  let mut segments = vec![segment(base, vec![base_source("prompts/task_base.md")])];
  if kind == "review" {
    segments
      .push(segment(review_text::review_rules_text(), vec![base_source("src/runtime/review.py:review_rules_text")]));
  } else if prompt_task_type(task) == TaskType::Verify {
    let contract = sections_text(repo, "verify.md", &["preamble", "scope"])?;
    let contract = spawner_prompt::substitute_tokens(&contract, &spawner_prompt::verify_contract_tokens(repo)?);
    segments.push(segment(contract, vec![base_source("prompts/verify.md")]));
  } else {
    let worker = sections_text(repo, "worker.md", &["role"])?;
    // The repo contract drops the bindings section, which renders per run. A repo-less task selects the repo-less
    // workflow and its source-files rule.
    let repo_less = !task.and_then(|task| task.repo_path.as_deref()).is_some_and(|path| !path.is_empty());
    let ids = spawner_prompt::workflow_rule_section_ids(task, repo_less)?;
    let workflow = sections_text(repo, "worker.md", &ids)?;
    segments.push(segment(format!("{worker}\n{workflow}"), vec![base_source("prompts/worker.md")]));
  }
  Ok(segments)
}

/// The overlay's segment, or its read error. An undeclared overlay (`None`) contributes nothing and no error.
fn overlay_rule_segments(repo: &Path, overlay: Option<&str>) -> (Vec<RuleSegment>, Option<OverlayError>) {
  let Some(name) = overlay else {
    return (Vec::new(), None);
  };
  let path = repo.join("prompts/model_overlays").join(format!("{name}.md"));
  match spawner_prompt::read_text(&path) {
    Ok(text) => (vec![segment(text, vec![base_source(&format!("prompts/model_overlays/{name}.md"))])], None),
    Err(cause) => (Vec::new(), Some(OverlayError { path, cause })),
  }
}

/// The subtree rules from the root down to this node, then this node's own node rule. A null ref contributes nothing.
fn local_rule_segments(
  bodies_dir: &Path,
  chain: &[ChainLink],
  node_ref: Option<&str>,
  node_id: &str,
) -> anyhow::Result<Vec<RuleSegment>> {
  let mut segments = Vec::new();
  for link in chain {
    let Some(reference) = link.subtree_ref.as_deref() else {
      continue;
    };
    let text = read_local_rule_body(bodies_dir, reference, &link.session_id, SCOPE_SUBTREE)?;
    segments.push(segment(text, vec![local_source(SCOPE_SUBTREE, reference, &link.session_id)]));
  }
  if let Some(reference) = node_ref {
    let text = read_local_rule_body(bodies_dir, reference, node_id, SCOPE_NODE)?;
    segments.push(segment(text, vec![local_source(SCOPE_NODE, reference, node_id)]));
  }
  Ok(segments)
}

fn segment(text: String, sources: Vec<PromptSource>) -> RuleSegment {
  RuleSegment { text, sources, delivery: DELIVERY_FULL.to_string() }
}

fn base_source(source_ref: &str) -> PromptSource {
  PromptSource { scope: SCOPE_BASE.to_string(), source_ref: source_ref.to_string(), source_session_id: None }
}

fn local_source(scope: &str, reference: &str, owner: &str) -> PromptSource {
  PromptSource {
    scope: scope.to_string(),
    source_ref: format!("{PROMPT_BODIES_DIR}/{reference}.md"),
    source_session_id: Some(owner.to_string()),
  }
}
