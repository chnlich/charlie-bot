# charliebot-claude-code

The `cc-claude` backend type (`src/backends/claude_code`): the Claude Code process, its account pool, launch,
lifecycle, environment, event translation, compaction, mid-turn delivery, and the Claude Code usage source.

## Public API

| Rust item | Python source |
| --- | --- |
| `charliebot_claude_code` and `charliebot_claude_code::register` | `src/backends/claude_code/__init__.py:17-75` |
| `ClaudeCodeBackend`, `ClaudeCodeBackend::build_command`, and its `AgentBackend` methods | `src/backends/claude_code/claude_code.py:240-292`; `src/runtime/agent_process/base.py:641-770` |
| `HEADLESS_DISALLOWED_TOOLS`, `DISALLOWED_TOOLS_FLAG`, `INSTRUCTIONS_FILE_NAME`, `INSTRUCTIONS_WRITTEN_EVENT` | `src/backends/claude_code/claude_code.py:24-37`, `:276-292` |
| `claude_child_env`, `headless_claude_env`, `claude_reading_limits`, `claude_reading_limits_from`, and `out_of_family_served_models` | `src/backends/claude_code/claude_code.py:58-68`, `:103-167`, `:198-225`; `src/backends/claude_code/claude_launch.py:43-53` |
| `claude_launch::{AUTO_COMPACT_WINDOW_ENV, AUTOCOMPACT_PCT_OVERRIDE_ENV, MAX_CONTEXT_TOKENS_ENV, SIMPLE_SYSTEM_PROMPT_ENV, AUTO_COMPACT_DEFAULT, COMPACT_OUTPUT_RESERVE, COMPACT_CONTEXT_RESERVE, DISABLE_CONNECTOR_SETTINGS_KEY, FAST_MODE_SETTINGS_KEY, HEADLESS_FORWARDED_ENV, headless_claude_env}` | `src/backends/claude_code/claude_launch.py:9-53` |
| `claude_lifecycle::EXCLUDE_DYNAMIC_SECTIONS_FLAG`, `ClaudeCliLifecycle`, `ClaudeCodeLifecycle`, `ClaudeCodeLifecycle::new`, and `Default` | `src/backends/claude_code/claude_lifecycle.py:42`, `:75-125`, `:128-155`, `:321-334` |
| `event_types::CLAUDE_ACCOUNT_LOGIN_REQUIRED` | `src/backends/claude_code/event_types.py:1-3` |
| `ClaudeCodeFactory` and its `BackendFactory::build` implementation | `src/backends/claude_code/factory.py:10-35` |
| `options::option_model` | `src/backends/claude_code/options.py:8-18` |
| `login_dirs::{CLAUDE_CONFIG_DIR_ENV, default_claude_dir, claude_config_dir, expanduser}` | `src/backends/claude_code/login_dirs.py:18-39`; `pathlib.Path.expanduser` used in `claude_code.py:256` |
| `claude_code::one_shot_text` implementation | `src/backends/claude_code/claude_code.py:294-320` |
| `charliebot_claude_code::AnthropicEndpointBackend`, `AnthropicEndpointBackend::new`, and its `AgentBackend` methods | `src/backends/claude_code/claude_code.py:328-348`; `src/backends/openai_compatible/openai_compatible_claude.py:6` |
| `charliebot_claude_code::claude_model_env` | `src/backends/claude_code/claude_code.py:87-100` |
