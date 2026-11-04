//! `features.failure_triage`: the stateless half of CharlieBot failure triage.
//!
//! A scan reads a time window of failed runs and error-level server log lines, then groups them by signature. It
//! decides no-cause records and archived signatures in code, retrieves candidate LESSONS entries, and judges each
//! remaining signature with one small model one-shot. `charliebot triage scan` prints the window and one verdict per
//! signature, and writes nothing. A `--cron` scan derives its window from the bound session's fires.
//!
//! This half registers no route and no service.

mod collect;
mod judge;
mod lessons;
mod ripgrep;
mod scan;
mod signature;
mod window;

pub use collect::{
  CollectedSignature, DEFAULT_LOG_DIR, LOG_DIR_ENV, ScanOptions, Source, collect, log_dir_from_environment,
};
pub use judge::{
  BackendJudge, JUDGE_COOLDOWN, JUDGE_TIMEOUT, JudgeBackends, JudgeError, JudgeInput, JudgeRunner, Judgement,
  NativeOneShotError, Verdict, evaluate_response, judge_signature, native_one_shot_text, system_prompt, user_prompt,
};
pub use lessons::{CANDIDATE_LIMIT, LESSONS_FILE_NAME, LessonCandidate, LessonIndex};
pub use scan::{JUDGE_CONCURRENCY, NO_CAUSE_FIX_SCOPE, RowVerdict, ScanRow, preflight, scan};
pub use signature::{ExtractedCause, NoCauseShape, extract_run_cause, log_signature, normalize, run_signature};
pub use window::{Window, cron_window};

use charliebot_registry::{Registry, RegistryError};

/// `register`: this half contributes no route and no service.
pub fn register(_registry: &mut Registry) -> Result<(), RegistryError> {
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn register_adds_no_router_and_no_service() {
    let mut registry = Registry::new();
    registry.register_package("features.failure_triage", register).expect("the package registers");
    assert!(registry.routers().is_empty());
    assert!(registry.services_in(charliebot_registry::wiring::Phase::Early).is_empty());
    assert!(registry.services_in(charliebot_registry::wiring::Phase::Ready).is_empty());
  }
}
