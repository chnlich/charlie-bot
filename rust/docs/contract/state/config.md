# Configuration: `config.yaml`, `config.d/`, `credentials.yaml`

Baseline: the Python tree at `4521a51b2`. This document gives every key the baseline accepts, its type and
default, the validation rule, and the error a bad value raises. Error texts are quoted from source; `<...>` marks
an interpolated value. A Rust server reading the same files must accept exactly this set and refuse the rest with
equivalent errors. **UNVERIFIED** marks a statement not confirmed from source, with the reason.

## 1. Files

| File | Holds | Owner module |
| --- | --- | --- |
| `<home>/config.yaml` | structure only, one sectioned YAML mapping | `src/infra/config.py` |
| `<home>/credentials.yaml` | secrets, `section -> key -> scalar` | `src/infra/credentials.py` |
| `<home>/config.d/cron.d/<name>.yaml` | one scheduled task per file | `src/features/cron/loader.py` |

YAML is parsed with libyaml's safe loader (`yaml.CSafeLoader`, `src/infra/yaml_utils.py`). A missing or empty
file loads as the default (`{}` for both main files). Writers use `save_yaml` (see `files.md`, discipline YAML).

## 2. Load pipeline (`config.load_config`)

Steps run in this order; the first failure raises `ValueError`.

1. **`config.d/` tripwire.** Any regular file directly in `<home>/config.d/` whose name ends in `.yaml` raises
   `"<path> is not a config location: package fragment files live in config.d/ subdirectories; keys belong in
   config.yaml (structure) or credentials.yaml (secrets)"`.
2. **Retired top-level keys.** Every top-level key found in the core `LEGACY_KEYS` or a package's registered
   legacy keys (section 7) raises one error that lists all of them:
   `"<config path> still uses retired top-level keys; move each one:\n  <key> -> <new location>"`. A secret's
   new location reads `credentials.yaml <section>.<key>`.
3. **Retired `backends` keys.** `backends.preference` or `backends.worker_default` raises `"<config path> still uses
   retired keys (backends.preference, ...); list the rotation members under backends.pool in rotation order
   instead (each id must name a backends.options entry)"`.
4. **`charliebot_home` key.** Raises `"<config path> sets 'charliebot_home'; that path is chosen by the
   CHARLIEBOT_HOME environment variable. Remove the key."`.
5. **Model validation** (`CharlieBotConfig(charliebot_home=<home>, **yaml)`):
   - A top-level key that is neither a field nor a registered section raises
     `"unknown config key(s) '<k>', ...; declare the key(s) on CharlieBotConfig, register a config section for
     them, or remove them"`.
   - An unknown field inside a `backends.options` entry raises
     `"backend entry '<id>' (type <type>) has unknown field '<field>'"`.
   - Any other validation failure propagates as a pydantic `ValidationError` whose location starts with the
     section key.
6. **Package checks** (`config_registry.run_config_checks`, in registration order, after the whole model has
   validated): `check_default_backend` from voice, then `check_claude_pools` from claude_code (section 5).

Registered sections and option types exist only after `src/app/registrations.py::register_all()` has imported
every package. The server and every CLI subcommand call it before the first config parse.

### Hot reload

`config.get_config()` re-runs `load_config` when the `(mtime, size)` of `config.yaml` changes. A reload that fails
keeps the previous config, logs `config_reload_failed` once per onset, and the old object stays in use. The
returned object keeps its identity across reloads: new values are copied into it field by field. With nothing
cached yet, the error propagates. `credentials.yaml` follows the same rule through its own fingerprint
(`credentials_reload_failed`), except that the cached object is replaced wholesale.

### Startup-only checks (server `main()`, `server.py`)

These run once, after the first load and before uvicorn starts. A failure stops the server.

1. `config.require_backends(cfg)`:
   - Empty `backends.options` raises `"config.yaml: backends.options lists no backend; copy the starter entries
     from configs/config.example.yaml"`.
   - Otherwise every violation is collected into one error. A repeated id adds
     `"<config path>: backends.options[<i>] repeats id '<id>'"` and an unknown pool member adds
     `"<config path>: backends.pool[<i>] names unknown backend '<id>'"`. The combined error is
     `"backend references must name a backends.options id (id rule: BackendsConfig in src/infra/config.py):\n  <problem>..."`.
