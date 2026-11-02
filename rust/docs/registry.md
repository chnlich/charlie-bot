# The plug-in registry

`charliebot-registry` is the plug-in seam. Each package makes its contributions through one
`register(&mut Registry)` function, and the server builds one `Registry` at startup. The registry is read-only after
that, except for one binding: the server binds its feature context right after building it (`bind_feature_context`),
and the store's hooks read that binding through a `Weak`.

Rules that hold for every hook kind:

- A registration names its hook. A name that another registration of the same kind holds fails. The error names the
  package that made the earlier registration, and the server does not start.
- A registration outside a package's `register` fails.
- The registries keep registration order. Routes merge, page parts list, metadata keys serialize, template paths
  search, and lifespan steps and services run in that order.
- A package appears once in the list. A second listing fails.

## Composition root

`rust/crates/app/packages/src/lib.rs` owns the package list, one entry per package. The list order is the server's
registration order:

```rust
pub const PACKAGES: &[(&str, RegisterFn)] = &[
  ("src.features.artifacts", charliebot_artifacts::register),
  ("src.features.backup", charliebot_backup::register),
  // ...
];
```

Before `PACKAGES`, `RUNTIME` registers hooks owned by the runtime itself. Its first entry is
`("runtime.agents", charliebot_agents::register_runtime)`. It installs `agent_environment` and then
`session_cgroups` as pre-recovery steps: the environment snapshot is available before recovery, and cgroup startup
logging and stale sweeping happen before recovery and early services.
`("runtime.usage", charliebot_usage::register)` follows them and inserts the one `SessionUsage` into startup state
before the session API routes serve requests. The runtime owns these steps, so they do not appear in
`PACKAGES`.

Each entry names its package's `register` function. `packages::registry()` calls `register_package` for each entry;
the first failure stops the server with exit code 1.

## Hook kinds

Each entry gives the Rust registration method and one example registration. The `&mut self`
receiver is `Registry`, and every method returns `Result<(), RegistryError>` unless noted.

### Routers

- Rust: `router_patterns(build: RouterBuilder, patterns: &[&str])`, where `RouterBuilder = fn(&Arc<FeatureContext>) -> axum::Router`
  and the patterns are the route patterns the router mounts, in axum's syntax. matchit matches these patterns and
  rejects empty parameter captures. `router(build, matches: PathMatch)` takes a `PathMatch = fn(&str) -> bool` for a
  router whose claims are not route patterns. The server merges the routers in registration order, after its own route
  families. The redirect for a trailing slash asks each router's claim.
- Example: `registry.router_patterns(notes_routes, &["/api/notes/{note_id}"])?;`

### WebSocket routes

- Rust: `websocket(pattern: &'static str, handler: WebSocketHandler)`. A `*` matches one non-empty decoded path segment.
  `WebSocketRequest` carries the decoded path, captured segments, raw query string, and axum upgrade. The server checks
  WebSocket auth before it calls the handler. A repeated pattern fails and names both packages.
- Example: `registry.websocket("/ws/voice/*", preview)?;`

### Services

- Rust: `service(service: Arc<dyn Service>)`. `Service` has `name`, `phase` (`Phase::Early` or `Phase::Ready`),
  `start(ServiceContext) -> BoxFuture<anyhow::Result<()>>` and `stop() -> BoxFuture<anyhow::Result<()>>`.
  `ServiceContext` carries the `FeatureContext` and the crash-recovery future.
- Example: `registry.service(Arc::new(Indexer::default()))?;`

### File views

- Rust: `file_view(name: &'static str, view: Arc<dyn FileView>)`. `FileView::view` receives the request's
  `FeatureContext`, request parts and path. It answers the request or passes it on.
- Example: `registry.file_view("artifacts", Arc::new(ArtifactView))?;`

### Startup checks

- Rust: `startup_check(name: &'static str, check: StartupCheckFn)`, where
  `StartupCheckFn = fn(&Registry, &CharlieBotConfig) -> Result<(), String>`. The check reads the registry, so it can
  resolve the usage sources and backend attributions the packages registered. The server runs each check once, after
  the config loads, and refuses to start on an `Err`.
- Example: `registry.startup_check("backend_ids", check_backend_ids)?;`

### Config checks

