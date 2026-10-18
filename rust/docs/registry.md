# The plug-in registry

`charliebot-registry` is the Rust counterpart of the Python hook layer: `src/runtime/hooks/*`,
`src/infra/config_registry.py`, `src/infra/metadata_slot_registration.py` and the lifespan in `server.py`. Each
package makes its contributions through one `register(&mut Registry)` function, and the server builds one
`Registry` at startup. The registry is read-only after that.

Rules that hold for every hook kind:

- A registration names its hook. A name that another registration of the same kind holds fails. The error names the
  package that made the earlier registration, as Python's `ValueError` does, and the server does not start.
- A registration outside a package's `register` fails.
- The registries keep registration order. Routes merge, page parts list, metadata keys serialize, template paths
  search, and lifespan steps and services run in that order.
- A package appears once in the list. A second listing fails.

## Composition root

`rust/crates/charliebot-server/src/packages/mod.rs` lists the packages in the order of
`src/app/registrations.py:16-46`, one entry per package:

```rust
pub const PACKAGES: &[(&str, RegisterFn)] = &[
  ("src.features.artifacts", features::artifacts), // stub: src.features.artifacts
  ("src.features.backup", stub),                   // stub: src.features.backup
  // ...
];
```

Before `PACKAGES`, `RUNTIME` registers hooks owned by the runtime itself. Its first entry is
`("runtime.agents", charliebot_agents::register_runtime)`. It installs `agent_environment` and then
`session_cgroups` as pre-recovery steps. This order matches `server.py:262-273`: the environment snapshot is
available before recovery, and cgroup startup logging and stale sweeping happen before recovery and early services.
No Python package owns these steps, so they do not appear in `PACKAGES`.

A package the port has not built calls a stub, and the entry carries a `// stub: <package>` marker. A stub that
carries Python's `register` data makes those registrations (its config section, its metadata slots, its backend
types). A stub with no data is `stub`, which registers nothing. `packages::registry()` calls `register_package` for
each entry; the first failure stops the server with exit code 1.

A package with a real port replaces its stub entry with its own `register` function. The registration calls in
`register` do not change.

## Hook kinds

Each entry gives the Python source, the Rust registration method, and one example registration. The `&mut self`
receiver is `Registry`, and every method returns `Result<(), RegistryError>` unless noted.

### Routers

- Python: `src/runtime/hooks/wiring.py:43` `register_router`
- Rust: `router(build: RouterBuilder, matches: PathMatch)`, where `RouterBuilder = fn(&Arc<FeatureContext>) -> axum::Router`
  and `PathMatch = fn(&str) -> bool`. The server merges the routers in registration order, after its own route families.
  The redirect for a trailing slash asks each `matches`.
- Example: `registry.router(notes_routes, is_notes_path)?;`

### WebSocket routes

- Python: `server.py:358` `@app.websocket`, `src/features/terminal/api.py:13`, and `src/features/voice/__init__.py:15` `ws_router`
- Rust: `websocket(pattern: &'static str, handler: WebSocketHandler)`. A `*` matches one non-empty decoded path segment.
  `WebSocketRequest` carries the decoded path, captured segments, raw query string, and axum upgrade. The server checks
  WebSocket auth before it calls the handler. A repeated pattern fails and names both packages.
- Example: `registry.websocket("/ws/voice/*", preview)?;`

### Services

- Python: `wiring.py:66` `register_service`
- Rust: `service(service: Arc<dyn Service>)`. `Service` has `name`, `phase` (`Phase::Early` or `Phase::Ready`),
  `start(ServiceContext) -> BoxFuture<anyhow::Result<()>>` and `stop() -> BoxFuture<anyhow::Result<()>>`.
  `ServiceContext` carries the `FeatureContext` and the crash-recovery future.
- Example: `registry.service(Arc::new(Indexer::default()))?;`

### File views

