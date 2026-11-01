# charliebot-openai-compatible

The `cc-openai-compatible` backend type and the Anthropic proxy route its calls go through.
The Claude CLI runs every call, so the type's usage counts under Claude Code.

## Public API

| Rust item | What it provides |
| --- | --- |
| `charliebot_openai_compatible::register` | The proxy route, the `cc-openai-compatible` backend type, and its usage attribution. |
| `charliebot_openai_compatible::{factory, openai_compatible_claude, options}` (private) | The backend factory, the Claude Code backend behind the proxy, and the option fields. |
| `charliebot_openai_compatible::anthropic_proxy` (private) | The route, its refusal, the stream relay, and the upstream client. |
| `charliebot_openai_compatible::{translate, sse, value_ops}` (private) | The request and answer translations, SSE framing, and the Python value semantics the translations use. |