- Rust: `config_check(name: &'static str, check: CheckFn)`. The checks run after every config load, including a hot
  reload, which then keeps the previous config.
- Example: `registry.config_check("check_default_backend", check_default_backend)?;`

### Diff roots

- Rust: `diff_root(name: &'static str, root: DiffRootFn)`, where `DiffRootFn = fn(&CharlieBotConfig) -> PathBuf`.
- Example: `registry.diff_root("workspace", workspace_root)?;`

### Scheduled handlers

- Rust: `scheduled_handler(name: &'static str, handler: ScheduledHandlerFn)`, where
  `ScheduledHandlerFn = fn(Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<String>>`. A cron task with
  `handler: <name>` awaits it, and the `Ok` string is the fire's summary.
- Example: `registry.scheduled_handler("publish_sweep", publish_sweep)?;`

### Config sections

- Rust: `config_section(section: ConfigSection)`, where `ConfigSection { key, legacy_keys, parse }` and
  `parse: fn(&serde_yaml::Value) -> Result<SectionValue, String>`. The parsed value is read with
  `CharlieBotConfig::section::<T>(key)`, which panics on an unregistered key or a type mismatch. An absent section
  parses from an empty mapping, and a parse failure fails the config load.
- Example: `registry.config_section(ConfigSection { key: "voice", legacy_keys: VOICE_LEGACY, parse: parse_voice })?;`

### Legacy config keys

- Rust: `legacy_keys(&[(&'static str, &'static str)])`. A key that moved reports its new location and fails the load.
  A `credentials: ` prefix names a path in `credentials.yaml`.
- Example: `registry.legacy_keys(&[("credentials: gemini_api_key", "gemini.api_key")])?;`

### Backend option models

- Rust: the `option_model: OptionModel` field of `BackendTypeRegistration`, where
  `OptionModel = fn(&serde_yaml::Mapping) -> Result<(), String>`. It checks one `backends.options[]` entry's
  type-specific keys. A failure fails the config load.
- Example: see `backend_type` below.

### Metadata slots

- Rust: `metadata_slot(slot: MetadataSlot)`, where `MetadataSlot { owner, on: MetadataFile, after, fields }`. One owner
  registers once per file (`MetadataFile::Session` or `MetadataFile::Thread`). The store serializes the keys in
  registration order: `metadata_slots()` hands the list to `Store::new` at startup.
- Example: `registry.metadata_slot(MetadataSlot { owner: "cron", on: MetadataFile::Session, after: Some("backend"), fields })?;`

### Page packages

- Rust: no method. `register_package` records the package after its `register` succeeds, so its `static/` mounts at
  `/static/<kind>/<name>/` and its `templates/` join the search path in list order. `page_parts(repo_root, packages, slot)`
  reads the part templates in that order.
- Example: none; the composition root's list is the registration.

### Template globals

- Rust: `template_global(name: &'static str, global: TemplateGlobalFn)`, where
  `TemplateGlobalFn = fn(&FeatureContext) -> Result<minijinja::Value, String>`. The global reads the live config through the
  context. An `Err` fails the page render. `Templating::render_page` calls every global once per render.
- Example: `registry.template_global("voice_default_backend", voice_default_backend)?;`

### Home cards

- Rust: `home_card(card: HomeCard)` with `HomeCard { name, url, description }`, all `&'static str`. Cards list in
  registration order.
- Example: `registry.home_card(HomeCard { name: "Usage", url: "/usage", description: "Token usage" })?;`

### Turn contributions

- Rust: `turn_contribution(name: &'static str, contribution: Arc<dyn TurnContribution>)`. The trait's one required
  method is `class_name()`, the name the after-turn task name carries. The other steps default to
  adding nothing: `instruction_segments(&self, meta: &SessionMetadata, kind: &str, cfg: &CharlieBotConfig,
  repo: &RepoRoot)`, `workflow_rules_file`, `input_preamble`, `event_renderers`, `claims_turn_end_unread`, and the
  async `before_turn` and `after_turn(meta, done_event, cfg, features)` hooks. `repo` is the checkout the server serves
  (`RepoRoot`); a contribution reads its prompt docs through it, and `Registry::instruction_segments` takes the same
  `repo` argument and passes it to each contribution. The registry's readers run them in registration order. Two
  contributions that render the same event type fail.
