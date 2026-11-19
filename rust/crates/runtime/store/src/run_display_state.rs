//! A run's display state, derived from its facts. The run-record readers and the message aggregator both name it, so
//! it sits in a leaf module that neither dependency reaches back into.

/// A fact-derived run display value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunDisplayState {
  Queued,
  Running,
  Attention,
  Stopped,
  Withheld,
  Success,
  Failed,
  Interrupted,
}

impl RunDisplayState {
  /// The persisted UI spelling for this state.
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Queued => "queued",
      Self::Running => "running",
      Self::Attention => "attention",
      Self::Stopped => "stopped",
      Self::Withheld => "withheld",
      Self::Success => "success",
      Self::Failed => "failed",
      Self::Interrupted => "interrupted",
    }
  }

  /// The state a persisted UI spelling names, or `None` for a word that names no state.
  pub fn parse(word: &str) -> Option<Self> {
    [
      Self::Queued,
      Self::Running,
      Self::Attention,
      Self::Stopped,
      Self::Withheld,
      Self::Success,
      Self::Failed,
      Self::Interrupted,
    ]
    .into_iter()
    .find(|state| state.as_str() == word)
  }

  /// Whether a run in this state is live: running or queued, so its round stays in flight and a stop request reaches it.
  pub fn is_live(self) -> bool {
    matches!(self, Self::Running | Self::Queued)
  }
}

#[cfg(test)]
mod tests {
  use super::RunDisplayState;

  #[test]
  fn the_persisted_spelling_names_its_state_and_liveness() {
    for word in ["queued", "running", "attention", "stopped", "withheld", "success", "failed", "interrupted"] {
      let state = RunDisplayState::parse(word).expect("a persisted spelling names its state");
      assert_eq!(state.as_str(), word);
      assert_eq!(state.is_live(), matches!(word, "queued" | "running"));
    }
    assert_eq!(RunDisplayState::parse("no-such-state"), None);
  }
}