- Python: `wiring.py:80` `register_file_view`
- Rust: `file_view(name: &'static str, view: Arc<dyn FileView>)`. `FileView::view` answers a request path, or passes it on.
- Example: `registry.file_view("artifacts", Arc::new(ArtifactView))?;`

### Startup checks

- Python: `wiring.py:93` `register_startup_check`
- Rust: `startup_check(name: &'static str, check: CheckFn)`, where `CheckFn = fn(&CharlieBotConfig) -> Result<(), String>`.
  The server runs each check once, after the config loads, and refuses to start on an `Err`.
- Example: `registry.startup_check("publish_dir", check_publish_dir)?;`

### Config checks

- Python: `src/infra/config_registry.py:109` `register_config_check`
- Rust: `config_check(name: &'static str, check: CheckFn)`. The checks run after every config load, including a hot
  reload, which then keeps the previous config.
- Example: `registry.config_check("check_default_backend", check_default_backend)?;`

### Diff roots

- Python: `wiring.py:117` `register_diff_root`
- Rust: `diff_root(name: &'static str, root: DiffRootFn)`, where `DiffRootFn = fn(&CharlieBotConfig) -> PathBuf`.
- Example: `registry.diff_root("workspace", workspace_root)?;`

### Scheduled handlers

- Python: `src/runtime/hooks/scheduled_handlers.py:14` `register_handler`
- Rust: `scheduled_handler(name: &'static str, handler: ScheduledHandlerFn)`, where
  `ScheduledHandlerFn = fn(Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<String>>`. A cron task with
  `handler: <name>` awaits it, and the `Ok` string is the fire's summary.
- Example: `registry.scheduled_handler("publish_sweep", publish_sweep)?;`

### Config sections

- Python: `src/infra/config_registry.py:88` `register_config_section`
- Rust: `config_section(section: ConfigSection)`, where `ConfigSection { key, legacy_keys, parse }` and
  `parse: fn(&serde_yaml::Value) -> Result<SectionValue, String>`. The parsed value is read with
  `CharlieBotConfig::section::<T>(key)`, which panics on an unregistered key or a type mismatch. An absent section
  parses from an empty mapping, and a parse failure fails the config load.
- Example: `registry.config_section(ConfigSection { key: "voice", legacy_keys: VOICE_LEGACY, parse: parse_voice })?;`

### Legacy config keys

- Python: `config_registry.py:75` `register_legacy_keys`, and the `legacy_keys=` argument of `register_config_section`
- Rust: `legacy_keys(&[(&'static str, &'static str)])`. A key that moved reports its new location and fails the load.
  A `credentials: ` prefix names a path in `credentials.yaml`.
- Example: `registry.legacy_keys(&[("credentials: gemini_api_key", "gemini.api_key")])?;`

### Backend option models

- Python: `config_registry.py:42` `register_option_model`
- Rust: the `option_model: OptionModel` field of `BackendTypeRegistration`, where
  `OptionModel = fn(&serde_yaml::Mapping) -> Result<(), String>`. It checks one `backends.options[]` entry's
  type-specific keys. A failure fails the config load.
- Example: see `backend_type` below.

### Metadata slots

- Python: `src/infra/metadata_slot_registration.py` `register_metadata_fields`
- Rust: `metadata_slot(slot: MetadataSlot)`, where `MetadataSlot { owner, on: MetadataFile, after, fields }`. One owner
  registers once per file (`MetadataFile::Session` or `MetadataFile::Thread`). The store serializes the keys in
  registration order: `metadata_slots()` hands the list to `Store::new` at startup.
- Example: `registry.metadata_slot(MetadataSlot { owner: "cron", on: MetadataFile::Session, after: Some("backend"), fields })?;`

### Page packages

- Python: `src/runtime/hooks/page_render.py:30` `register_page_package`
- Rust: no method. `register_package` records the package after its `register` succeeds, so its `static/` mounts at
  `/static/<kind>/<name>/` and its `templates/` join the search path in list order. `page_parts(repo_root, packages, slot)`
  reads the part templates in that order.
