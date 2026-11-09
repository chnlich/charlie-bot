# charliebot-opencode

The `opencode` backend type (`web/backends/opencode`): its shared server, serve transport, session backend, relay,
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

## OpenCode Go quota

The usage source reads the active credential row from the OpenCode database in read-only mode and requests
`GET https://opencode.ai/zen/go/v1/usage`. It does not refresh the credential or write to that database. The response
supplies rolling, weekly and monthly utilization and reset times; the panel names those windows and leaves the month
length unspecified. Subscription member readings use the last successful response and the shared quota rule. Member
selection makes no request.
