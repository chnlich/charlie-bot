# charliebot-opencode

The `opencode` backend type (`src/backends/opencode`): its shared server, serve transport, session backend, relay,
usage source and snapshot reading limits.

## Public API

| Rust item |
| --- |
| `charliebot_opencode::{register, USAGE_SOURCE}` and backend attribution |
| `OpenCodeOption` |
| `OpenCodeBackend`, `OpenCodeTranslator` |
| `OpenCodeHost`, `OpenCodeProgram`, `OpenCodeServer`, `OpenCodeServerPool`, `ServerTimeouts`, `Turn`, and `headless_config` |
| OpenCode snapshot reading limits |
| `charliebot_opencode::register_usage_source` |
| `charliebot_opencode::usage_logs::OpencodeLogs`, `OpencodeLogs::new`, and its `UsageSourceImpl` methods `ops`, `logs`, `read`, `sweep` |
| `charliebot_opencode::usage_logs::read` (its `file_state`, `stored_state` and `record` helpers, and the `TOKENS_KEY` prefilter) |
| `charliebot_opencode::usage_sweep::sweep` (its vacuum, writer count, freelist and the `SQLITE_LOCK_WAIT_SECONDS` and `SQLITE_LOCK_WAIT_MS` waits) |
