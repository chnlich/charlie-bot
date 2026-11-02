//! The judge: one small-model one-shot per signature, over the light backends of `backends.pool`.
//!
//! The judge runs only through a backend's native one-shot path, which starts no tools. A backend without that path
//! is not a judge candidate, and the judge never falls back to the tool-capable one-shot. Each call has a thirty-second
//! cap, a failed call rotates to the next light backend, and an answer that breaks the contract fails only its own
//! signature. The same function serves this CLI and the server-side handler the later task adds.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use charliebot_agents::backend_pool::BackendPool;
use charliebot_agents::backend_types::{self, BuiltBackend};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::Registry;
use charliebot_registry::backend::{Env, LaunchArgs};
use charliebot_store::metadata::UtcDateTime;
use serde::Deserialize;

use crate::lessons::LessonCandidate;

/// The cap on one judge call. A hung call fails its signature instead of holding the batch.
pub const JUDGE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a backend that outlived [`JUDGE_TIMEOUT`] stays at the back of the judge's rotation.
pub const JUDGE_COOLDOWN: time::Duration = time::Duration::minutes(5);

/// The judge contract, read from the repository's prompt file.
pub fn system_prompt() -> &'static str {
  include_str!("../../../../../prompts/failure_triage_judge.md")
}

/// One verdict class.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
  RealBug,
  KnownPattern,
  NeedsDecision,
}

/// What the judge answered for one signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Judgement {
  pub verdict: Verdict,
  pub why: String,
  pub lessons_ref: Option<String>,
  pub fix_scope: String,
}

/// One signature's material for the judge.
#[derive(Clone, Debug)]
pub struct JudgeInput {
  pub signature: String,
  pub kind: String,
  pub error_text: String,
  pub candidates: Vec<LessonCandidate>,
}

/// Why one signature has no judgement.
#[derive(Debug)]
pub enum JudgeError {
  /// The backend could not be built, or the call failed for a reason other than the cap.
  Call(String),
  /// The call outlived the cap. The backend is set aside for the rest of the batch, so one hung member costs one
  /// signature rather than every signature.
  Timeout(String),
  /// The answer was not the JSON schema.
  Response(String),
  /// The answer broke the contract of the verdict classes.
  Assertion(String),
}

impl std::fmt::Display for JudgeError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      JudgeError::Call(message)
      | JudgeError::Timeout(message)
      | JudgeError::Response(message)
      | JudgeError::Assertion(message) => formatter.write_str(message),
    }
  }
}

impl std::error::Error for JudgeError {}

/// The schema the judge answers in: `why` precedes the verdict, and the row keeps `lessons_ref` and `fix_scope`.
#[derive(Debug, Deserialize)]
struct RawJudgement {
  why: String,
  verdict: Verdict,
  #[serde(default)]
  lessons_ref: Option<String>,
  #[serde(default)]
  fix_scope: String,
}

/// The user prompt: the signature, its run kind, its error text and the candidate LESSONS entries.
pub fn user_prompt(input: &JudgeInput) -> String {
  let mut prompt = format!(
    "Signature: {}\nRun kind: {}\nError text:\n{}\n\nCandidate LESSONS entries:\n",
    input.signature, input.kind, input.error_text
  );
  if input.candidates.is_empty() {
    prompt.push_str("(none)\n");
  } else {
    for candidate in &input.candidates {
      prompt.push_str(&format!("### {}\n{}\n\n", candidate.heading, candidate.snippet));
    }
  }
  prompt
}

