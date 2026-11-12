//! How one member failure sorts: a spent quota, a refusal of authentication or model access, or anything else.
//!
//! A backend's launch verdict carries this result, and every record site writes its availability reason, so the type
//! lives with the backend contracts. The error-text rules that produce a result from a run's output live in
//! `charliebot_agents::backend_pool`.

/// How one member failure sorts: a spent quota, a refusal of authentication or model access, or anything else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberFailure {
  Quota,
  AccessRefused,
  Other,
}

impl MemberFailure {
  /// The availability reason this failure records, or `None` when it records the member nowhere.
  pub fn availability_reason(self) -> Option<&'static str> {
    match self {
      Self::Quota => Some(charliebot_core::event_types::QUOTA_REASON_QUOTA_ERROR),
      Self::AccessRefused => Some(charliebot_core::event_types::QUOTA_REASON_ACCESS_REFUSED),
      Self::Other => None,
    }
  }

  /// Whether this failure records the member out of every pool that lists it.
  pub fn records_out(self) -> bool {
    self.availability_reason().is_some()
  }
}
