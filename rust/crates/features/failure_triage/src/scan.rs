//! The stateless scan: collect signatures, decide no-cause records in code, retrieve LESSONS candidates and judge the
//! rest, one verdict per signature.
//!
//! A scan writes nothing. The ledger, the cron handler and the pass-on to a session belong to the later task.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use charliebot_core::config::CharlieBotConfig;
use futures_util::stream::StreamExt;
use serde::Serialize;

use crate::collect::{CollectedSignature, ScanOptions, collect, require_log_dir};
use crate::judge::{JudgeError, JudgeInput, JudgeRunner, Judgement, Verdict};
use crate::lessons::{LESSONS_FILE_NAME, LessonIndex};
use crate::signature::NoCauseShape;

/// The fix scope of a no-cause record: its exit path must record the error text.
pub const NO_CAUSE_FIX_SCOPE: &str = "The exit path must record its error text.";

/// One signature's verdict, as the dry run prints it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowVerdict {
  RealBug,
  KnownPattern,
  NeedsDecision,
  JudgeFailed,
}

impl From<Verdict> for RowVerdict {
  fn from(verdict: Verdict) -> RowVerdict {
    match verdict {
      Verdict::RealBug => RowVerdict::RealBug,
      Verdict::KnownPattern => RowVerdict::KnownPattern,
      Verdict::NeedsDecision => RowVerdict::NeedsDecision,
    }
  }
}

/// One JSON object per signature, in the dry run's order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ScanRow {
  pub signature: String,
  pub source: &'static str,
  pub count: usize,
  pub verdict: RowVerdict,
  pub decided_by: Option<&'static str>,
  pub lessons_ref: Option<String>,
  pub reason: String,
  pub fix_scope: String,
  pub examples: Vec<String>,
  pub example_error: String,
}

impl ScanRow {
  fn code(group: &CollectedSignature, shape: NoCauseShape) -> ScanRow {
    ScanRow {
      signature: group.signature.clone(),
      source: group.source.as_str(),
      count: group.count,
      verdict: RowVerdict::RealBug,
      decided_by: Some("code"),
      lessons_ref: None,
      reason: shape.reason().to_string(),
      fix_scope: NO_CAUSE_FIX_SCOPE.to_string(),
      examples: group.examples.clone(),
      example_error: group.example_error.clone(),
    }
  }

  fn judged(group: &CollectedSignature, judgement: Judgement) -> ScanRow {
    ScanRow {
      signature: group.signature.clone(),
      source: group.source.as_str(),
      count: group.count,
      verdict: judgement.verdict.into(),
      decided_by: Some("judge"),
      lessons_ref: judgement.lessons_ref,
      reason: judgement.why,
      fix_scope: judgement.fix_scope,
      examples: group.examples.clone(),
      example_error: group.example_error.clone(),
    }
  }

  fn judge_failed(group: &CollectedSignature, error: &JudgeError) -> ScanRow {
    ScanRow {
      signature: group.signature.clone(),
      source: group.source.as_str(),
      count: group.count,
      verdict: RowVerdict::JudgeFailed,
      decided_by: None,
      lessons_ref: None,
      reason: error.to_string(),
      fix_scope: String::new(),
      examples: group.examples.clone(),
      example_error: group.example_error.clone(),
    }
  }
}

/// Preflight: the scan's mechanisms must be present before it reads anything. Each failure names what is missing.
pub fn preflight(cfg: &CharlieBotConfig, log_dir: &Path) -> Result<()> {
  match std::process::Command::new("rg")
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
  {
    Ok(status) if status.success() => {}
    _ => bail!("rg is not available: install ripgrep so failure triage can rank LESSONS entries"),
  }
  if cfg.backends.pool.is_empty() {
    bail!("backends.pool is empty: failure triage needs a light backend to judge signatures with");
  }
  require_log_dir(log_dir)
}

/// How many judge calls a scan keeps in flight. Three fills the light pool without stacking calls on one gateway.
pub const JUDGE_CONCURRENCY: usize = 3;

