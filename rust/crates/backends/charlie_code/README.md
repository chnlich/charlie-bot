# charliebot-charlie-code

The `charlie-code` backend type (`src/backends/charlie_code`): the CharlieCode CLI against an OpenAI-compatible
endpoint. CLC's usage lives only in CharlieBot's own run logs, so its usage source has no log reader.

## Public API

| Rust item | Python source |
| --- | --- |
| `charliebot_charlie_code::{register, USAGE_SOURCE}`, `CharlieCodeBackend`, `API_KEY_ENV` (private `charlie_code_cli`, `translate`, `factory`, `options`) | `src/backends/charlie_code/__init__.py:6-34`; `charlie_code.py:43-166` (backend, command, env, upload gate), `:168-239` (translator), `:28-40` (context fields); `factory.py:44-56`; `options.py:66-81` |
| `charliebot_core::pyfmt::format_g` (the `%g` of the progress labels) | `src/backends/charlie_code/charlie_code.py:21-25` (`_duration_label`) |