2. `v1_sessions.require_no_v1_sessions(cfg)` (see `files.md` 6.1).
3. Registered startup checks, in order:
   - `src.features.cron.backend_refs.check_backend_refs`: every cron task `backend` and step `backend` must name an
     option id. Each problem reads `"<home>/config.d/cron.d/<name>.yaml: <backend|steps '<step>' backend> names
     unknown backend '<id>'"`, under the same `backend references must name ...` heading.
   - `src.features.usage.backend_ids.check_backend_ids`: every option id must start with one of the id prefixes of
     its type's usage source. Violations read
     `"<config path>: backends.options[<i>] id '<id>' type '<type>' attributes to no usage source"` or
     `"<config path>: backends.options[<i>] id '<id>' type '<type>' starts with none of the id prefixes <source name> registers <prefix tuple>"` (the tuple is Python's repr, for example `('codex-',)`), under the heading
     `"backend option ids must attribute to their type's usage source (id rule: BackendsConfig in src/infra/config.py):"`.

CLIs never run these startup checks: `load_config` stays permissive for commands that never resolve a backend.

## 3. Top-level keys (core)

Every core section model sets `extra='forbid'`.

| Key | Type | Default | Rule and meaning |
| --- | --- | --- | --- |
| `headless_chrome_bin` | str | `""` | absolute path of a headless-Chromium-compatible binary for the page-height assertion of `artifact check` and plan registration. Empty fails that assertion with `"headless_chrome_bin is required for plan registration: set it in the host config.yaml to the absolute path of a headless-chromium-compatible binary"`; a missing file fails it with `"... but the configured path does not exist: <path>"`. |
| `server.host` | str | `127.0.0.1` | uvicorn bind address |
| `server.port` | int | `18498` | uvicorn port; also `server_base_url = http://localhost:<port>` for CLI-to-server and server-to-self calls |
| `server.subprocess_buffer_limit_mb` | int | `1024` | asyncio stream buffer limit for agent stdout, in MB |
| `server.session_memory_max_mb` | int | `12288` | per-session cgroup `memory.max`, in MB; `0` disables session cgroups entirely. A value above 0 places the session under the agents' slice (`charliebot-agents.slice`), and a missing slice raises `SessionCgroupUnavailableError` instead of running the session unbounded (`src/infra/process.py`, `ensure_session_cgroup`) |
| `server.session_swap_max_mb` | int | `0` | per-session cgroup `memory.swap.max`, in MB |
| `server.session_tasks_max` | int | `4096` | per-session cgroup `pids.max`; `0` writes `max` |
| `server.min_free_disk_gib` | int | `10` | worker-class launch precheck on the filesystems of the home and `paths.worktree_dir`; `0` disables it. A failing check keeps the run queued and reports blocked to the parent. Manager turns skip it. |
| `paths.workspace_dirs` | list[str] | `["~/workspace"]` | directories scanned one level deep for git repositories (`discover_repos`); `~` is expanded at load |
| `paths.worktree_dir` | str | `"~/worktrees"` | root for worker git worktrees; `~` is expanded at load |
| `paths.scratch_dir` | str | `"~/scripts"` | root whose top-level entries `disk-sweep` treats as old scratch directories; `~` is expanded at load (`src/infra/config.py:98`, `:105`) |
| `backends.pool` | list[str] | `[]` | rotation order of option ids for unpinned work; each must name an option (startup check) |
| `backends.routing` | `RoutingConfig` or null | null | optional router command; routed unpinned work asks it to order candidates before pool rotation |
| `backends.options` | list[option] | `[]` | backend catalog (section 4); an empty list fails server startup |
| `ui.home_services` | list[`{name: str, description: str, url: str}`] | `[]` | cards on `/home`, probed by connecting to the URL's host and port; each item forbids extra keys |
| `telegram.chat_id` | str or null | null | Telegram notification target; sending without it raises `RuntimeError` (`src/infra/notifications.py`) |

No pydantic constraint rejects a negative integer in `server.*`. **UNVERIFIED:** how negative values behave; no
validator was found, so they pass load.

Derived values: `sessions_dir = <home>/sessions`, `claude_md_file = <home>/MASTER_AGENT_PROMPT.md`,
`config_d_dir = <home>/config.d`, `charlie_bot_repo` = the repo root of the running server (from the module
location), and `HOUSE_TIMEZONE = "America/Los_Angeles"`.

`require_backend_option(cfg, id, subject=...)` raises `"<subject>backend '<id>' is not in backends.options"`
wherever a stored id no longer resolves at run time.

`RoutingConfig` (`src/infra/config.py`) has one required field, `command: list[str]`. Extra keys are refused. The first command item resolves on the server process's `PATH`; the command runs once per routed choice and exchanges one JSON object on stdin and stdout. `null` keeps pool rotation as the only selector.

## 4. `backends.options[]`

Each entry is parsed by the option model registered for its `type` (`src/infra/config_registry.py::option_model`):

- an unknown `type` raises `"backend type '<t>' is not registered; registered types: <list>"`;
- every model sets `extra='forbid'`, so an unknown field gives the error in section 2, step 5.

Fields of every entry (`src/infra/backend_models.py::BackendOption`):

| Field | Type | Default | Rule |
| --- | --- | --- | --- |
| `id` | str | required | `<type prefix>-<family>`, no version. The prefix must be one its usage source registers (table below). A pool-bound cc-claude entry may end with `-<pool name>`. Session, thread and run metadata, cron files and `pool` store the id. |
| `label` | str | required | display label |
| `model` | str | required at load | a missing `model` raises `"backend '<id>' (type '<type>') requires 'model'"`. An empty string passes load, and use then raises `"<subject>'<id>' has no default model"`. |
| `prompt_overlay` | str or null | null | overlay name under `prompts/model_overlays/` (no `.md`). `"none"` means deliberately no overlay. null or an unreadable file sends a `backend_overlay_inactive` alert event and continues. |
| `type` | str | required | one of the types below |

Per-type fields:

| `type` | Package | Extra fields | Usage source and id prefixes |
| --- | --- | --- | --- |
| `cc-claude` | `src.backends.claude_code` | `effort: str\|null`; `fast_mode: bool = false` (adds `"fastMode": true` to `--settings`); `cli_binary: str\|null` (replaces `claude` as argv[0]); `account_pool: str\|null` | `Claude Code`: `claude-` |
| `cc-anthropic-compatible` | `src.backends.anthropic_compatible` | `api_base: str` (required, at least one character; the endpoint root, and a value ending in `/v1` is refused); `context_window: int` (required, > 0; sets the model window and the auto-compact window); `credential: str\|null` (section whose `api_key` is the bearer; null sends `no-auth`); `effort: str\|null` (passed to Claude Code as `--effort` when set); `headers: dict[str,str]` (defaults to `{}`, sent to Claude Code as custom request headers; `options.py:17`, `factory.py:23`); `extra_body: dict[str,Any]` (defaults to `{}`; sent to Claude Code as `CLAUDE_CODE_EXTRA_BODY`, a gateway-only variable that `without_gateway_env` strips from the official Anthropic path; `options.py:18`, `factory.py:24`, `claude_code.py:462-463`) | attributed to `Claude Code`: `claude-` |
| `cc-openai-compatible` | `src.backends.openai_compatible` | `api_base: str` (required; upstream OpenAI-compatible base URL); `credential: str\|null` (section whose `api_key` becomes the upstream bearer) | attributed to `Claude Code`: `claude-` |
| `codex` | `src.backends.codex` | `model_reasoning_effort: str\|null` (null means `xhigh`); `model_auto_compact_token_limit: int\|null`, > 0 | `Codex`: `codex-` |
| `gemini` | `src.backends.gemini` | none | **none registered**, so a gemini option fails `check_backend_ids` at server start ("attributes to no usage source") |
| `antigravity` | `src.backends.antigravity` | `print_timeout: str\|null` (Go duration for `agy --print-timeout`; null means `1h`) | `Antigravity`: `antigravity-`, `agy-` |
| `charlie-code` | `src.backends.charlie_code` | `api_base: str\|null` (required at use: `"charlie-code backend requires api_base (set api_base on its backends.options entry in config.yaml)"`); `context_window: int\|null` > 0; `credential: str\|null`; `proxy_url: str\|null`; `image_input: bool = true`; `stream: bool = true`; `timeout_seconds: int\|null` > 0; `top_p: float\|null` in (0, 1]; `temperature: float\|null` >= 0 | `CLC`: `charlie-code-`, `clc-` |
| `opencode` | `src.backends.opencode` | `proxy_url: str\|null`; `context_limit: int\|null` > 0 (the model's context limit in opencode's `providers` config); `extra_body: dict` (defaults to `{}`; top-level fields added to every request body that model sends, under the model's `body` in the `providers` config; `src/backends/opencode/options.py:16,35-37`); two options that name one model must agree on each field they set, else `ValueError` naming both options at use (`model_overrides_config`, `options.py:19`); `model` must be `<provider>/<model>` (split on the first `/` at use) | `opencode`: `opencode-`, `oc-` |

Run-time credential errors: `credentials.require(section, key)` raises
`"credentials.<section>.<key> is not set in <credentials path>"`. A cc-openai-compatible backend also needs
`credentials.charliebot.access_key`, because the child CLI authenticates to the server's own Anthropic proxy with
it.

## 5. Package sections

Each package registers its section with `config_registry.register_config_section(key, "module:Model",
legacy_keys=...)`. An omitted section takes the model's defaults. Every model sets `extra='forbid'`.

### `publish` (`src.features.artifacts`, `src.features.artifacts.config:PublishConfig`)

| Key | Type | Default | Rule |
| --- | --- | --- | --- |
| `publish.command` | list[str] or null | null | an external program's argv; `charliebot publish` runs it with the artifact |
| `publish.dir` | path or null | null | static publish directory; `~` is expanded |
| `publish.public_base_url` | str or null | null | base URL of published links |

Errors: an empty list raises `"publish.command is an empty list; name the program and its arguments, or leave the
key unset"`. `command` set together with `dir` or `public_base_url` raises `"publish.command cannot be set
together with publish.dir or publish.public_base_url; a host configures one publish lane"`.

### `code_server` (`src.features.code_server`, `CodeServerConfig`)

| Key | Type | Default | Rule |
| --- | --- | --- | --- |
| `code_server.bin` | str or null | null | code-server executable |
| `code_server.config` | str | `"configs/code-server.yaml"` | code-server config path; `~` is expanded and a relative path is taken against the server's repo root (`code_server_config_path`) |

Use-time errors (`code_server_listen_port`): `"code-server config must be a YAML mapping: <path>"`,
`"code-server config must define bind-addr: <path>"`, `"code-server bind-addr port must be an integer: <bind-addr>"`.

### `slack` (`src.features.slack`, `SlackConfig`)

| Key | Type | Default | Rule |
| --- | --- | --- | --- |
| `slack.allowed_user_ids` | list[str] | `[]` | Slack user ids allowed to summon; empty means nobody. The `slack` service starts only when `slack.bot_token`, `slack.app_token` and this list are all set. |

### `discord` (`src.features.discord`, `DiscordConfig`)

| Key | Type | Default | Rule |
| --- | --- | --- | --- |
| `discord.allowed_users` | dict[str, str] | `{}` | Discord user id mapped to the person who owns it; empty means nobody |

A retired `discord.allowed_user_ids` raises `"discord.allowed_user_ids is retired: move each allowed Discord user id
into discord.allowed_users as <user id>: <the person that account belongs to>"`.

### `voice` (`src.features.voice`, `VoiceConfig`)

| Key | Type | Default | Rule |
| --- | --- | --- | --- |
| `voice.default_backend` | str | `"gemini"` | must be a transcription registry id (`gemini`, `muse`). `VoiceConfig` forbids other keys (`src/features/voice/config.py:9`), so a `voice.engine` or `voice.model_id` key is refused; UNVERIFIED: the load failure was read from the pydantic model and not probed. Config check: `"voice.default_backend '<v>' is not a transcription backend; known: <ids>"`. |
| `voice.vocabulary` | list[str] | `[]` | proper nouns for backends that accept them |
| `voice.languages` | list[str] | `[]` | BCP-47 base codes; empty means auto-detect |

### `accounts` (`src.backends.claude_code`, `src.backends.claude_code.claude_config:AccountsConfig`)

| Key | Type | Default | Rule |
| --- | --- | --- | --- |
| `accounts.claude` | list[`{label: str, config_dir: str}`] | `[]` | Claude subscription logins; `config_dir` becomes the child's `CLAUDE_CONFIG_DIR` (`~` expanded at use) |
| `accounts.claude_pools` | dict[str, list[str]] | `{}` | named pools of account labels |
| `accounts.claude_compaction.relay_tokens` | int > 0 | `100000` | compaction floor for relays |
| `accounts.claude_compaction.expired_cache_tokens` | int > 0 | `50000` | compaction floor after cache expiry |
| `accounts.claude_compaction.idle_tokens` | int > 0 | `100000` | context floor for compacting an idle manager session before its Claude prompt cache expires |

Config check `check_claude_pools`:

- `"accounts.claude_pools['<pool>'] names accounts missing from accounts.claude: <labels>"`
- `"accounts.claude_pools['<pool>'] lists no account"`
- with no pools defined: `"backend '<id>' sets account_pool '<pool>' but accounts.claude_pools defines no pools"`
- with pools defined, every cc-claude option must name one: `"backend '<id>' (cc-claude) names no account_pool;
  defined pools: <names>"` and `"backend '<id>' names undefined account_pool '<pool>'; defined pools: <names>"`.

## 6. Registration evidence

Every package in `src/app/registrations.py::PACKAGES` that registers a config section, config check, legacy keys
or option model. `register_backend_type` registers the type's option model (`config_registry.register_option_model`)
as part of the type. Found by this grep, run from the worktree root (the same loop is in `services.md` section 5):

```
for pkg in $(sed -n '/^PACKAGES = (/,/^)/p' src/app/registrations.py | grep -o '"src[^"]*"' | tr -d '"'); do
  d=${pkg//.//}
  grep -lE 'register_config_section|register_legacy_keys|register_config_check|register_backend_type' $d/__init__.py
done
```

| Package | Registers |
| --- | --- |
| `src.features.artifacts` | section `publish` (+ legacy keys) |
| `src.features.code_server` | section `code_server` (+ legacy keys) |
| `src.features.slack` | section `slack` (+ legacy keys) |
| `src.features.discord` | section `discord` |
| `src.features.voice` | section `voice`, check `check_default_backend` |
| `src.backends.claude_code` | section `accounts` (+ legacy keys), check `check_claude_pools`, option type `cc-claude` |
| `src.backends.gemini` | legacy keys, option type `gemini` |
| `src.backends.openai_compatible` | option type `cc-openai-compatible` (through `register_backend_type`) |
| `src.backends.antigravity` | option type `antigravity` |
| `src.backends.charlie_code` | option type `charlie-code` |
| `src.backends.codex` | option type `codex` |
| `src.backends.anthropic_compatible` | option type `cc-anthropic-compatible` |
| `src.backends.opencode` | option type `opencode` |

## 7. Retired keys (load error, section 2 step 2)

Core (`config.LEGACY_KEYS`):

| Old top-level key | Move to |
| --- | --- |
| `server_host`, `server_port`, `subprocess_buffer_limit_mb` | `server.host`, `server.port`, `server.subprocess_buffer_limit_mb` |
| `workspace_dirs`, `project_dirs` | `paths.workspace_dirs` |
| `worktree_dir` | `paths.worktree_dir` |
| `backend_options` | `backends.options` |
| `model_preference` | `backends.pool` |
| `home_services` | `ui.home_services` |
| `telegram_chat_id` | `telegram.chat_id` |
| `telegram_bot_token` | credentials `telegram.bot_token` |
| `charliebot_access_key` | credentials `charliebot.access_key` |
| `moonshot_api_key`, `aigw_api_key`, `linear_api_key` | credentials `moonshot.api_key`, `aigw.api_key`, `linear.api_key` |
| `feishu_app_id`, `feishu_app_secret`, `feishu_refresh_token`, `feishu_user_access_token` | credentials `feishu.<same suffix>` |
| `google_client_id`, `google_client_secret`, `google_refresh_token` | credentials `google.client_id`, `google.client_secret`, `google.refresh_token` |
| `google_docs_client_id`, `google_docs_client_secret`, `google_docs_refresh_token` | credentials `google.client_id`, `google.client_secret`, `google.refresh_token` |
| `google_docs_default_folder_id` | credentials `google.docs_default_folder_id` |
| `twitter_api_key`, `twitter_api_secret`, `twitter_access_token`, `twitter_access_token_secret` | credentials `twitter.<same suffix>` |

Packages (`register_legacy_keys` or `register_config_section(legacy_keys=...)`):

| Old key | Move to | Package |
| --- | --- | --- |
| `publish_dir`, `public_base_url` | `publish.dir`, `publish.public_base_url` | artifacts |
| `code_server_bin`, `code_server_config` | `code_server.bin`, `code_server.config` | code_server |
| `slack_allowed_user_ids` | `slack.allowed_user_ids` | slack |
| `slack_bot_token`, `slack_app_token`, `slack_user_token` | credentials `slack.bot_token`, `slack.app_token`, `slack.user_token` | slack |
| `claude_accounts`, `claude_compaction` | `accounts.claude`, `accounts.claude_compaction` | claude_code |
| `gemini_api_key`, `gemini_model` | credentials `gemini.api_key`, `gemini.model` | gemini |

## 8. `credentials.yaml`

Loader: `src/infra/credentials.py::load_credentials`.

- The document must be a mapping, else `"credentials must be a mapping of sections: <path>"`. An empty file loads as
  no sections.
- Each section must be a mapping, else `"credentials.<section> must be a mapping of keys: <path>"`.
- Each value must be a string or an integer, else `"credentials.<section>.<key> must be a string or integer: <path>"`.
  A null value counts as unset and is dropped. YAML `true`/`false` pass, because Python's `bool` is an `int`
  subclass; floats, lists and mappings fail.
- Section and key names are never validated.
- The seed copy from `configs/credentials.example.yaml` is chmod 0600; the loader does not check the mode.

Keys the server and its CLIs read:

| Key | Reader | Effect when missing |
| --- | --- | --- |
| `charliebot.access_key` | `configured_access_key()`: HTTP auth (`src/runtime/api/auth.py`, `deps.py`), run-token signing (`task_execution._child_env`), CLI auth (`cli/common.py`), `memory` CLI, page `auth_enabled` | Auth is disabled (`auth_enabled: false`). Agent child launches raise `"run-token signing requires credentials.yaml charliebot.access_key; agent child environments cannot be built without it"`. cc-openai-compatible launches raise the `require` error. |
| `<option.credential>.api_key` | cc-anthropic-compatible factory (when `credential` is set), charlie-code factory, cc-openai-compatible proxy upstream | `require` error at launch or request |
| `<router.jev.credential>.api_key` | `src/features/router/routing.py::design_probability` | `charliebot router` exits 1 with the missing credential error |
| `slack.bot_token`, `slack.app_token` | `src/features/slack/service.py`, `slack_listener.py` | the slack service does not start |
| `discord.bot_token` | `src/features/discord/service.py`, `discord_listener.py`, `discord_commands.py` | the discord service does not start (it starts only when `discord.bot_token` and `discord.allowed_users` are both set) |
| `telegram.bot_token` | `src/infra/notifications.py::send_telegram` | `require` error when a notification is sent |
| `gemini.api_key` | voice transcription backend `gemini` | that transcription backend reports unavailable |
| `meta.model_api_key` | voice transcription backend `muse` | that transcription backend reports unavailable |

`slack.user_token` and the other sections in the example file (`moonshot`, `gemini.model`, `aigw`, `feishu`,
`google`, `linear`, `twitter`) are not read by server code; skills read their own sections.

## 9. `config.d/cron.d/<name>.yaml`

Loader: `src/features/cron/loader.py`; model: `src/features/cron/config.py::ScheduledTaskConfig`
(`extra='forbid'`).

Files and names:

- Only regular files ending in `.yaml` whose name does not start with `.` are read, in sorted order. A missing
  `cron.d/` means no tasks.
- The job name is the file stem and must match `^[A-Za-z0-9][A-Za-z0-9._-]*$`; otherwise the file becomes a
  per-file error `"file name is not a valid cron task name"`.
- Each file loads independently: a bad file becomes a `ScheduledTaskError{name, path, error, enabled}` shown in the
  cron UI, never a server startup failure. The one exception is a backend reference, checked at startup
  (section 2).
- A broken file set triggers one Telegram alert per new set of names (`state/cron_alert_fingerprint.json`).
- Reload follows a fingerprint over the files and every `prompt_file` they point to.

Body errors:

- The body is not a mapping: `"cron config must be a mapping"`.
- A `name` key: `"the body must not carry a 'name' key; the file name is the job name"`.
- An inline `prompt` at task or step level: `"a cron.d host file must not carry an inline 'prompt'; it holds the
  path to the prompt source under 'prompt_file', and the loader reads that file on every load"` (the step form
  says `a step holds the path ...`).

| Key | Type | Default | Rule |
| --- | --- | --- | --- |
| `cron` | str | required | cron expression (croniter) |
| `prompt_file` | str or null | null | path to the prompt body. `~`-prefixed or absolute paths are taken literally; a relative path resolves against the **server's repo root** (`cfg.charlie_bot_repo`), not the home. The file is read on every load; an unreadable file raises `"cron entry '<name>' prompt_file unreadable: <path> (<error>)"`. |
| `repo` | str or null | null | repository for worker runs; `~` is expanded |
| `backend` | str or null | null | option id; must exist (startup check) |
| `timezone` | str | `"America/Los_Angeles"` | IANA zone. The literal `local` is rewritten at load to the host zone from `/etc/localtime`, falling back to `UTC` with a warning. |
| `enabled` | bool | true | |
| `project` | str or null | null | |
| `allow_failure` | bool | false | |
| `session_id` | str or null | null | binds the task to one existing manager node; written back by the scheduler's auto-bind |
| `mode` | `master`, `worker`, or null | null | needs `session_id`. `master` admits the prompt into the bound manager; `worker` (or absent) creates one worker leaf per firing. |
| `handler` | str or null | null | a registered handler name: `backup`, `usage_ledger`, `cool_storage` |
| `steps` | list or null | null | ordered worker chain; each step is `{name: str (min 1 char), prompt_file, backend, distinct_backend_from}` with `extra='forbid'` |

Model validation errors (`ScheduledTaskConfig`):

- `"task must have exactly one of 'prompt', 'prompt_file', 'steps', or 'handler'"`
- `"'mode' requires 'session_id' (mode selects how a bound task fires)"`
- `"mode 'master' requires a prompt source ('prompt' or 'prompt_file')"`
- `"mode 'master' forbids 'steps' and 'handler'; the manager wake is a prompt"`
- `"steps must be a non-empty list"`
- `"duplicate step name '<name>'"`
- `"step '<name>' has no prompt body; the loader resolves each step's ..."`
- `"step '<name>' declares distinct_backend_from '<src>', which is not a step of this task"`
- `"step '<name>' declares distinct_backend_from '<src>', which must name a step listed earlier in the same task"`
- `"step '<name>' and its distinct_backend_from source '<src>' both declare backend '<id>'; the two steps must name
  different backends"`

Writers (`files.md` 5.1): the cron HTTP API, `cron/seed.py` (from `configs/cron.default.yaml`, setup step only;
the server never seeds cron files), and `cron_files.write_cron_key`.

## 10. `charliebot config get <key>`

`src/runtime/cli/config.py` prints the value of one top-level field or registered section: a model, list or dict
as compact JSON, anything else with `print`. A key that is neither a field nor a section prints `"error: unknown
config key: <key> (not a CharlieBotConfig field or registered section)"` and exits 2. A null value prints
`"error: config key <key> is unset"` and exits 1. See `cli.md`.
