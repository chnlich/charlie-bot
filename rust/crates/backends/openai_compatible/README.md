# charliebot-openai-compatible

The `cc-openai-compatible` backend type and the Anthropic proxy route its calls go through
(`src/backends/openai_compatible`). The Claude CLI runs every call, so the type's usage counts under Claude Code.

## Public API

| Rust item | Python source |
| --- | --- |
| `charliebot_openai_compatible::register` | `src/backends/openai_compatible/__init__.py:6-27` |
| `charliebot_openai_compatible::{factory, openai_compatible_claude, options}` (private) | `src/backends/openai_compatible/factory.py:1-17`, `openai_compatible_claude.py:6-21`, `options.py:8-11` |
| `charliebot_openai_compatible::anthropic_proxy`: the route, `Refusal`, the stream relay, and the upstream client (private) | `src/backends/openai_compatible/anthropic_proxy.py:447-468` (relay), `:471-490` (headers, upstream errors), `:493-542` (route; `request.json()` after its own checks, outside FastAPI's request-body path) |
| `charliebot_openai_compatible::{translate, sse, value_ops}` (private) | `src/backends/openai_compatible/anthropic_proxy.py:22-278` (request and answer translation), `:280-445` (`_sse_event` and the stream translator); `src/infra/sse.py` (line framing) |
