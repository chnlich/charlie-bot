# charliebot-codex

The codex backend package (`src/backends/codex/__init__.py`): the backend type, its usage resolver, and its usage
source with the Codex quota account.

## Public API

| Rust item | Python source |
| --- | --- |
| `register` and `CodexBackend` | `src/backends/codex/__init__.py`, `codex.py` |
| `CodexHost`, `resolver::CodexSessionHost`, and `resolver::CodexUsageResolver` | `src/backends/codex/codex_usage.py`; `src/infra/home.py:28-50` (store paths) |
| `usage_logs::CodexLogs` and `USAGE_SOURCE` | `src/backends/codex/usage_logs.py` |
| `usage_quota::CodexQuotaAccount`, `pricing`, and `usage_sweep` | `src/backends/codex/usage_quota.py`, `codex_pricing.py`, `usage_sweep.py` |
| `translate::{FILE_WRITE, CodexTranslator}` | `src/backends/codex/codex.py` (event translation) |
