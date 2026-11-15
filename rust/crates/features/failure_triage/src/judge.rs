//! The judge: one small-model one-shot per signature, over the members of the pool that `backends.calls.triage`
//! names.
//!
//! The judge runs only through a backend's native one-shot path, which starts no tools. A backend without that path
//! is not a judge candidate, and the judge never falls back to the tool-capable one-shot. It takes its candidates from
//! the pool's shared rotation, as the autonamer does, so it holds no order or cooldown of its own. Each call
//! has a thirty-second cap, a failed call rotates to the next light backend, and an answer that breaks the contract
//! fails only its own signature. The same function serves this CLI and the server-side handler the later task adds.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use charliebot_agents::backend_availability;
use charliebot_agents::backend_pool::{self, BackendPool};
use charliebot_agents::backend_types::{self, BuiltBackend};
use charliebot_core::config::{BackendOption, CallClass, CallTarget, CharlieBotConfig};
use charliebot_registry::Registry;
use charliebot_registry::backend::{Env, LaunchArgs};
use charliebot_store::metadata::UtcDateTime;
use regex::Regex;
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
      prompt.push_str(&format!("### {}\n{}\n\n", candidate.heading, candidate.body));
    }
  }
  prompt
}

/// A judge answer checked against its contract: a `known_pattern` verdict cites one of the candidate headings,
/// and with no candidates it has nothing to cite.
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
      let reference = raw.lessons_ref.as_deref().map(comparable_heading).unwrap_or_default();
      let Some(candidate) = candidates.iter().find(|candidate| comparable_heading(&candidate.heading) == reference)
      else {
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

/// The comparable text of a heading: the leading date or date-plus-session prefix and the markdown decoration a
/// model copies around it removed. The rest must match exactly.
fn comparable_heading(heading: &str) -> String {
  let text = heading.trim();
  let text = text.strip_prefix("###").unwrap_or(text).trim_start();
  let text = text.replace('`', "");
  let text = text.trim_matches(['"', '\'', '*']).trim();
  LEADING_DATE.replace(text, "").trim().to_string()
}

/// A heading's leading date, with an optional `(session …)` part and an optional separator after it.
static LEADING_DATE: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"^\d{4}-\d{2}-\d{2}\b(?:\s*\(session\s+[^)]*\))?\s*(?:—|–|-|:)?\s*")
    .expect("the leading-date expression is valid")
});

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
}

impl<'a> JudgeBackends<'a> {
  /// The judge's dependencies with the thirty-second cap.
  pub fn new(
    cfg: &'a CharlieBotConfig,
    registry: Arc<Registry>,
    base_env: &'a Env,
    pool: Arc<BackendPool>,
  ) -> JudgeBackends<'a> {
    JudgeBackends { cfg, registry, base_env, pool, timeout: JUDGE_TIMEOUT }
  }
}

/// One signature judged on the `backends.calls.triage` target: a call failure moves to the next member the target
/// selects, and a contract failure ends this signature at once. A timed-out member goes into the pool's shared cooldown
/// for [`JUDGE_COOLDOWN`], so a hung member costs one signature instead of every signature.
///
/// The members come from `backend_pool::one_shot_members`, so the pool holds the position and the cooldown table, and
/// the judge keeps neither.
pub async fn judge_signature(backends: &JudgeBackends<'_>, input: &JudgeInput) -> Result<Judgement, JudgeError> {
  let system = system_prompt();
  let prompt = user_prompt(input);
  let target = backends.cfg.backends.calls.get(CallClass::Triage);
  let mut last_error = None;
  for option in backend_pool::one_shot_members(backends.cfg, Arc::clone(&backends.pool), "triage", target) {
    let option = option.map_err(|error| JudgeError::Call(format!("{error:#}")))?;
    match attempt(backends, &option, &prompt, system, input).await {
      Ok(judgement) => return Ok(judgement),
      Err(JudgeError::Assertion(message)) => return Err(JudgeError::Assertion(message)),
      Err(error) => {
        record_member_failure(backends, &option.id, &error)?;
        last_error = Some(error);
      }
    }
  }
  if let Some(error) = last_error {
    return Err(error);
  }
  let reason = match target {
    None => "backends.calls.triage is unset; name a model or a pool to judge failures".to_string(),
    Some(CallTarget::Backend(backend_id)) => backends
      .pool
      .selector(backends.cfg)
      .exhausted_message(backend_id)
      .map_err(|error| JudgeError::Call(format!("{error:#}")))?,
    Some(_) => "backends.calls.triage names no selectable member".to_string(),
  };
  Err(JudgeError::Call(reason))
}