/// Read a judge answer and assert its contract: a `known_pattern` verdict cites one of the candidate headings, and
/// with no candidates it has nothing to cite.
pub fn evaluate_response(candidates: &[LessonCandidate], answer: &str) -> Result<Judgement, JudgeError> {
  let text = strip_fences(answer.trim());
  let raw: RawJudgement = serde_json::from_str(text)
    .map_err(|error| JudgeError::Response(format!("the judge answer is not the schema ({error}): {answer}")))?;
  let lessons_ref = match raw.verdict {
    Verdict::KnownPattern => {
      if candidates.is_empty() {
        return Err(JudgeError::Assertion(
          "known_pattern names no candidate heading, and the scan found no candidate".to_string(),
        ));
      }
      let reference = raw.lessons_ref.as_deref().map(cited_heading).unwrap_or_default();
      let Some(candidate) = candidates.iter().find(|candidate| candidate.heading == reference) else {
        return Err(JudgeError::Assertion(format!(
          "lessons_ref {:?} is not a candidate heading; the candidates are {}",
          raw.lessons_ref.as_deref().unwrap_or_default(),
          headings(candidates)
        )));
      };
      Some(candidate.heading.clone())
    }
    Verdict::RealBug | Verdict::NeedsDecision => raw.lessons_ref.clone(),
  };
  Ok(Judgement { verdict: raw.verdict, why: raw.why, lessons_ref, fix_scope: raw.fix_scope })
}

/// The heading a `lessons_ref` cites, with the markdown decoration a model copies around it removed. The reference
/// still has to name a candidate: this undoes `###`, backticks, quotes and asterisks, not a different string.
fn cited_heading(reference: &str) -> &str {
  let text = reference.trim();
  let text = text.strip_prefix("###").unwrap_or(text).trim_start();
  text.trim_matches(['`', '"', '\'', '*']).trim()
}

fn headings(candidates: &[LessonCandidate]) -> String {
  let listed: Vec<String> = candidates.iter().map(|candidate| format!("{:?}", candidate.heading)).collect();
  format!("[{}]", listed.join(", "))
}

/// The judge's dependencies: the config and pool that rotate, the registry that builds each backend, and the
/// environment its child inherits.
pub struct JudgeBackends<'a> {
  pub cfg: &'a CharlieBotConfig,
  pub registry: Arc<Registry>,
  pub base_env: &'a Env,
  pub pool: Arc<BackendPool>,
  pub timeout: Duration,
  /// The backend that answered last. A batch tries it first, so a slow pool member costs one signature, not each.
  preferred: std::sync::Mutex<Option<String>>,
}

impl<'a> JudgeBackends<'a> {
  /// The judge's dependencies with the thirty-second cap.
  pub fn new(
    cfg: &'a CharlieBotConfig,
    registry: Arc<Registry>,
    base_env: &'a Env,
    pool: Arc<BackendPool>,
  ) -> JudgeBackends<'a> {
    JudgeBackends { cfg, registry, base_env, pool, timeout: JUDGE_TIMEOUT, preferred: std::sync::Mutex::new(None) }
  }

  fn preferred(&self) -> Option<String> {
    self.preferred.lock().expect("the judge preference lock is not poisoned").clone()
  }

  fn note_preference(&self, backend_id: &str) {
    *self.preferred.lock().expect("the judge preference lock is not poisoned") = Some(backend_id.to_string());
  }

  fn clear_preference(&self) {
    *self.preferred.lock().expect("the judge preference lock is not poisoned") = None;
  }
}

/// The pool members in the order one signature tries them: the backend that answered last first, then the members the
/// pool has not set aside, then the cooled ones. Cooling only moves a member back, so a signature whose first choices
/// are all slow still reaches a call instead of failing with no backend.
fn candidate_options(backends: &JudgeBackends<'_>) -> Result<Vec<BackendOption>, JudgeError> {
  let preferred = backends.preferred();
  let mut options = Vec::with_capacity(backends.cfg.backends.pool.len());
  for member in &backends.cfg.backends.pool {
    let option = backends
      .cfg
      .get_backend_option(member)
      .ok_or_else(|| JudgeError::Call(format!("backends.pool entry backend '{member}' is not in backends.options")))?;
    options.push(option.clone());
  }
  options.sort_by_key(|option| (is_cooling(backends, &option.id), Some(option.id.as_str()) != preferred.as_deref()));
  Ok(options)
}