- Example: none; the composition root's list is the registration.

### Template globals

- Python: `page_render.py:35` `register_template_global`
- Rust: `template_global(name: &'static str, global: TemplateGlobalFn)`, where
  `TemplateGlobalFn = fn(&FeatureContext) -> Result<minijinja::Value, String>`. The global reads the live config through the
  context. An `Err` fails the page render. `Templating::render_page` calls every global once per render.
- Example: `registry.template_global("voice_default_backend", voice_default_backend)?;`

### Home cards

- Python: `page_render.py:45` `register_home_card`
- Rust: `home_card(card: HomeCard)` with `HomeCard { name, url, description }`, all `&'static str`. Cards list in
  registration order.
- Example: `registry.home_card(HomeCard { name: "Usage", url: "/usage", description: "Token usage" })?;`

### Turn contributions

- Python: `src/runtime/hooks/turn_contributions.py:92` `register_turn_contribution`
- Rust: `turn_contribution(name: &'static str, contribution: Arc<dyn TurnContribution>)`. The trait has the default-able
  `instruction_segments`, `workflow_rules_file`, `input_preamble`, `event_renderers` and `claims_turn_end_unread`, and
  the async `before_turn` and `after_turn` hooks. The registry's readers run them in registration order. Two
  contributions that render the same event type fail.
- Example: `registry.turn_contribution("memory", Arc::new(MemoryTurn))?;`

### Sidebar contributions

- Python: `src/runtime/hooks/sidebar_contributions.py:54` `register_sidebar_contribution`
- Rust: `sidebar_contribution(name: &'static str, contribution: Arc<dyn SidebarContribution>)`. The trait has
  `watched_files`, `row_flags`, `copy_on_fork`, `drop_runtime_state` and `view_member`, each defaulted.
- Example: `registry.sidebar_contribution("trace", Arc::new(TraceSidebar))?;`

### Backend types

- Python: `src/runtime/hooks/backend_type_registration.py:49` `register_backend_type`
- Rust: `backend_type(registration: BackendTypeRegistration)`. The registration holds `type_name`, `traits: BackendTraits`
  (`resume`, `restart_reattach`, `preassigned_session_id`, `family_prefix`, `manual_compact`, `compact_hint`),
  `option_model`, `factory: Arc<dyn BackendFactory>`, `lifecycle: Option<Arc<dyn BackendLifecycle>>`, and
  `translate_fallback`. At most one type holds the translate fallback. A type without a lifecycle runs one process per
  run.
- Example: `registry.backend_type(BackendTypeRegistration { type_name: "codex", traits, option_model, factory, lifecycle: None, translate_fallback: false })?;`

### Running turns

- Python: `src/backends/claude_code/master_cc_relay.py:91-95` reads account labels from the other running manager turns.
- Rust: `LaunchContext.running_turns: Option<RunningTurnsFn>`, where `RunningTurnsFn` returns metadata for every other running manager turn.
- A manager turn receives `Some`; a task receives `None`. A task path that reads this field fails with an error naming `running_turns`.
- The callback reads the queue when called because Python probes transcript lineage before it reads the busy set.
- Example: `running_turns: Some(Arc::new(|| queue.running_turns_except(session_id)))`.

### Child environment

- Python: `src/runtime/hooks/backend_lifecycle_registration.py:25` `register_child_env`
- Rust: `child_env(name: &'static str, edit: ChildEnvFn)`, where `ChildEnvFn = fn(&mut Env)` and `Env` is a
  `BTreeMap<String, String>`. The edits apply in registration order to every agent child process.
- Example: `registry.child_env("home", set_home_env)?;`

### Reading limits

