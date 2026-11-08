//! One backend member's subscription-quota reading: the windows a backend reports for it, and the decisions the
//! reading's provider derives from them.
//!
//! The definitions sit in this leaf module because `registry` and `usage` both need them, and `registry` already
//! imports `usage`: a definition in `registry` would make `usage` import it back and close a cycle.

use charliebot_store::metadata::UtcDateTime;

/// One quota window of a member's reading, with its utilization in `0..=1`, its length when the backend states one,
/// and the reset the backend states for it.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberWindow {
  pub utilization: f64,
  pub window_minutes: Option<i64>,
  pub resets_at: Option<UtcDateTime>,
}

/// One backend member's quota reading: the windows the backend reports, the weekly window's reset, and the two
/// decisions the reading's provider derives from them.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberReading {
  pub windows: Vec<MemberWindow>,
  /// The reset of the member's weekly (longest) window, when the provider reports one.
  pub weekly_resets_at: Option<UtcDateTime>,
  /// A window is at or above the 90% line, and that window's reset is more than 45 minutes away or is not stated.
  pub near_exhaustion: bool,
  /// A live window is at 100%, and its reset is not stated or has not passed.
  pub exhausted: bool,
}

impl MemberReading {
  /// The earliest reset any window states.
  pub fn earliest_reset(&self) -> Option<UtcDateTime> {
    self.windows.iter().filter_map(|window| window.resets_at).min()
  }
}