/// Record one failed judge attempt against the member that failed it. A timeout is the judge's own reason; a quota
/// or an access refusal is the shared classification's reason. A response or contract failure records nothing: it is
/// not a member failure.
fn record_member_failure(backends: &JudgeBackends<'_>, backend_id: &str, error: &JudgeError) -> Result<(), JudgeError> {
  let (resets_at, reason) = match error {
    JudgeError::Timeout(_) => {
      (Some(UtcDateTime::now().into_inner() + JUDGE_COOLDOWN), backend_availability::REASON_TIMEOUT)
    }
    JudgeError::Call(_) => {
      let Some(reason) = backend_pool::classify_member_failure(&error.to_string(), None).availability_reason() else {
        return Ok(());
      };
      (None, reason)
    }
    JudgeError::Response(_) | JudgeError::Assertion(_) => return Ok(()),
  };
  let detail = error.to_string();
  backends
    .pool
    .availability()
    .record(backend_id, resets_at.map(UtcDateTime::from), reason, &detail)
    .map_err(|error| JudgeError::Call(format!("{error:#}")))?;
  Ok(())
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

/// One native, tool-less one-shot with the judge's cap. A backend without the native path is an error, not a
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
  /// The judgement of `input`. `Err` marks only this signature judge_failed.
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

  use charliebot_registry::RegistryError;
  use charliebot_registry::backend::{
    AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, ResumeStyle, Translate, TransportKind,
  };
  use futures_util::future::BoxFuture;
  use serde_json::Value;

  use super::*;

  fn candidates(headings: &[&str]) -> Vec<LessonCandidate> {
    headings
      .iter()
      .map(|heading| LessonCandidate { heading: (*heading).to_string(), body: String::new(), score: 1 })
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
  fn a_citation_may_omit_a_leading_date_and_backticks() {
    let candidate = "2026-09-09 — `charliebot delegate` rejects a local base branch that is ahead of its remote; pin \
                     the base with the full commit sha (session 67af29dd)";
    let answer = serde_json::json!({
      "why": "it matches",
      "verdict": "known_pattern",
      "lessons_ref": "charliebot delegate rejects a local base branch that is ahead of its remote; pin the base with \
                      the full commit sha (session 67af29dd)",
      "fix_scope": "",
    })
    .to_string();
    let judgement = evaluate_response(&candidates(&[candidate]), &answer).unwrap();
    assert_eq!(judgement.lessons_ref.as_deref(), Some(candidate));
  }

  #[test]
  fn a_citation_may_omit_a_leading_date_and_session_prefix() {
    let candidate = "2026-10-22 (session 28f1df1f-b557-5c1f-b4a3-d5fd12fccc50) — Seven parallel DeepSeek Flash \
                     audits over the flash service's concurrency cap";
    let answer = serde_json::json!({
      "why": "it matches",
      "verdict": "known_pattern",
      "lessons_ref": "Seven parallel DeepSeek Flash audits over the flash service's concurrency cap",
      "fix_scope": "",
    })
    .to_string();
    let judgement = evaluate_response(&candidates(&[candidate]), &answer).unwrap();
    assert_eq!(judgement.lessons_ref.as_deref(), Some(candidate));
  }

  #[test]
  fn a_citation_that_changes_one_word_still_fails() {
    let answer = serde_json::json!({
      "why": "it matches",
      "verdict": "known_pattern",
      "lessons_ref": "2026-11-01 — scope guards",
      "fix_scope": "",
    })
    .to_string();
    let error = evaluate_response(&candidates(&["2026-11-01 — scope guard"]), &answer).unwrap_err();
    assert!(matches!(error, JudgeError::Assertion(_)), "{error}");
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

  /// A pool whose availability owner reads the empty state of `home` and persists to it.
  fn test_pool(home: &std::path::Path) -> Arc<BackendPool> {
    Arc::new(BackendPool::new(
      Arc::new(backend_availability::BackendAvailability::load(home).unwrap()),
      Arc::new(Registry::new()),
      Arc::new(charliebot_agents::model_pool::RotationPositions::default()),
    ))
  }

  fn config_with_pool(ids: &[&str]) -> CharlieBotConfig {
    use charliebot_core::config::{
      BackendsConfig, PathsConfig, PoolConfig, PoolMember, PoolRule, Sections, ServerConfig, TelegramConfig, UiConfig,
    };

    let options = ids
      .iter()
      .map(|id| BackendOption {
        id: (*id).to_string(),
        label: (*id).to_string(),
        model: String::new(),
        prompt_overlay: None,
        billing: None,
        backend_type: "synthetic".to_string(),
        type_fields: Default::default(),
        visible: true,
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
        public_url: None,
        session_tasks_max: 0,
        min_free_disk_gib: 0,
        enable_session_cpu_weight: true,
        enable_session_io_weight: true,
        enable_local_work_guard: false,
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic".to_string(),
        scratch_dir: "/synthetic".to_string(),
      },
      backends: BackendsConfig {
        pools: vec![PoolConfig {
          id: "light".to_string(),
          label: "Light".to_string(),
          rule: PoolRule::Rotate,
          members: ids.iter().map(|id| PoolMember { backend: (*id).to_string() }).collect(),
          visible: true,
        }],
        routing: None,
        options,
        calls: [(CallClass::Triage, CallTarget::Backend("light".to_string()))].into_iter().collect(),
      },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      remote_run: charliebot_core::config::RemoteRunConfig::default(),
      github: Default::default(),
      sections: Sections::default(),
    }
  }

  fn judge_input() -> JudgeInput {
    JudgeInput {
      signature: "run:work:1:synthetic cause".to_string(),
      kind: "work".to_string(),
      error_text: "synthetic cause".to_string(),
      candidates: Vec::new(),
    }
  }

  #[tokio::test]
  async fn a_judge_call_tries_the_shared_pool_rotation_order() {
    let config = config_with_pool(&["alpha", "beta"]);
    let env = env();
    let home = tempfile::tempdir().unwrap();
    let pool = test_pool(home.path());
    let backends = JudgeBackends {
      cfg: &config,
      registry: Arc::new(Registry::new()),
      base_env: &env,
      pool: Arc::clone(&pool),
      timeout: JUDGE_TIMEOUT,
    };
    // Another shared caller takes the first slot, so the judge must try beta before alpha. No "synthetic" type is
    // registered, so each build fails and the last error names the last member tried. That member is alpha under the
    // shared rotation order, and beta under the pool's own declaration order.
    assert_eq!(pool.selector(&config).select("light", &[]).unwrap().unwrap().id, "alpha");
    let error = judge_signature(&backends, &judge_input()).await.unwrap_err().to_string();
    assert!(error.contains("backend alpha"), "{error}");
  }

  #[tokio::test]
  async fn a_refused_judge_member_moves_to_the_next_member_that_answers() {
    let config = config_with_pool(&["alpha", "beta"]);
    let env = env();
    let home = tempfile::tempdir().unwrap();
    let pool = test_pool(home.path());
    let mut registry = Registry::new();
    registry.register_package("test.judge", register_scripted).unwrap();
    let backends = JudgeBackends {
      cfg: &config,
      registry: Arc::new(registry),
      base_env: &env,
      pool: Arc::clone(&pool),
      timeout: JUDGE_TIMEOUT,
    };

    let judgement = judge_signature(&backends, &judge_input()).await.unwrap();

    assert_eq!(judgement.verdict, Verdict::RealBug, "the next member answers after the refusal");
    let row = pool.availability().row("alpha").expect("the refused member is recorded out");
    assert_eq!(row.reason, backend_availability::REASON_ACCESS_REFUSED);
  }

  #[tokio::test]
  async fn a_refused_judge_member_is_recorded_with_the_refusal_reason() {
    let config = config_with_pool(&["alpha", "beta"]);
    let env = env();
    let home = tempfile::tempdir().unwrap();
    let pool = test_pool(home.path());
    let backends = JudgeBackends {
      cfg: &config,
      registry: Arc::new(Registry::new()),
      base_env: &env,
      pool: Arc::clone(&pool),
      timeout: JUDGE_TIMEOUT,
    };

    record_member_failure(
      &backends,
      "alpha",
      &JudgeError::Call("backend alpha: API Error: 401 Not allowed to access model".to_string()),
    )
    .unwrap();

    let row = pool.availability().row("alpha").expect("the refused member is recorded out");
    assert_eq!(row.reason, backend_availability::REASON_ACCESS_REFUSED);
    assert_eq!(
      pool.selector(&config).select("light", &[]).unwrap().unwrap().id,
      "beta",
      "the next judge attempt skips the refused member"
    );
  }

  #[test]
  fn a_member_cooling_in_the_shared_pool_is_skipped() {
    let config = config_with_pool(&["alpha", "beta"]);
    let env = env();
    let home = tempfile::tempdir().unwrap();
    let pool = test_pool(home.path());
    pool
      .availability()
      .record("alpha", None, backend_availability::REASON_QUOTA_ERROR, "synthetic quota error")
      .unwrap();
    let backends = JudgeBackends {
      cfg: &config,
      registry: Arc::new(Registry::new()),
      base_env: &env,
      pool: Arc::clone(&pool),
      timeout: JUDGE_TIMEOUT,
    };
    let mut tried = Vec::new();
    let mut offered = Vec::new();
    while let Some(option) = backends.pool.selector(backends.cfg).select("light", &tried).unwrap() {
      offered.push(option.id.clone());
      tried.push(option.id);
    }
    assert_eq!(offered, vec!["beta"]);
  }

  struct ScriptedBackend {
    answer: Option<String>,
    error: Option<String>,
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
      if self.answer.is_none() && self.error.is_none() {
        return None;
      }
      let answer = self.answer.clone();
      let error = self.error.clone();
      let delay = self.delay;
      Some(Box::pin(async move {
        if let Some(delay) = delay {
          tokio::time::sleep(delay).await;
        }
        match error {
          Some(error) => Err(anyhow::anyhow!(error)),
          None => Ok(answer.expect("a scripted answer or error is set")),
        }
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
      backend: Box::new(ScriptedBackend { answer: answer.map(str::to_string), error: None, delay }),
      backend_type: "synthetic".to_string(),
      launch: LaunchArgs::default(),
      registry: Arc::new(Registry::new()),
    }
  }

  /// Builds one judge backend per option: `alpha` refuses access, and `beta` answers a real-bug judgement.
  struct JudgeFactory;

  impl BackendFactory for JudgeFactory {
    fn build(
      &self,
      option: Option<&BackendOption>,
      _cfg: &CharlieBotConfig,
      _launch: LaunchArgs,
    ) -> anyhow::Result<Box<dyn AgentBackend>> {
      let id = option.expect("the judge builds a configured backend").id.clone();
      if id == "alpha" {
        return Ok(Box::new(ScriptedBackend {
          answer: None,
          error: Some(
            "Failed to authenticate. API Error: 401 Not allowed to access model due to tags configuration.".to_string(),
          ),
          delay: None,
        }));
      }
      Ok(Box::new(ScriptedBackend {
        answer: Some(
          r#"{"why":"it is a real bug","verdict":"real_bug","lessons_ref":null,"fix_scope":"code"}"#.to_string(),
        ),
        error: None,
        delay: None,
      }))
    }
  }

  fn register_scripted(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.backend_type(BackendTypeRegistration {
      type_name: "synthetic",
      traits: BackendTraits {
        resume: ResumeStyle::NativeId,
        restart_reattach: false,
        preassigned_session_id: false,
        family_prefix: None,
        manual_compact: false,
        compact_hint: None,
      },
      option_model: |_| Ok(()),
      factory: Arc::new(JudgeFactory),
      lifecycle: None,
      translate_fallback: false,
    })
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
      Duration::from_millis(5),
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