/// Run one scan over the window and print nothing. `judge` decides every signature that has a cause.
pub async fn scan(options: &ScanOptions, judge: &dyn JudgeRunner) -> Result<Vec<ScanRow>> {
  let lessons_path = options.home.join(LESSONS_FILE_NAME);
  let lessons = LessonIndex::load(&lessons_path)
    .with_context(|| format!("loading the LESSONS entries for {}", lessons_path.display()))?;
  let groups = collect(options)?;
  let mut rows = Vec::with_capacity(groups.len());
  let mut judgeable = Vec::new();
  for group in groups {
    let Some(shape) = group.no_cause else {
      let candidates = lessons.retrieve(&group.cause_line)?;
      let input = JudgeInput {
        signature: group.signature.clone(),
        kind: group.kind.clone(),
        error_text: group.example_error.clone(),
        candidates,
      };
      judgeable.push((group, input));
      continue;
    };
    rows.push(ScanRow::code(&group, shape));
  }
  let jobs = judgeable.into_iter().map(|(group, input)| async move {
    let result = judge.judge(input).await;
    (group, result)
  });
  let judged: Vec<_> = futures_util::stream::iter(jobs).buffer_unordered(JUDGE_CONCURRENCY).collect().await;
  for (group, result) in judged {
    rows.push(match result {
      Ok(judgement) => ScanRow::judged(&group, judgement),
      Err(error) => ScanRow::judge_failed(&group, &error),
    });
  }
  rows.sort_by(|left, right| left.source.cmp(right.source).then(left.signature.cmp(&right.signature)));
  Ok(rows)
}

/// The ledger a non-dry-run scan would write. The later task adds it; naming it here keeps the refusal specific.
pub fn ledger_path(home: &Path) -> PathBuf {
  home.join("state").join("failure_triage").join("state.json")
}

#[cfg(test)]
mod tests {
  use std::cell::{Cell, RefCell};
  use std::collections::HashMap;
  use std::fs;
  use std::future::Future;
  use std::io::Write;
  use std::pin::Pin;
  use std::time::SystemTime;

  use jiff::Timestamp;

  use super::*;
  use crate::judge::Judgement;

  struct JudgeScript {
    results: RefCell<HashMap<String, Result<Judgement, JudgeError>>>,
    calls: Cell<usize>,
  }

