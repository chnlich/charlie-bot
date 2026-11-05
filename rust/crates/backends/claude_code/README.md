# charliebot-claude-code

The `cc-claude` backend type (`web/backends/claude_code`): the Claude Code process, its account pool, launch,
lifecycle, environment, event translation, compaction, mid-turn delivery, and the Claude Code usage source.

The endpoint backends built here (`AnthropicEndpointBackend`) disallow Claude Code's built-in `WebSearch` and mount
the server's `charliebot-web` MCP tool instead, which `features/web_search` serves.

## Configuration

The package registers the `claude_code:` section. `enable_subagents` (default false) gives manager turns, work runs
and improve iterations Claude Code's own subagents (the `Agent` tool): `headless_disallowed_tools` leaves `Agent` off
those run kinds' `--disallowed-tools` value. Review runs, scheduled steps, one-shot runs and compaction always
disallow `Agent`, and the setup check reads the work-run list with the switch on. The server re-reads
`config.yaml` for each run, so an edit reaches the next run without a restart.

## Public API

| Rust item |
| --- |
| `charliebot_claude_code` and `charliebot_claude_code::register` |
| `ClaudeCodeBackend`, `ClaudeCodeBackend::build_command`, and its `AgentBackend` methods |
| `HEADLESS_DISALLOWED_TOOLS`, `DISALLOWED_TOOLS_FLAG`, `INSTRUCTIONS_WRITTEN_EVENT` |
| `claude_config::{ClaudeCodeConfig, enable_subagents, parse_claude_code, dump_claude_code}` |
| `claude_child_env`, `headless_claude_env`, `claude_reading_limits`, `claude_reading_limits_from`, and `out_of_family_served_models` |
| `claude_launch::{AUTO_COMPACT_WINDOW_ENV, AUTOCOMPACT_PCT_OVERRIDE_ENV, MAX_CONTEXT_TOKENS_ENV, SIMPLE_SYSTEM_PROMPT_ENV, AUTO_COMPACT_DEFAULT, COMPACT_OUTPUT_RESERVE, COMPACT_CONTEXT_RESERVE, DISABLE_CONNECTOR_SETTINGS_KEY, FAST_MODE_SETTINGS_KEY, HEADLESS_FORWARDED_ENV, headless_claude_env}` |
| `claude_lifecycle::EXCLUDE_DYNAMIC_SECTIONS_FLAG`, `ClaudeCliLifecycle`, `ClaudeCodeLifecycle`, `ClaudeCodeLifecycle::new`, and `Default` |
| `event_types::CLAUDE_ACCOUNT_LOGIN_REQUIRED` |
| `ClaudeCodeFactory` and its `BackendFactory::build` implementation |
| `options::option_model` |
| `login_dirs::{CLAUDE_CONFIG_DIR_ENV, default_claude_dir, claude_config_dir, expanduser}` |
| `claude_code::one_shot_text` implementation |
| `charliebot_claude_code::AnthropicEndpointBackend`, `AnthropicEndpointBackend::new`, and its `AgentBackend` methods |
| `charliebot_claude_code::claude_model_env` |
