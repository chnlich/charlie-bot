//! How one member failure sorts: a spent quota, a refusal of authentication or model access, a token shortage, or
//! anything else.
//!
//! A backend's launch verdict carries this result, and every record site writes its availability reason, so the type
//! lives with the backend contracts. The error-text rules that produce a result from a run's output live in
//! `charliebot_agents::backend_pool`.

/// How one member failure sorts: a spent quota, a refusal of authentication or model access, a login whose token has
/// too little life for one round, or anything else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemberFailure {
  Quota,
  AccessRefused,
  /// A keyed round's login has less access-token life than a round needs. The round moves to another member for its
  /// own retries only: the login may serve other rounds, so no member is recorded out.
  TokenShort,
  Other,
}

impl MemberFailure {
  /// The availability reason this failure records, or `None` when it records the member nowhere.
  pub fn availability_reason(self) -> Option<&'static str> {
    match self {
      Self::Quota => Some(charliebot_core::event_types::QUOTA_REASON_QUOTA_ERROR),
      Self::AccessRefused => Some(charliebot_core::event_types::QUOTA_REASON_ACCESS_REFUSED),
      Self::TokenShort | Self::Other => None,
    }
  }

  /// Whether this failure records the member out of every pool that lists it.
  pub fn records_out(self) -> bool {
    self.availability_reason().is_some()
  }

  /// Whether a run that ends on this failure moves to another member of its pool. Only an ordinary error does not.
  pub fn fails_over(self) -> bool {
    self != Self::Other
  }
}

#[cfg(test)]
mod tests {
  use super::MemberFailure;

  #[test]
  fn a_token_shortage_fails_over_but_records_no_member_out() {
    assert_eq!(MemberFailure::TokenShort.availability_reason(), None);
    assert!(!MemberFailure::TokenShort.records_out());
    assert!(MemberFailure::TokenShort.fails_over());
  }

  #[test]
  fn an_ordinary_error_neither_fails_over_nor_records_a_member_out() {
    assert!(!MemberFailure::Other.fails_over());
    assert!(!MemberFailure::Other.records_out());
  }
}