- Example: `registry.turn_contribution("memory", Arc::new(MemoryTurn))?;`
- After-turn work: `RegistryRuntimeEvents::after_turn` (`store_hooks.rs`) answers one work per contribution, in
  registration order. The store starts each work as its own task after it broadcasts a `master_done` of a session that
  exists. The task is named `after-turn-<class_name>-<session id>`. A task that fails logs
  `background_task_failed` with that `task_name`, and the other tasks run on.
- The context: each work needs the `FeatureContext`. The server calls `Registry::bind_feature_context` once, right after
  it builds the context and before the lifespan starts, because the lifespan's recovery can finish a round. The registry
  keeps a `Weak` link, so the store never holds the context. When no context is bound, or the bound one was dropped,
  every work fails with a message that names the `FeatureContext`.
- Round origins: every `master_done` is persisted by the master queue's round end (`master_queue.rs:538`). That covers a
  chat turn, a task turn, an auto-trigger, and a turn re-attached after a restart,
  so each reaches the hook. The announce path (`announce_appended_event`) carries no `master_done` and starts no
  after-turn work.

### Sidebar contributions

- Rust: `sidebar_contribution(name: &'static str, contribution: Arc<dyn SidebarContribution>)`. The trait has
  `watched_files`, `row_flags`, `copy_on_fork` and `view_member`, each defaulted.
- Example: `registry.sidebar_contribution("trace", Arc::new(TraceSidebar))?;`

### Backend types

- Rust: `backend_type(registration: BackendTypeRegistration)`. The registration holds `type_name`, `traits: BackendTraits`
  (`resume`, `restart_reattach`, `preassigned_session_id`, `family_prefix`, `manual_compact`, `compact_hint`),
  `option_model`, `factory: Arc<dyn BackendFactory>`, `lifecycle: Option<Arc<dyn BackendLifecycle>>`, and
  `translate_fallback`. At most one type holds the translate fallback. A type without a lifecycle runs one process per
  run.
- Example: `registry.backend_type(BackendTypeRegistration { type_name: "codex", traits, option_model, factory, lifecycle: None, translate_fallback: false })?;`
- `LaunchContext.base_env: Arc<Env>` carries the base agent environment into lifecycle launches and compaction.
- `LaunchContext.run_kind: RunKind` carries `manager_turn` for a manager launch and the persisted run kind for a worker launch.
- Three seams that a backend type uses beyond `AgentBackend`'s command, environment and translator:
  - **One-shot.** `AgentBackend::one_shot_text(base_env, prompt, system, timeout)` returns
    `Option<BoxFuture<..>>`: `Some` is the CLI-native, tool-less one-shot, and `None` means the type has none (the
    default). Callers never call the trait method; `charliebot_agents::one_shot_text(built, base_env, prompt, system,
    timeout)` runs the native one-shot when present and `agent_one_shot_text` otherwise.
  - **Uploads.** `RunRequest.uploaded_files: Vec<Value>` carries the message's attachment refs. The runner calls
    `AgentBackend::accept_uploads(&uploaded_files)` before the cwd, the transport and the process. `Err(message)` ends
    the run with exactly one error event and no process; the outcome's exit code is `None`. The default accepts every
    upload.
  - **Final stdout.** `TransportKind::FinalStdout` runs the process with a piped stdout, which the runner tees into
    `stdout.log` (truncated at open), and with stderr streamed to `stderr.log`. After the exit the runner calls
    `AgentBackend::finish_final_stdout(stdout, stderr, exit_code)`, which returns a `FinalStdoutOutcome { events,
    exit_code, guard_error }`. The runner emits the events, records `exit_code` (`AgentRunner::exit_code()`), and then
    fails the run with `guard_error` when it is set.

### Running turns

- Rust: `LaunchContext.running_turns: Option<RunningTurnsFn>`, where `RunningTurnsFn` returns metadata for every other running manager turn.
- `LaunchContext.session_meta: Arc<Mutex<SessionMetadata>>` is shared by the master queue's running-turn entry and the turn's lifecycle calls.
- Lifecycle account-label setters and the queue's anchor refresh write the shared metadata object.
- No `std::sync::Mutex` guard crosses `.await`; `clippy::await_holding_lock`, denied by `-D warnings` in the acceptance command, enforces this rule.
- A manager turn receives `Some`; a task receives `None`. A task path that reads this field fails with an error naming `running_turns`.
- The callback reads the queue when it is called, not when the launch context is built.
- Example: `running_turns: Some(Arc::new(|| queue.running_turns_except(session_id)))`.

