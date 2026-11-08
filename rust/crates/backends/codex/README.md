# charliebot-codex

The codex backend package: the backend type, its usage resolver, and its usage source with the Codex quota account.

## Public API

- `register` and `register_with_host`, the backend type `CodexBackend` and the factory `factory::CodexFactory`
- `CodexHost`, `resolver::CodexSessionHost` and `resolver::CodexUsageResolver`
- `usage_logs::CodexLogs`, `usage_logs::session_id` and `USAGE_SOURCE`
- `usage_quota::CodexQuotaAccount` and `pricing`
- `FILE_WRITE`, the event type a `file_change` item writes; the event translator stays private to the crate

## Quota reading

Codex keeps its existing rollout source: the newest plan-pool event from the recent scan set. Its Auto member reading
uses the shared quota-window rule also used by Devin, OpenCode Go and Antigravity. Each live window contributes its
own reset to the near-exhaustion decision; a window whose reset passed does not exhaust the member. The weekly reset
comes from the seven-day window.
