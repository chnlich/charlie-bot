# charliebot-anthropic-compatible

The `cc-anthropic-compatible` backend type: Claude Code against a
configured Anthropic-compatible endpoint.

## Public API

| Rust item | What it provides |
| --- | --- |
| `charliebot_anthropic_compatible::{TYPE_NAME, register}` | The backend type name and its registration. |
| `charliebot_anthropic_compatible::{factory, options}` | The backend factory and the option fields (`usage` stays private). |
