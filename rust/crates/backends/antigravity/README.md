# charliebot-antigravity

The `antigravity` backend type (`src/backends/antigravity`): `agy --print`, whose JSON envelope the runner reads once
the process has exited.

## Public API

| Rust item | Python source |
| --- | --- |
| `charliebot_antigravity::{register, USAGE_SOURCE}`, `AntigravityBackend` and its finish (private `antigravity_cli`, `factory`, `options`) | `src/backends/antigravity/__init__.py:7-28`; `antigravity_cli.py:17-67` (platform notices, command, env, envelope), `:69-165` (`run` and its finish); `factory.py:38-43`; `options.py:51-53` |