/// Judge one signature over the light backends of the pool. The backend that answered last is tried first; a call
/// failure rotates to the next backend, and a contract failure ends this signature at once. A timed-out backend moves
/// to the back of the rotation for [`JUDGE_COOLDOWN`], so a hung pool member costs one signature instead of every
/// signature.
pub async fn judge_signature(backends: &JudgeBackends<'_>, input: &JudgeInput) -> Result<Judgement, JudgeError> {
  let system = system_prompt();
  let prompt = user_prompt(input);
  let mut last_error = None;
  for option in candidate_options(backends)? {
    match attempt(backends, &option, &prompt, system, input).await {
      Ok(judgement) => {
        backends.note_preference(&option.id);
        return Ok(judgement);
      }
      Err(JudgeError::Assertion(message)) => return Err(JudgeError::Assertion(message)),
      Err(error) => {
        if let JudgeError::Timeout(_) = error {
          let until = UtcDateTime::now().into_inner() + JUDGE_COOLDOWN;
          backends.pool.cool(&option.id, Some(UtcDateTime::from(until)));
        }
        if backends.preferred().as_deref() == Some(option.id.as_str()) {
          backends.clear_preference();
        }
        last_error = Some(error);
      }
    }
  }
  Err(last_error.unwrap_or_else(|| JudgeError::Call("backends.pool is empty".to_string())))
}

/// Whether the pool already set this backend aside.
fn is_cooling(backends: &JudgeBackends<'_>, backend_id: &str) -> bool {
  backends.pool.cooling_until(backend_id).is_some_and(|until| until > UtcDateTime::now().into_inner())
}

/// One backend's attempt: build it, run its native one-shot, and read the answer.
async fn attempt(
  backends: &JudgeBackends<'_>,
  option: &BackendOption,
  prompt: &str,
  system: &str,
  input: &JudgeInput,
) -> Result<Judgement, JudgeError> {
  let backend_id = option.id.clone();
  let built = backend_types::build_backend(Arc::clone(&backends.registry), option, backends.cfg, LaunchArgs::default())
    .map_err(|error| JudgeError::Call(format!("backend {backend_id} could not be built: {error:#}")))?;
  let answer =
    native_one_shot_text(built, backends.base_env, prompt, system, backends.timeout).await.map_err(|error| {
      let message = format!("backend {backend_id}: {error}");
      if error.timed_out() { JudgeError::Timeout(message) } else { JudgeError::Call(message) }
    })?;
  evaluate_response(&input.candidates, &answer).map_err(|error| match error {
    JudgeError::Assertion(message) => JudgeError::Assertion(format!("backend {backend_id}: {message}")),
    other => other,
  })
}

/// Why one native one-shot produced no answer.
#[derive(Debug)]
pub struct NativeOneShotError {
  message: String,
  timed_out: bool,
}

impl NativeOneShotError {
  fn other(message: impl Into<String>) -> NativeOneShotError {
    NativeOneShotError { message: message.into(), timed_out: false }
  }

  fn timeout(message: impl Into<String>) -> NativeOneShotError {
    NativeOneShotError { message: message.into(), timed_out: true }
  }

  /// Whether the judge's cap ended the call.
  pub fn timed_out(&self) -> bool {
    self.timed_out
  }
}

impl std::fmt::Display for NativeOneShotError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(&self.message)
  }
}

impl std::error::Error for NativeOneShotError {}

/// Run one native, tool-less one-shot with the judge's cap. A backend without the native path is an error, not a
/// fallback to the tool-capable one-shot.
pub async fn native_one_shot_text(
  mut built: BuiltBackend,
  base_env: &Env,
  prompt: &str,
  system: &str,
  timeout: Duration,
) -> Result<String, NativeOneShotError> {
  let Some(call) = built.backend.one_shot_text(base_env, prompt, system, Some(timeout)) else {
    return Err(NativeOneShotError::other("the backend has no native one-shot path"));
  };
  match tokio::time::timeout(timeout, call).await {
    Ok(Ok(answer)) => Ok(answer),
    Ok(Err(error)) => Err(NativeOneShotError::other(format!("{error:#}"))),
    Err(error) => Err(NativeOneShotError::timeout(format!("the judge call timed out after {timeout:?}: {error}"))),
  }
}

/// One judge call for one signature, as the scan drives it. It takes `&self` because a scan keeps several calls in
/// flight; each call's state lives in [`Judgement`] or in the caller.
pub trait JudgeRunner {
  /// Judge `input`. `Err` marks only this signature judge_failed.
  fn judge<'a>(&'a self, input: JudgeInput) -> Pin<Box<dyn Future<Output = Result<Judgement, JudgeError>> + 'a>>;
}