- Python: `backend_lifecycle_registration.py:49` `register_reading_limits`
- Rust: `reading_limits(kind: &'static str, limits: ReadingLimitsFn)`, where `ReadingLimitsFn = fn() -> ContextLimits`.
- Example: `registry.reading_limits("claude", claude_limits)?;`

### Usage resolvers

- Python: `backend_lifecycle_registration.py:71` `register_usage_resolver`
- Python: `src/runtime/session_usage.py:423-434` builds the registered resolver once per backend type with the config,
  events cache and chat-events path.
- Rust: `usage_resolver(backend_type: &'static str, constructor: UsageResolverConstructor)`, where
  `UsageResolverConstructor = Arc<dyn Fn(&UsageResolverHost) -> Arc<dyn UsageResolver> + Send + Sync>`.
  `UsageResolverHost` carries the hot-reloaded config and store; the store owns the events cache and chat-events path.
  The registry stores cloneable constructors only. The usage owner builds each resolver once per backend type and retains it.
- Example:
  ```rust
  let codex_home = host.codex_home();
  registry.usage_resolver(
    "codex",
    Arc::new(move |usage_host| resolver::usage_resolver(codex_home.clone(), usage_host)),
  )?;
  ```

### Usage sources

- Python: `src/runtime/hooks/usage_source_registration.py:32` `register_source`, and `:43` `attribute_backend_type`
- Rust: `usage_source(source: UsageSource)` and `attribute_backend_type(backend_type, source)`. A source declares the
  operations it defines in `UsageOps { logs, read, live_cli_sessions, quota_accounts, sweep }`, and the usage code calls
  only those, as Python's `hasattr` checks did. A call to an undeclared operation panics. An attribution may name a source
  that registers later, and it fails at use if the source never registers.
- Example: `registry.usage_source(UsageSource { name: "Codex", id_prefixes: &["codex"], run_logs_only: false, implementation: Some(Arc::new(CodexLogs)) })?;`

### Sequence controllers

- Python: `src/runtime/hooks/sequence_controllers.py:236` `register_sequence_controller`
- Rust: `sequence_controller(name: &'static str, controller: Arc<dyn SequenceController>)`. The trait has
  `sequence_kind`, `redrive`, `launch_context`, `after_run`, `recover_run`, `reconcile_interrupted`, `binding`,
  `owns_session`, `listing_fields` and the defaulted `move_bindings`. Two controllers that bind one session fail at use.
- Example: `registry.sequence_controller("scheduled", Arc::new(ScheduledSequences))?;`

`SequenceTree` joins the store's run records, task metadata, control lock and successor delivery with the dispatcher,
completion and session-event services. `StoreSequenceTree::new(store, runtime)` requires those runtime providers; the
executor is optional because startup and recovery can run before the execution adapter is installed. No controller is
registered by the registry crate. Its controllers receive the combined tree and implement the five run lifecycle
methods above.

`SequenceCompletion::restore_chain_locked` returns Python's `(restored, announcements)` pair
(`task_completion.py::restore_chain_locked`): the restored ids, topmost first, and one `(node, event, epoch)` triple per
`task_reopened` fact the call wrote. The caller sends each announcement with its epoch after it releases the control
lock. `restore_chain` takes the lock itself and returns the restored ids.

### Lifespan steps

- Python: `server.py:229` `lifespan`
- Rust: `pre_recovery_step(name, StepFn)`, `recovery_step(name, StepFn)` (at most one), `trigger_step(name, StepFn)` (at most
  one) and `let_go_step(name, StepFn)`, where `StepFn = fn(Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>>`.
- Order: the pre-recovery steps in registration order; the recovery step, started as a task before the early services
  start and awaited after them; the early services; the trigger step; the ready services. Shutdown stops the ready
  services in reverse, then the early ones in reverse, then runs the let-go steps in order.
- Example: `registry.recovery_step("sweep_orphans", sweep_orphans)?;`

## The typed startup state

