//! The limits a manager turn and a worker run apply before they wait out a reset.
//! Their backend states that reset through `AgentBackend::retry_after`, and they rerun the same input on the same
//! backend.

use std::time::Duration;

/// How many automatic retries one input gets, so an input runs at most four times.
pub(crate) const MAX_AUTOMATIC_RETRIES: usize = 3;
/// The longest stated reset that a caller waits out. A session's next message waits behind the retry, so a longer
/// reset takes the failure path.
const MAX_WAIT: Duration = Duration::from_secs(5 * 60);

/// Whether an input that has had `retries` automatic retries waits out `reset` and runs again.
pub(crate) fn waits(retries: usize, reset: Duration) -> bool {
  retries < MAX_AUTOMATIC_RETRIES && reset <= MAX_WAIT
}

/// Whether a turn's error text and launch verdict mark a spent quota. A refused launch carries its own verdict; any
/// other error counts only when the shared member-failure classification sorts it as a quota failure.
pub(crate) fn is_quota_end(message: &str, quota_exhausted: Option<bool>) -> bool {
  charliebot_agents::backend_pool::classify_member_failure(message, quota_exhausted)
    == charliebot_agents::backend_pool::MemberFailure::Quota
}

/// The error text an input shows after its automatic retries ran out: the original text and a note that they ran.
pub(crate) fn exhausted_message(message: &str) -> String {
  format!("{message}\n\n(automatic retries ran out after {MAX_AUTOMATIC_RETRIES} attempts)")
}

/// The session notice of one automatic retry: the backend, the reset it waits out and the retry's number.
pub(crate) fn retry_notice(backend: &str, reset: Duration, attempt: usize) -> String {
  format!(
    "{backend} rate limited, retrying automatically in {} s (retry {attempt}/{MAX_AUTOMATIC_RETRIES})",
    reset.as_secs()
  )
}

#[cfg(test)]
mod tests {
  use super::is_quota_end;

  /// The quota verdict of one turn: a spent quota switches members, and a temporary failure does not.
  #[test]
  fn a_temporary_failure_is_not_a_quota_end() {
    assert!(is_quota_end("API Error: 402 Insufficient Balance", None));
    assert!(is_quota_end("API Error: 402 litellm.BadRequestError: OpenAIException - Insufficient Balance", None));
    assert!(is_quota_end("Claude account pool has no available account", Some(true)));
    assert!(!is_quota_end("500 Internal Server Error: quota exhausted", None));
    assert!(!is_quota_end("at capacity; service unavailable", None));
    assert!(!is_quota_end("at capacity", Some(false)));
  }
}
