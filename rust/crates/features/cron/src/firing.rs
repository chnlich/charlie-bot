//! The firing reference a cron fire's runs share: `cron:<task>:<firing>` as a sequence's `owner_ref`, and the round
//! request id that appends `:leaf`.

use crate::config::ScheduledTaskConfig;

/// The prefix that marks a cron firing reference.
const FIRING_REF_PREFIX: &str = "cron:";
/// The suffix that marks a round's leaf request id.
const ROUND_REQUEST_SUFFIX: &str = ":leaf";

/// The firing reference of `task`'s `firing`: `cron:<task>:<firing>`. A task name carries no colon, so the task name
/// ends at the first colon after the prefix.
pub(crate) fn firing_ref(task: &ScheduledTaskConfig, firing: &str) -> String {
  format!("{FIRING_REF_PREFIX}{}:{firing}", task.name)
}

/// The task name and firing in a sequence's `owner_ref`, or `None` for a reference in another format.
pub(crate) fn parse_firing_ref(owner_ref: &str) -> Option<(String, String)> {
  let rest = owner_ref.strip_prefix(FIRING_REF_PREFIX)?;
  let (name, firing) = rest.split_once(':')?;
  if name.is_empty() || firing.is_empty() {
    return None;
  }
  Some((name.to_string(), firing.to_string()))
}

/// The request id of the worker session a round creates. The session's creation fact stores it, so `round_task_of`
/// reads the creating task back.
pub(crate) fn round_request_id(task: &ScheduledTaskConfig, firing: &str) -> String {
  format!("{}{ROUND_REQUEST_SUFFIX}", firing_ref(task, firing))
}

/// The task name in a round's request id, or `None` for any other request id.
pub(crate) fn round_request_task(request_id: &str) -> Option<&str> {
  let firing = request_id.strip_prefix(FIRING_REF_PREFIX)?.strip_suffix(ROUND_REQUEST_SUFFIX)?;
  firing.split_once(':').map(|(name, _)| name)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn task(name: &str) -> ScheduledTaskConfig {
    serde_yaml::from_str(&format!("name: {name}\ncron: \"0 3 * * *\"\n")).unwrap()
  }

  #[test]
  fn a_firing_reference_parses_back_to_its_task_and_firing() {
    let reference = firing_ref(&task("nightly"), "2026-11-06T03:00:00-08:00");
    assert_eq!(reference, "cron:nightly:2026-11-06T03:00:00-08:00");
    assert_eq!(parse_firing_ref(&reference), Some(("nightly".to_string(), "2026-11-06T03:00:00-08:00".to_string())));
    assert_eq!(parse_firing_ref("nightly"), None);
    assert_eq!(parse_firing_ref("cron:nightly:"), None);
  }

  #[test]
  fn a_round_request_id_names_the_task_that_created_the_round() {
    let task = task("nightly");
    let request_id = round_request_id(&task, "2026-11-06T03:00:00-08:00");
    assert_eq!(round_request_task(&request_id), Some("nightly"));
    assert_eq!(round_request_task(&firing_ref(&task, "2026-11-06T03:00:00-08:00")), None);
  }
}