`FeatureContext` holds the config and credentials (`Arc<HotReload<_>>`), the store, the registry and the
`StartupState`. The state holds one value per Rust type: a lifespan step or service inserts its value with `insert`, a
second value of one type fails, and a route handler reads it with `get` or `require`. A pre-recovery step that
fills a value makes it visible to a ready service, which reads it at start.

## Rendering pages

Two values in the startup state serve pages. `run()` inserts both before the lifespan starts, so a lifespan step or
a service reads them at start, and a route handler reads them per request.

- `RepoRoot` (`charliebot_core::repo_root`) is the checkout the server serves. `main.rs` reads the repo root variable
  once and validates it. Read it with `startup.require::<RepoRoot>()?` and `.path()` when a file lives in the checkout.
  A template needs no path: `render_page` searches `web/templates` and each page package's `templates/`.
- `Templating` (`charliebot_registry::templating`) is the page engine, built once at startup. Read it with
  `startup.require::<Templating>()?`, then call `render_page(features, name, context, request)`. It evaluates the
  registry's template globals, renders the template with `request` added to the context (the method, the raw path and
  the raw query), and returns the response: status 200 with `text/html; charset=utf-8`. `RenderError::kind()` names the
  Python exception class for the request log.

A feature page whose template is `src/features/notes/templates/notes.html`:

```rust
use axum::http::request::Parts;
use axum::response::Response;
use charliebot_registry::FeatureContext;
use charliebot_registry::templating::Templating;
use serde_json::json;

fn notes_page(features: &FeatureContext, request: &Parts) -> anyhow::Result<Response> {
  let templating = features.startup.require::<Templating>()?;
  let context = json!({"title": "Notes"});
  Ok(templating.render_page(features, "notes.html", &context, request)?)
}
```

The package registers `src.features.notes` and its template name must be unique across the search path, since a name
in two directories fails the first render.

## Store hooks

The store calls six traits (`AccountLabelKeeper`, `AnchorFieldReconciler`, `SequenceListingFields`,
`SequenceArchiveHook`, `SequenceBindingMover` and `RuntimeEventHooks`) and does not name any package. `store_hooks::store_hooks(registry)`
implements them over the registered lifecycles and sequence controllers, and the server passes the result to
`Store::new` at startup.

## Excluded from the seam

- CLI commands (`wiring.py:59` `register_command`) and setup steps (`wiring.py:104` `register_setup_step`). The CLI stays in
  Python (Trade-off 2), so the Rust server registers neither.

## What the stubs register

Packages whose port has not landed still register their data, so the server's behavior does not change:

- The `claude_code` package is ported. It registers typed account configuration, its config check, metadata slots,
  pooled lifecycle and the login-notice renderer. The `codex` package is ported too.
- Data-carrying stubs: `artifacts` (`publish` section), `cron` (session slot), `slack` (`slack` section, session slot
  and turn contribution), `discord` (`discord` section, session slot and turn contribution), `web_reply` (session slot
  and turn contribution), `code_server` (`code_server` section), `voice` (`voice` section, its two template globals
  and the default-backend check), `gemini` (two legacy keys), and the backend types `cc-openai-compatible`,
  `antigravity`, `charlie-code`, `gemini`, `cc-kimi` and `opencode`. Each type's factory refuses every launch with
  "not ported".
- Empty stubs: the other listed packages.
- `backend_pool` is ported, not a stub: its session slot comes from `charliebot_agents::backend_pool::register`.

Not registered yet:

- `code_server`'s template global for the diff page, which the Rust page does not port yet.
- Usage attributions and usage sources. The usage package is not ported.
- `cc-openai-compatible` and `cc-kimi` lifecycles. They use the default lifecycle, not the Python `ClaudeCliLifecycle`.
- WebSocket routes, services and startup checks. No stub makes them yet. The ported `files` package registers the
  routers, file views and cards, and the ported `memory` package registers the diff root `memory`.