### Child environment

- Rust: `child_env(name: &'static str, edit: ChildEnvFn)`, where `ChildEnvFn = fn(&mut Env)` and `Env` is a
  `BTreeMap<String, String>`. The edits apply in registration order to every agent child process.
- Example: `registry.child_env("home", set_home_env)?;`

### Reading limits

- Rust: `reading_limits(kind: &'static str, limits: ReadingLimitsFn)`, where `ReadingLimitsFn = fn(RunKind) -> ContextLimits`.
- Example: `registry.reading_limits("claude", claude_limits)?;`

### Usage resolvers

- Rust: `usage_resolver(backend_type: &'static str, constructor: UsageResolverConstructor)`, where
  `UsageResolverConstructor = Arc<dyn Fn(&UsageResolverHost) -> Arc<dyn UsageResolver> + Send + Sync>`.
  `UsageResolverHost` carries the hot-reloaded config and store; the store owns the events cache and chat-events path.
  The registry stores cloneable constructors only. `charliebot-usage::SessionUsage` builds each resolver once per
  backend type and retains it.
- Example:
  ```rust
  let codex_home = host.codex_home();
  registry.usage_resolver(
    "codex",
    Arc::new(move |usage_host| resolver::usage_resolver(codex_home.clone(), usage_host)),
  )?;
  ```

### Usage sources

- Rust: `usage_source(source: UsageSource)` and `attribute_backend_type(backend_type, source)`. A source declares the
  operations it defines in `UsageOps { logs, read, live_cli_sessions, quota_accounts, sweep }`, and the usage code calls
  only those. A call to an undeclared operation panics. An attribution may name a source
  that registers later, and it fails at use if the source never registers.
- `UsageSourceImpl::logs(&self, cfg: &CharlieBotConfig) -> anyhow::Result<Vec<(PathBuf, String)>>` receives the loaded
  config. The source reads config through `cfg`.
- `UsageSourceImpl::quota_accounts(&self, cfg: &CharlieBotConfig) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>>` derives
  accounts from the loaded config. `Registry::quota_accounts` returns the first source error.
- `QuotaAccount::fetch<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Option<Map<String, Value>>>>` returns `Ok(Some(_))`
  for a reading, `Ok(None)` for no reading, and `Err` for a failed fetch.
- The poll loop logs each fetch error once as `ext_usage_poll_error`, with provider, account, and error, then skips that
  account. A `quota_accounts` error skips the whole round.
- Example: `registry.usage_source(UsageSource { name: "Codex", id_prefixes: &["codex"], run_logs_only: false, implementation: Some(Arc::new(CodexLogs)) })?;`

### Sequence controllers

- Rust: `sequence_controller(name: &'static str, controller: Arc<dyn SequenceController>)`. The trait has
  `sequence_kind`, `redrive`, `launch_context`, `after_run`, `recover_run`, `reconcile_interrupted`, `binding`,
  `owns_session`, `listing_fields` and the defaulted `move_bindings`. Two controllers that bind one session fail at use.
- Example: `registry.sequence_controller("scheduled", Arc::new(ScheduledSequences))?;`

`SequenceTree` joins the store's run records, task metadata, control lock and successor delivery with the dispatcher,
completion and session-event services. `StoreSequenceTree::new(store, runtime)` requires those runtime providers; the
executor is optional because startup and recovery can run before the execution adapter is installed. No controller is
registered by the registry crate. Its controllers receive the combined tree and implement the five run lifecycle
methods above.

`SequenceCompletion::restore_chain_locked` returns the `(restored, announcements)` pair:
the restored ids, topmost first, and one `(node, event, epoch)` triple per
`task_reopened` fact the call wrote. The caller sends each announcement with its epoch after it releases the control
lock. `restore_chain` takes the lock itself and returns the restored ids.

### Lifespan steps

- Rust: `pre_recovery_step(name, StepFn)`, `recovery_step(name, StepFn)` (at most one), `trigger_step(name, StepFn)` (at most
  one) and `let_go_step(name, StepFn)`, where `StepFn = fn(Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>>`.
