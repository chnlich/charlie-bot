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
}