/// The production judge over [`judge_signature`].
pub struct BackendJudge<'a> {
  pub backends: JudgeBackends<'a>,
}

impl<'a> BackendJudge<'a> {
  pub fn new(backends: JudgeBackends<'a>) -> BackendJudge<'a> {
    BackendJudge { backends }
  }
}

impl JudgeRunner for BackendJudge<'_> {
  fn judge<'a>(&'a self, input: JudgeInput) -> Pin<Box<dyn Future<Output = Result<Judgement, JudgeError>> + 'a>> {
    Box::pin(async move { judge_signature(&self.backends, &input).await })
  }
}

fn strip_fences(text: &str) -> &str {
  let Some(rest) = text.strip_prefix("```") else { return text };
  let Some((_label, body)) = rest.split_once('\n') else { return text };
  let Some(body) = body.strip_suffix("```") else { return text };
  body.trim()
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use charliebot_registry::backend::{AgentBackend, Translate, TransportKind};
  use futures_util::future::BoxFuture;
  use serde_json::Value;

  use super::*;

  fn candidates(headings: &[&str]) -> Vec<LessonCandidate> {
    headings
      .iter()
      .map(|heading| LessonCandidate { heading: (*heading).to_string(), snippet: String::new(), score: 1 })
      .collect()
  }

  #[test]
  fn a_known_pattern_must_cite_one_of_the_candidate_headings() {
    let answer = r#"{"why":"it matches","verdict":"known_pattern","lessons_ref":"2026-10-01 — other","fix_scope":""}"#;
    let error = evaluate_response(&candidates(&["2026-11-01 — scope guard"]), answer).unwrap_err();
    assert!(matches!(error, JudgeError::Assertion(_)), "{error}");
    assert!(error.to_string().contains("is not a candidate heading"));
  }

  #[test]
  fn a_citation_wrapped_in_markdown_names_the_candidate_heading() {
    let answer = serde_json::json!({
      "why": "it matches",
      "verdict": "known_pattern",
      "lessons_ref": "### `2026-11-01 — scope guard`",
      "fix_scope": "",
    })
    .to_string();
    let judgement = evaluate_response(&candidates(&["2026-11-01 — scope guard"]), &answer).unwrap();
    assert_eq!(judgement.lessons_ref.as_deref(), Some("2026-11-01 — scope guard"));
  }

  #[test]
  fn known_pattern_with_no_candidates_is_invalid() {
    let answer =
      r#"{"why":"it matches","verdict":"known_pattern","lessons_ref":"2026-11-01 — scope guard","fix_scope":""}"#;
    let error = evaluate_response(&[], answer).unwrap_err();
    assert!(matches!(error, JudgeError::Assertion(_)), "{error}");
    assert!(error.to_string().contains("found no candidate"));
  }

  #[test]
  fn a_cited_candidate_passes_and_fences_are_read() {
    let answer = concat!(
      "```json\n",
      r#"{"why":"a known scope guard","verdict":"known_pattern","lessons_ref":"2026-11-01 — scope guard","fix_scope":""}"#,
      "\n```"
    );
    let judgement = evaluate_response(&candidates(&["2026-11-01 — scope guard"]), answer).unwrap();
    assert_eq!(judgement.verdict, Verdict::KnownPattern);
    assert_eq!(judgement.lessons_ref.as_deref(), Some("2026-11-01 — scope guard"));
  }

  fn config_with_pool(ids: &[&str]) -> CharlieBotConfig {
    use charliebot_core::config::{BackendsConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig};

    let options = ids
      .iter()
      .map(|id| BackendOption {
        id: (*id).to_string(),
        label: (*id).to_string(),
        model: String::new(),
        prompt_overlay: None,
        backend_type: "synthetic".to_string(),
        type_fields: Default::default(),
      })
      .collect();
    CharlieBotConfig {
      charliebot_home: std::path::PathBuf::from("/synthetic/home"),
      headless_chrome_bin: "/synthetic/chrome".to_string(),
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
        worktree_dir: "/synthetic".to_string(),
        scratch_dir: "/synthetic".to_string(),
      },
      backends: BackendsConfig { pool: ids.iter().map(|id| (*id).to_string()).collect(), routing: None, options },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      sections: Sections::default(),
    }
  }

  #[test]
  fn a_pool_that_is_all_cooling_still_offers_every_member_with_the_answerer_first() {
    let config = config_with_pool(&["alpha", "beta", "gamma"]);
    let env = env();
    let pool = Arc::new(BackendPool::default());
    pool.cool("alpha", None);
    pool.cool("beta", None);
    let backends = JudgeBackends {
      cfg: &config,
      registry: Arc::new(Registry::new()),
      base_env: &env,
      pool,
      timeout: JUDGE_TIMEOUT,
      preferred: std::sync::Mutex::new(Some("gamma".to_string())),
    };
    let ids: Vec<String> = candidate_options(&backends).unwrap().into_iter().map(|option| option.id).collect();
    assert_eq!(ids, vec!["gamma", "alpha", "beta"]);

    *backends.preferred.lock().unwrap() = Some("beta".to_string());
    let ids: Vec<String> = candidate_options(&backends).unwrap().into_iter().map(|option| option.id).collect();
    assert_eq!(ids, vec!["gamma", "beta", "alpha"]);
  }

  struct ScriptedBackend {
    answer: Option<String>,
    delay: Option<Duration>,
  }

  impl AgentBackend for ScriptedBackend {
    fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
      None
    }

    fn prepare_transport(&mut self, _log_dir: &Path) -> anyhow::Result<()> {
      Ok(())
    }

    fn build_command(&self, _prompt: &str) -> anyhow::Result<Vec<String>> {
      Ok(Vec::new())
    }

    fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
      Ok(env)
    }

    fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
      None
    }

    fn translator(&self) -> Box<dyn Translate> {
      Box::new(IdentityTranslate)
    }

    fn transport(&self) -> TransportKind {
      TransportKind::RawLog
    }

    fn one_shot_text<'a>(
      &'a mut self,
      _base_env: &'a Env,
      _prompt: &'a str,
      _system: &'a str,
      _timeout: Option<Duration>,
    ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
      let answer = self.answer.clone()?;
      let delay = self.delay;
      Some(Box::pin(async move {
        if let Some(delay) = delay {
          tokio::time::sleep(delay).await;
        }
        Ok(answer)
      }))
    }
  }

  struct IdentityTranslate;

  impl Translate for IdentityTranslate {
    fn translate(&mut self, event: Value) -> anyhow::Result<Vec<Value>> {
      Ok(vec![event])
    }
  }

  fn built(answer: Option<&str>, delay: Option<Duration>) -> BuiltBackend {
    BuiltBackend {
      backend: Box::new(ScriptedBackend { answer: answer.map(str::to_string), delay }),
      backend_type: "synthetic".to_string(),
      launch: LaunchArgs::default(),
      registry: Arc::new(Registry::new()),
    }
  }

  fn env() -> Env {
    Env::from([("PATH".to_string(), "/bin:/usr/bin".to_string())])
  }

  #[tokio::test]
  async fn a_backend_without_a_native_one_shot_fails_the_judge() {
    let error = native_one_shot_text(built(None, None), &env(), "prompt", "system", JUDGE_TIMEOUT).await.unwrap_err();
    assert!(!error.timed_out(), "{error}");
    assert!(error.to_string().contains("no native one-shot path"), "{error}");
  }

  #[tokio::test]
  async fn a_judge_call_that_outlives_the_cap_times_out() {
    let error = native_one_shot_text(
      built(Some("answer"), Some(Duration::from_secs(5))),
      &env(),
      "prompt",
      "system",
      Duration::from_millis(50),
    )
    .await
    .unwrap_err();
    assert!(error.timed_out(), "{error}");
    assert!(error.to_string().contains("timed out"), "{error}");
  }

  #[tokio::test]
  async fn a_native_answer_returns_inside_the_cap() {
    let answer =
      native_one_shot_text(built(Some("answer"), None), &env(), "prompt", "system", JUDGE_TIMEOUT).await.unwrap();
    assert_eq!(answer, "answer");
  }
}