- Order: the pre-recovery steps in registration order; the recovery step, started as a task before the early services
  start and awaited after them; the early services; the trigger step; the ready services. Shutdown stops ready services
  in reverse order, stops early services in reverse order, closes shared clients, releases the fence, and logs
  `charliebot_shutdown`. On SIGINT and failed bind, the server then runs let-go steps in
  registration order and exits 0 or 1. SIGTERM skips let-go and is re-raised under its
  default action. The registered order is `task_execution`, `headless_renderer`, then
  `speech`.
- Example: `registry.recovery_step("sweep_orphans", sweep_orphans)?;`

## Route helpers

`caller::resolve_caller(headers: &HeaderMap, features: &FeatureContext) -> Result<CallerIdentity, Response>` resolves
the operator or active agent identity from request headers, the live `FeatureContext.credentials` and the store's run
identity. `CallerIdentity` exposes `is_operator()`, `session_id()` and `claims()`, and implements
`FromRequestParts<Arc<FeatureContext>>` for route extractors. A handler that checks the caller takes this extractor.

`task_errors::task_http_error(error: StoreError) -> Response` is the shared HTTP mapping for task-domain errors. It
maps refusal variants to their HTTP status and detail, conflicts include their message and blockers, and failures use
the ordinary unhandled-exception response. Every task-error answer from a route uses this function.

`request_body::read(&headers, body).await` reads a route's body before its dependencies. It decodes only
`application/json` and `application/*+json` bodies through `json_loads`; other
content types stay raw bytes. After the dependencies succeed, `RequestBody::validate(&model)` checks the body model
and returns its declared fields. A handler keeps that order:

```rust
async fn update(request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let session = match require_session(&parts).await {
    Ok(session) => session,
    Err(response) => return response,
  };
  let model = match body.validate(&SESSION_BODY) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  update_session(session, model).await
}
```

`form_body::read(&headers, body).await` streams multipart file parts to temporary files before route dependencies run.
After the dependencies succeed, `FormBody::validate(&model)` checks required and typed `File` and `Form` fields in
declaration order. A handler then reads the parsed values:

```rust
async fn upload(request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let form = match form_body::read(&parts.headers, stream).await {
    Ok(form) => form,
    Err(response) => return *response,
  };
  let session = match require_session(&parts).await {
    Ok(session) => session,
    Err(response) => return response,
  };
  if let Err(response) = form.validate(&UPLOAD_FORM) {
    return *response;
  }
  let file = form.file("file").expect("the required file field validated");
  save_upload(session, file).await
}
```

A model forbids extra keys with `BodyModel::required(FIELDS).forbid_extra()`. Each `Field`
declares its bounds as builders: `ge`, `gt` and `le` on an `Integer`, `NullableInteger` or `NullableFloat` field,
with a `Bound::Int` or `Bound::Float` bound, and `min_length` and `max_length` on a `String` or `StringList` field. `validate` reports the
pinned validation errors for the body, in a fixed order: the declared fields in declaration order, then
the forbidden keys in input order. A route takes these errors from `validate` and does not re-derive them. The module
doc of `charliebot_compat::request_body` gives the order in full.

## The typed startup state

`FeatureContext` holds the config and credentials (`Arc<HotReload<_>>`), the store, the registry and the
`StartupState`. The state holds one value per Rust type: a lifespan step or service inserts its value with `insert`, a
second value of one type fails, and a route handler reads it with `get` or `require`. A pre-recovery step that
fills a value makes it visible to a ready service, which reads it at start.
`AgentBaseEnvironment` is the startup state's one owner of the base agent environment. Code with a `FeatureContext`
reads it through `features.startup.require::<AgentBaseEnvironment>()?`; one-shot calls and launch contexts carry the
environment explicitly.
`RunningTurnInputs` (`charliebot_registry::RunningTurnInputs`) answers the input event ids of a session's running turn.
The task runtime inserts it in the pre-recovery step
that inserts `MasterQueue`, and it reads the same queue. Its one method is `user_event_ids(&self, session_id: &str) ->
Vec<String>`, which is empty when no turn runs. The web reply package reads it with
`features.startup.require::<RunningTurnInputs>()?`; a missing entry is an error.

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
  failure's exception class for the request log.

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

- CLI commands and setup steps. The `charliebot` CLI owns its subcommands outside the registry, so the server
  registers neither.
