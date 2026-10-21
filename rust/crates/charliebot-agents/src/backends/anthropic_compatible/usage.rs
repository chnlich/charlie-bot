//! Context limits for `cc-anthropic-compatible` usage readings.

use charliebot_core::config::BackendOption;
use charliebot_registry::backend::ContextLimits;
use charliebot_store::run_records::RunKind;

use super::options::CcAnthropicCompatibleOption;
use crate::backends::claude_code::claude_launch;

pub fn claude_reading_limits(option: &BackendOption, _run_kind: RunKind) -> ContextLimits {
  let fields = CcAnthropicCompatibleOption::from_fields(&option.type_fields)
    .expect("backend options are validated when the registry loads config");
  ContextLimits {
    declared_window: Some(fields.context_window),
    compact_reserve: Some(claude_launch::COMPACT_OUTPUT_RESERVE + claude_launch::COMPACT_CONTEXT_RESERVE),
  }
}
