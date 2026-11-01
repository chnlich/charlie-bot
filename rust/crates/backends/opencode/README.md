# charliebot-opencode

The `opencode` backend type (`src/backends/opencode`): its shared server, serve transport, session backend, relay,
usage source and snapshot reading limits.

## Public API

| Rust item | Python source |
| --- | --- |
| `charliebot_opencode::{register, USAGE_SOURCE}` and backend attribution | `src/backends/opencode/__init__.py:6-32` |
| `OpenCodeOption` | `src/backends/opencode/options.py:8-10` |
| `OpenCodeBackend`, `OpenCodeTranslator` | `src/backends/opencode/opencode.py:118-207` (backend, command and turn), `:322-459` (event consumption, translator and accumulated result), `:481-524` (one-shot text); `src/backends/opencode/factory.py:10-12` |
| `OpenCodeHost`, `OpenCodeProgram`, `OpenCodeServer`, `OpenCodeServerPool`, `ServerTimeouts`, `Turn`, and `headless_config` | `src/backends/opencode/server.py:33-35` (program and join script), `:47-68` (headless config), `:71-207` (turn and server API), `:222-448` (serve lifecycle), `:451-459` (shared server by proxy); `src/infra/home.py:28-50` (store paths); `src/backends/opencode/opencode_limits.py:13-44` (timeouts) |
| OpenCode snapshot reading limits | `src/backends/opencode/opencode_limits.py:52-57` |
| `charliebot_opencode::register_usage_source` | `src/backends/opencode/__init__.py:24-30` |
| `charliebot_opencode::usage_logs::OpencodeLogs`, `OpencodeLogs::new`, and its `UsageSourceImpl` methods `ops`, `logs`, `read`, `sweep` | `src/backends/opencode/usage_logs.py:39` (`logs`), `:115` (`read`), `:140` (`sweep`) |
| `charliebot_opencode::usage_logs::read` (its `file_state`, `stored_state` and `record` helpers, and the `TOKENS_KEY` prefilter) | `src/backends/opencode/usage_logs.py:115`, `:46`, `:56`, `:71`, `:34` |
| `charliebot_opencode::usage_sweep::sweep` (its vacuum, writer count, freelist and the `SQLITE_LOCK_WAIT_SECONDS` and `SQLITE_LOCK_WAIT_MS` waits) | `src/backends/opencode/usage_sweep.py:109`, `:70`, `:49`, `:36`, `:98`; `src/infra/timeouts.py:213-214` |