  impl JudgeRunner for JudgeScript {
    fn judge<'a>(&'a self, input: JudgeInput) -> Pin<Box<dyn Future<Output = Result<Judgement, JudgeError>> + 'a>> {
      self.calls.set(self.calls.get() + 1);
      let result = self
        .results
        .borrow_mut()
        .remove(&input.signature)
        .unwrap_or_else(|| Err(JudgeError::Call("no scripted result".to_string())));
      Box::pin(async move { result })
    }
  }

  fn judgement(verdict: Verdict, lessons_ref: Option<&str>) -> Judgement {
    Judgement {
      verdict,
      why: "synthetic reason".to_string(),
      lessons_ref: lessons_ref.map(str::to_string),
      fix_scope: "synthetic scope".to_string(),
    }
  }

  struct Fixture {
    root: tempfile::TempDir,
    home: PathBuf,
    log_dir: PathBuf,
  }

  impl Fixture {
    fn new() -> Fixture {
      let root = tempfile::tempdir().unwrap();
      let home = root.path().join("home");
      let log_dir = root.path().join("logs");
      fs::create_dir_all(&home).unwrap();
      fs::create_dir_all(&log_dir).unwrap();
      fs::write(
        home.join(LESSONS_FILE_NAME),
        "# LESSONS\n\n## 2026-11-01 — synthetic scope guard\n\
         The improve runner reports `the previous round still runs: unit improve-ux.scope exists`.\n",
      )
      .unwrap();
      Fixture { root, home, log_dir }
    }

    fn options(&self) -> ScanOptions {
      crate::collect::ScanOptions::new(
        self.home.clone(),
        self.log_dir.clone(),
        Some("2026-10-18T00:00:00-07:00"),
        Some("2026-11-02T00:00:00-08:00"),
      )
      .unwrap()
    }

    fn add_run(&self, session: &str, run_id: &str, exit_code: i32, backend: &str, events: &str, stderr: &str) {
      let run_dir = self.home.join("sessions").join(session).join("data").join("runs").join(run_id);
      fs::create_dir_all(&run_dir).unwrap();
      let metadata = format!(r#"{{"id":"{run_id}","kind":"work","exit_code":{exit_code},"backend":"{backend}"}}"#);
      fs::write(run_dir.join("metadata.json"), metadata).unwrap();
      if !events.is_empty() {
        fs::write(run_dir.join("events.jsonl"), events).unwrap();
      }
      if !stderr.is_empty() {
        fs::write(run_dir.join("agent.stderr.log"), stderr).unwrap();
      }
      let chat_path = self.home.join("sessions").join(session).join("data").join("chat_events.jsonl");
      let mut chat = fs::OpenOptions::new().create(true).append(true).open(chat_path).unwrap();
      writeln!(chat, r#"{{"type":"run_finished","run_id":"{run_id}","outcome":"failed"}}"#).unwrap();
      let modified: SystemTime = "2026-10-25T12:00:00Z".parse::<Timestamp>().unwrap().into();
      fs::File::options().write(true).open(run_dir.join("metadata.json")).unwrap().set_modified(modified).unwrap();
    }

    fn root(&self) -> &std::path::Path {
      self.root.path()
    }
  }

  fn error_event(text: &str) -> String {
    format!("{{\"type\":\"error\",\"content\":{}}}\n", serde_json::json!(text))
  }

  #[tokio::test]
  async fn the_two_improve_ux_exit_five_texts_are_one_signature_and_reach_the_judge() {
    let fixture = Fixture::new();
    let text = "the previous round still runs: unit improve-ux.scope exists";
    fixture.add_run("session", "run-a", 5, "synthetic", &error_event(text), "");
    fixture.add_run("session", "run-b", 5, "synthetic", &error_event(text), "");
    let signature = "run:work:5:the previous round still runs: unit improve-ux.scope exists".to_string();
    let judge = JudgeScript {
      results: RefCell::new(HashMap::from([(
        signature.clone(),
        Ok(judgement(Verdict::KnownPattern, Some("2026-11-01 — synthetic scope guard"))),
      )])),
      calls: Cell::new(0),
    };
    let rows = scan(&fixture.options(), &judge).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].signature, signature);
    assert_eq!(rows[0].count, 2);
    assert_eq!(rows[0].verdict, RowVerdict::KnownPattern);
    assert_eq!(rows[0].decided_by, Some("judge"));
    assert_eq!(rows[0].lessons_ref.as_deref(), Some("2026-11-01 — synthetic scope guard"));
    assert_eq!(judge.calls.get(), 1);
  }

  #[tokio::test]
  async fn a_missing_yaml_frontmatter_skill_load_error_has_its_own_signature() {
    let fixture = Fixture::new();
    fixture.add_run(
      "session",
      "run-a",
      5,
      "synthetic",
      &error_event("the previous round still runs: unit improve-ux.scope exists"),
      "",
    );
    fixture.add_run(
      "session",
      "run-b",
      1,
      "synthetic",
      "",
      "2026-10-19T06:09:24Z ERROR codex_core::session: failed to load skill /home/synthetic/skills/a/SKILL.md: \
       missing YAML frontmatter delimited by ---\n",
    );
    let judge = JudgeScript { results: RefCell::new(HashMap::new()), calls: Cell::new(0) };
    let rows = scan(&fixture.options(), &judge).await.unwrap();
    let signatures: Vec<&str> = rows.iter().map(|row| row.signature.as_str()).collect();
    assert!(signatures.contains(&"run:work:1:failed to load skill <path>: missing YAML frontmatter"), "{signatures:?}");
    assert!(
      signatures.iter().any(|signature| signature.starts_with("run:work:5:the previous round")),
      "{signatures:?}"
    );
  }

  #[tokio::test]
  async fn the_four_no_cause_shapes_are_real_bugs_decided_in_code_without_a_call() {
    let fixture = Fixture::new();
    fixture.add_run("session", "run-a", 1, "synthetic", &error_event("\u{1b}[0m   "), "");
    fixture.add_run("session", "run-b", 2, "synthetic", &error_event("Provider List: https://docs.litellm.ai/x"), "");
    fixture.add_run(
      "session",
      "run-c",
      3,
      "synthetic",
      "{\"type\":\"result\",\"terminal_reason\":\"api_error\"}\n",
      "",
    );
    fixture.add_run("session", "run-d", 4, "synthetic", "", "");
    let judge = JudgeScript { results: RefCell::new(HashMap::new()), calls: Cell::new(0) };
    let rows = scan(&fixture.options(), &judge).await.unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(judge.calls.get(), 0);
    for row in &rows {
      assert_eq!(row.verdict, RowVerdict::RealBug, "{row:?}");
      assert_eq!(row.decided_by, Some("code"), "{row:?}");
      assert_eq!(row.fix_scope, NO_CAUSE_FIX_SCOPE);
    }
  }

  #[tokio::test]
  async fn each_judge_failure_marks_only_its_own_signature() {
    let fixture = Fixture::new();
    let causes = [
      "the previous round still runs: unit improve-ux.scope exists",
      "No module named 'mcp'",
      "RateLimitError: rate limit exceeded",
      "failed to load skill /home/synthetic/skills/a/SKILL.md: missing YAML frontmatter",
    ];
    for (index, cause) in causes.iter().enumerate() {
      let events = if cause.contains("failed to load skill") { String::new() } else { error_event(cause) };
      let stderr = if cause.contains("failed to load skill") { format!("{cause}\n") } else { String::new() };
      fixture.add_run("session", &format!("run-{index}"), 1, "synthetic", &events, &stderr);
    }
    let judge = JudgeScript {
      results: RefCell::new(HashMap::from([
        (
          "run:work:1:the previous round still runs: unit improve-ux.scope exists".to_string(),
          Ok(judgement(Verdict::RealBug, None)),
        ),
        (
          "run:work:1:No module named 'mcp'".to_string(),
          Err(JudgeError::Assertion("lessons_ref is not a candidate heading".to_string())),
        ),
        (
          "run:work:1:RateLimitError: rate limit exceeded".to_string(),
          Err(JudgeError::Timeout("the judge call timed out after 30s".to_string())),
        ),
        (
          "run:work:1:failed to load skill <path>: missing YAML frontmatter".to_string(),
          Err(JudgeError::Call("backend synthetic: the backend has no native one-shot path".to_string())),
        ),
      ])),
      calls: Cell::new(0),
    };
    let rows = scan(&fixture.options(), &judge).await.unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows.iter().filter(|row| row.verdict == RowVerdict::JudgeFailed).count(), 3);
    let real_bug: Vec<&ScanRow> = rows.iter().filter(|row| row.verdict == RowVerdict::RealBug).collect();
    assert_eq!(real_bug.len(), 1);
    assert_eq!(real_bug[0].signature, "run:work:1:the previous round still runs: unit improve-ux.scope exists");
  }

  #[test]
  fn preflight_names_rg_the_pool_and_the_log_directory() {
    let fixture = Fixture::new();
    let config = synthetic_config(Vec::new());
    let error = preflight(&config, &fixture.log_dir).unwrap_err();
    assert!(error.to_string().contains("backends.pool"), "{error}");
    let config = synthetic_config(vec!["synthetic".to_string()]);
    let missing = fixture.root().join("no-logs");
    let error = preflight(&config, &missing).unwrap_err();
    assert!(error.to_string().contains("log directory"), "{error}");
    preflight(&config, &fixture.log_dir).unwrap();
  }

  fn synthetic_config(pool: Vec<String>) -> CharlieBotConfig {
    use charliebot_core::config::{BackendsConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig};
    CharlieBotConfig {
      charliebot_home: PathBuf::from("/synthetic/home"),
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
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic".to_string(),
        scratch_dir: "/synthetic".to_string(),
      },
      backends: BackendsConfig { pool, routing: None, strong: None, options: Vec::new() },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      sections: Sections::default(),
    }
  }
}
