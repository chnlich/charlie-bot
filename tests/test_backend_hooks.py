"""Backend type table and lifecycle hooks: what the runtime reads from a registered backend package.

Covers the registration contract (src/runtime/hooks/backend_types.py), the import isolation of the
runtime (a registration costs no backend import), the Claude CLI lifecycles of the variants and of
the non-pooled cc-claude entry (src/backends/claude_code/claude_lifecycle.py), and the
``quota_exhausted`` flag that a refused launch copies onto its error event, for turns and tasks.
"""

import json
import pathlib
import subprocess
import sys
import types
import uuid

import conftest
import pytest

from src.backends.claude_code import claude_accounts, claude_code, claude_relay
from src.features.discord import metadata as discord_metadata
from src.features.slack import metadata as slack_metadata
from src.infra import config, models
from src.infra import event_types as ET
from src.runtime import master_cc_run, worker
from src.runtime.hooks import backend_lifecycle, backend_type_registration, backend_types

CC_ID = "11111111-2222-3333-4444-555555555555"
CLI_VARIANT_TYPES = ["cc-anthropic-compatible", "cc-openai-compatible"]

_fresh_pool_state = conftest.fresh_state_fixture(conftest.reset_claude_account_readings)


def _variant_option(backend_type: str):
  extra = {"api_base": "http://test.invalid"}
  if backend_type == "cc-anthropic-compatible":
    extra["context_window"] = 409600
  return conftest.backend_option(id="variant", label="Variant", type=backend_type, model="claude-sonnet-4-5", **extra)


def _turn_context(
    cfg: config.CharlieBotConfig,
    option,
    held_native_id: str | None,
    tmp_path: pathlib.Path,
    *,
    meta: models.SessionMetadata | None = None) -> backend_lifecycle.LaunchContext:
  meta = meta or models.SessionMetadata(
      profile="manager", id="s1", name="t", backend=option.id, cc_session_id=held_native_id)
  return master_cc_run._turn_launch_context(
      conftest.make_work_item(cfg, meta, option), option, str(tmp_path), held_native_id)


# ---------------------------------------------------------------------------
# The registration contract
# ---------------------------------------------------------------------------


def test_registering_a_type_twice_raises() -> None:
  traits = backend_types.traits_for("codex")

  with pytest.raises(ValueError, match="already registered"):
    backend_type_registration.register_backend_type(
        "codex",
        options="src.backends.codex.options:CodexBackend",
        factory="src.backends.codex.factory:build",
        traits=traits)


def test_an_unregistered_type_raises_naming_the_type() -> None:
  option = types.SimpleNamespace(type="no-such-backend")

  with pytest.raises(ValueError, match="no-such-backend"):
    backend_types.build_backend(option, config.CharlieBotConfig())
  with pytest.raises(ValueError, match="no-such-backend"):
    backend_types.traits_for("no-such-backend")


def test_a_type_without_a_lifecycle_gets_the_default() -> None:
  option = conftest.backend_option(id="c", label="C", type="charlie-code", model="m")

  assert type(backend_types.lifecycle_for(option)) is backend_lifecycle.BackendLifecycle


def test_registering_the_packages_imports_no_backend_module() -> None:
  """A fresh interpreter that registers every package holds no module of any backend package
  besides the packages themselves: the factories and lifecycles import on first use."""
  script = "\n".join(
      [
          "import sys",
          "from src.app import registrations",
          "registrations.register_all()",
          "from src.runtime.hooks import backend_lifecycle, backend_type_registration, backend_types",
          "registry = backend_type_registration.registrations()",
          "factory_modules = sorted({r.factory.partition(':')[0] for r in registry.values()})",
          "loaded = [m for m in factory_modules if m in sys.modules]",
          "backend_modules = sorted(m for m in sys.modules if m.startswith('src.backends.') and m.count('.') >= 3)",
          "print(len(factory_modules), loaded, backend_modules)",
      ])

  result = subprocess.run(
      [sys.executable, "-c", script], cwd=conftest.ROOT, capture_output=True, text=True, check=True, timeout=60)

  assert result.stdout.strip() == f"{len(backend_type_registration.registrations())} [] []"


# ---------------------------------------------------------------------------
# The Claude CLI variants and the non-pooled cc-claude entry
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("backend_type", CLI_VARIANT_TYPES)
@pytest.mark.asyncio
async def test_cli_variant_turn_checks_the_transcript_and_adds_the_dynamic_sections_flag(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, backend_type: str) -> None:
  config_dir = tmp_path / "claude-default"
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(config_dir))
  option = _variant_option(backend_type)
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home", backends={"options": [option]})
  lifecycle = backend_types.lifecycle_for(option)

  missing_ctx = _turn_context(cfg, option, CC_ID, tmp_path)
  missing = await lifecycle.place(missing_ctx)
  conftest.make_transcript(config_dir, CC_ID)
  present_ctx = _turn_context(cfg, option, CC_ID, tmp_path)
  present = await lifecycle.place(present_ctx)
  fresh_ctx = _turn_context(cfg, option, None, tmp_path)
  fresh = await lifecycle.place(fresh_ctx)

  assert missing.resume_id is None
  assert present.resume_id == CC_ID
  assert fresh.resume_id is None
  for ctx, launch in ((missing_ctx, missing), (present_ctx, present), (fresh_ctx, fresh)):
    assert launch.backend_kwargs == {
        "extra_flags": [claude_code.EXCLUDE_DYNAMIC_SECTIONS_FLAG],
        "run_kind": "manager_turn",
        "midturn_delivery": True,
        "midturn_run_id": ctx.run_id,
    }


@pytest.mark.parametrize("backend_type", CLI_VARIANT_TYPES)
def test_cli_variant_round_reports_a_reply_from_outside_the_pinned_family(backend_type: str) -> None:
  option = _variant_option(backend_type)
  lifecycle = backend_types.lifecycle_for(option)
  outside = conftest.assistant_text_event("hi")
  outside["message"]["model"] = "claude-haiku-4-5"
  inside = conftest.assistant_text_event("hi")
  inside["message"]["model"] = "claude-sonnet-4-5"

  notices = lifecycle.round_notices(option, [outside])

  assert [(n["type"], n["backend"], n["served_models"]) for n in notices] == [
      (ET.MODEL_FALLBACK_NOTICE, option.id, ["claude-haiku-4-5"])
  ]
  assert lifecycle.round_notices(option, [inside]) == []


@pytest.mark.asyncio
async def test_non_pooled_cc_claude_turn_checks_the_transcript_and_selects_no_account(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  config_dir = tmp_path / "claude-default"
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(config_dir))

  def _no_selection(*args: object, **kwargs: object) -> None:
    raise AssertionError("a non-pooled turn must not select an account")

  monkeypatch.setattr(claude_accounts, "select", _no_selection)
  option = conftest.backend_option(id="cc", label="CC", type="cc-claude", model="claude-opus-4-8")
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home", backends={"options": [option]})
  lifecycle = backend_types.lifecycle_for(option)

  missing_ctx = _turn_context(cfg, option, CC_ID, tmp_path)
  missing = await lifecycle.place(missing_ctx)
  conftest.make_transcript(config_dir, CC_ID)
  present_ctx = _turn_context(cfg, option, CC_ID, tmp_path)
  present = await lifecycle.place(present_ctx)

  assert (missing.resume_id, present.resume_id) == (None, CC_ID)
  for ctx, launch in ((missing_ctx, missing), (present_ctx, present)):
    assert launch.account_label is None
    assert launch.backend_kwargs == {
        "extra_flags": [claude_code.EXCLUDE_DYNAMIC_SECTIONS_FLAG],
        "run_kind": "manager_turn",
        "midturn_delivery": True,
        "midturn_run_id": ctx.run_id,
    }
  assert lifecycle.watch(_turn_context(cfg, option, CC_ID, tmp_path), present) is None


@pytest.mark.asyncio
async def test_pooled_relay_turn_uses_the_same_midturn_launch_arguments(tmp_path: pathlib.Path) -> None:
  cfg = conftest.fable_pool_cfg(tmp_path)
  option = cfg.get_backend_option(conftest.POOLED_FABLE_ID)
  assert option is not None
  ctx = _turn_context(cfg, option, None, tmp_path)
  [account, *_] = cfg.accounts.claude
  lifecycle = backend_types.lifecycle_for(option)

  first = lifecycle._pooled_launch(ctx, account, resume_id=None, prompt=None, relays=0)
  relay = lifecycle._pooled_launch(ctx, account, resume_id=None, prompt="continue", relays=1)

  assert first.backend_kwargs["midturn_delivery"] is True
  assert relay.backend_kwargs["midturn_delivery"] is True
  assert first.backend_kwargs["midturn_run_id"] == ctx.run_id
  assert relay.backend_kwargs["midturn_run_id"] == ctx.run_id


@pytest.mark.parametrize("thread_kind", ["slack", "discord"])
@pytest.mark.asyncio
async def test_thread_turn_keeps_plain_text_stdin_and_queues_later_messages(
    tmp_path: pathlib.Path, thread_kind: str) -> None:
  option = conftest.backend_option(id="cc", label="CC", type="cc-claude", model="claude-haiku-4-5")
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home", backends={"options": [option]})
  thread_fields = (
      {
          "slack_origin": slack_metadata.SlackOrigin(team_id="T", channel_id="C", thread_ts="1.0")
      } if thread_kind == "slack" else {
          "discord_origin": discord_metadata.DiscordOrigin(guild_id="G", parent_channel_id="C", thread_id="1")
      })
  meta = models.SessionMetadata(profile="manager", id="s1", name="thread", backend=option.id, **thread_fields)
  ctx = _turn_context(cfg, option, None, tmp_path, meta=meta)
  launch = await backend_types.lifecycle_for(option).place(ctx)
  backend = backend_types.build_backend(option, cfg, **launch.backend_kwargs, cgroup_session_id=meta.id)

  assert launch.backend_kwargs["midturn_delivery"] is False
  assert "--input-format" not in backend._build_command("prompt")
  assert "--replay-user-messages" not in backend._build_command("prompt")


@pytest.mark.parametrize("backend_type", ["codex", "cc-anthropic-compatible"])
@pytest.mark.asyncio
async def test_non_claude_task_build_receives_no_claude_account(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, backend_type: str) -> None:
  option = _variant_option(backend_type) if backend_type == "cc-anthropic-compatible" else conftest.backend_option(
      id="variant", label="Variant", type="codex", model="m")
  backend = conftest.ScriptedRelayBackend([conftest.assistant_text_event("done")], exit_code=0)
  builds = conftest.install_scripted_backends(monkeypatch, [backend], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  built_worker = worker.Worker(
      models.ThreadMetadata(id="t1", session_id="s1", description="task"),
      tmp_path / "work",
      tmp_path / "data" / "events.jsonl",
      "do the thing",
      config.CharlieBotConfig(charliebot_home=tmp_path / "home"),
      backend_option=option,
      session_meta=models.SessionMetadata(profile="manager", id="s1", name="S", backend=option.id),
      run_kind="work",
  )

  assert await built_worker.run() == 0

  assert "claude_account" not in builds[0]["kwargs"]


def _task_context(
    cfg: config.CharlieBotConfig,
    option,
    held_native_id: str | None,
    tmp_path: pathlib.Path,
    run_kind: models.RunKind = "work") -> backend_lifecycle.LaunchContext:

  async def unused(*args: object) -> None:
    raise AssertionError("placement emits nothing")

  meta = models.SessionMetadata(profile="worker", id="s1", name="t", backend=option.id)
  return backend_lifecycle.LaunchContext(
      cfg=cfg,
      option=option,
      session_meta=meta,
      kind="task",
      run_kind=run_kind,
      cwd=str(tmp_path),
      held_native_id=held_native_id,
      preassigned_native_id="99999999-2222-3333-4444-555555555555",
      emit=unused,
      record_account=unused,
      context_state=unused)


@pytest.mark.asyncio
async def test_pooled_task_resume_starts_on_the_login_holding_the_transcript(tmp_path: pathlib.Path) -> None:
  cfg = conftest.fable_pool_cfg(tmp_path)
  option = next(o for o in cfg.backends.options if o.id == conftest.POOLED_FABLE_ID)
  lifecycle = backend_types.lifecycle_for(option)

  # A held conversation no login holds is refused: the continued run's prompt
  # is only the new input, so a fresh process would work without the task spec.
  with pytest.raises(backend_lifecycle.LaunchRefused, match="has no transcript") as refused:
    await lifecycle.place(_task_context(cfg, option, CC_ID, tmp_path))
  assert refused.value.quota_exhausted is False

  conftest.make_transcript(tmp_path / "claude-ext-2", CC_ID)
  resumed = await lifecycle.place(_task_context(cfg, option, CC_ID, tmp_path))
  fresh = await lifecycle.place(_task_context(cfg, option, None, tmp_path))

  assert resumed.resume_id == CC_ID and resumed.account_label == "ext-2"
  # The resumed process opens no new conversation id; the fresh one opens the runtime-chosen id.
  assert "claude_session_id" not in resumed.backend_kwargs
  assert fresh.resume_id is None
  assert fresh.backend_kwargs["claude_session_id"] == "99999999-2222-3333-4444-555555555555"


# ---------------------------------------------------------------------------
# quota_exhausted on the launch error event: master turns
# ---------------------------------------------------------------------------


def _rejected_run() -> conftest.ScriptedRelayBackend:
  return conftest.ScriptedRelayBackend([conftest.rate_limit_event("rejected", 1.0)], exit_code=1)


# A turn scenario returns the pooled config and the backends that serve the turn's processes.
def _all_logins_rejected(tmp_path: pathlib.Path,
                         monkeypatch: pytest.MonkeyPatch) -> tuple[config.CharlieBotConfig, list]:
  cfg = conftest.fable_pool_cfg(tmp_path)
  for label in ("main", "ext-1", "ext-2"):
    conftest.reject_claude_login(label)
  return cfg, []


def _every_login_failed(tmp_path: pathlib.Path,
                        monkeypatch: pytest.MonkeyPatch) -> tuple[config.CharlieBotConfig, list]:
  cfg = conftest.fable_pool_cfg(tmp_path)
  for label in ("main", "ext-1", "ext-2"):
    conftest.write_pool_credentials(tmp_path / f"claude-{label}", access_token="")
  return cfg, []


def _pool_spent_at_the_relay(tmp_path: pathlib.Path,
                             monkeypatch: pytest.MonkeyPatch) -> tuple[config.CharlieBotConfig, list]:
  cfg = conftest.fable_pool_cfg(tmp_path)
  for label in ("ext-1", "ext-2"):
    conftest.write_pool_credentials(tmp_path / f"claude-{label}", access_token="")
  return cfg, [_rejected_run()]


def _fail_every_move(monkeypatch: pytest.MonkeyPatch) -> None:

  def _fail(*args: object, **kwargs: object) -> None:
    raise claude_accounts.TranscriptMoveError("disk full")

  monkeypatch.setattr(claude_accounts, "move_transcript", _fail)


def _relay_move_fails(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> tuple[config.CharlieBotConfig, list]:
  _fail_every_move(monkeypatch)
  return conftest.fable_pool_cfg(tmp_path), [_rejected_run()]


def _relay_limit(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> tuple[config.CharlieBotConfig, list]:
  cfg = conftest.fable_pool_cfg(tmp_path, labels=("main", "a", "b", "c"))
  return cfg, [_rejected_run() for _ in range(claude_relay.MAX_RELAYS_PER_TURN + 1)]


# (id, scenario, session holds a conversation id, expected quota_exhausted, message fragment)
TURN_REFUSALS = [
    ("pool-spent-before-spawn", _all_logins_rejected, True, True, "no available account"),
    ("every-login-failed", _every_login_failed, True, True, "no available account"),
    ("pool-spent-at-relay", _pool_spent_at_the_relay, True, True, "no available account"),
    ("relay-without-session-id", _pool_spent_at_the_relay, False, False, "relay impossible"),
    ("relay-move-failed", _relay_move_fails, True, False, "relay failed: disk full"),
    ("relay-limit", _relay_limit, True, False, "relay limit"),
]


@pytest.mark.parametrize(
    ("scenario", "held_id", "quota_exhausted", "fragment"), [row[1:] for row in TURN_REFUSALS],
    ids=[row[0] for row in TURN_REFUSALS])
@pytest.mark.asyncio
async def test_turn_refusal_writes_an_error_event_carrying_quota_exhausted(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, scenario, held_id: bool, quota_exhausted: bool,
    fragment: str) -> None:
  cfg, backends = scenario(tmp_path, monkeypatch)
  conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  meta = models.SessionMetadata(
      profile="manager",
      id="s1",
      name="t",
      backend=conftest.POOLED_FABLE_ID,
      cc_session_id=CC_ID if held_id else None,
      claude_account="main",
      slack_origin=slack_metadata.SlackOrigin(team_id="T", channel_id="C", thread_ts="1.0"))
  conftest.install_scripted_backends(monkeypatch, backends, conftest.BUILD_BACKEND_PATCH_TARGET)
  item = conftest.make_work_item(cfg, meta, cfg.get_backend_option(conftest.POOLED_FABLE_ID))

  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 1
  assert fragment in error_msg
  errors = [
      call.args[1]
      for call in item.callbacks.persist_and_broadcast.await_args_list
      if call.args[1].get("type") == ET.ASSISTANT_ERROR
  ]
  assert [event[ET.QUOTA_EXHAUSTED] for event in errors] == [quota_exhausted]
  assert fragment in errors[0]["content"]


# ---------------------------------------------------------------------------
# quota_exhausted on the launch error event: task runs
# ---------------------------------------------------------------------------


# A task scenario returns the pool's labels and the backends that serve the run's processes.
def _task_pool_spent_before_spawn(monkeypatch: pytest.MonkeyPatch) -> tuple[tuple[str, ...], list, tuple[str, ...]]:
  labels = ("main", "ext-1", "ext-2")
  for label in labels:
    conftest.reject_claude_login(label)
  return labels, [], ()


def _task_every_login_failed(monkeypatch: pytest.MonkeyPatch) -> tuple[tuple[str, ...], list, tuple[str, ...]]:
  labels = ("main", "ext-1", "ext-2")
  return labels, [], labels


def _task_pool_spent_at_relay(monkeypatch: pytest.MonkeyPatch) -> tuple[tuple[str, ...], list, tuple[str, ...]]:
  conftest.reject_claude_login("ext-1")
  return ("main", "ext-1"), [_rejected_run()], ()


def _task_relay_move_fails(monkeypatch: pytest.MonkeyPatch) -> tuple[tuple[str, ...], list, tuple[str, ...]]:
  _fail_every_move(monkeypatch)
  return ("main", "ext-1"), [_rejected_run()], ()


def _task_relay_limit(monkeypatch: pytest.MonkeyPatch) -> tuple[tuple[str, ...], list, tuple[str, ...]]:
  return ("main", "a", "b", "c"), [_rejected_run() for _ in range(claude_relay.MAX_RELAYS_PER_TURN + 1)], ()


# (id, scenario, expected quota_exhausted, message fragment)
TASK_REFUSALS = [
    ("pool-spent-before-spawn", _task_pool_spent_before_spawn, True, "no available account"),
    ("every-login-failed", _task_every_login_failed, True, "no available account"),
    ("pool-spent-at-relay", _task_pool_spent_at_relay, True, "no available account"),
    ("relay-move-failed", _task_relay_move_fails, False, "relay failed: disk full"),
    ("relay-limit", _task_relay_limit, False, "relay limit"),
]


@pytest.mark.parametrize(
    ("scenario", "quota_exhausted", "fragment"), [row[1:] for row in TASK_REFUSALS],
    ids=[row[0] for row in TASK_REFUSALS])
@pytest.mark.asyncio
async def test_task_refusal_writes_an_error_event_carrying_quota_exhausted(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, scenario, quota_exhausted: bool, fragment: str) -> None:
  from tests import test_task_execution as tte

  labels, backends, failed_login_labels = scenario(monkeypatch)
  cfg, session_blocks, tree = tte.build_pooled_env(tmp_path, monkeypatch, labels=labels)
  for label in failed_login_labels:
    conftest.write_pool_credentials(tmp_path / f"claude-{label}", access_token="")
  task_session = await conftest.create_task(
      tree,
      parent=None,
      request_id="w",
      profile="worker",
      task=tte.models.TaskSpec(goal="ship it", task_type="quick-edit"))
  tree.dispatch.executor = tte._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  # The fresh launch binds a new Claude session id; pin it so the transcript the relay moves exists.
  monkeypatch.setattr(tte.task_execution_module.uuid, "uuid4", lambda: uuid.UUID(CC_ID))
  conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  builds = conftest.install_scripted_backends(monkeypatch, backends, conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  await tte._register_work_run(tree, task_session.id, "run-x", conftest.POOLED_FABLE_ID, conftest.FABLE_MODEL)
  tree.dispatch.executor.launch(task_session.id, "run-x")
  _run, outcome = await tte.wait_for_terminal_run(tree, task_session.id, "run-x", timeout=20)

  assert outcome == "failed"
  assert len(builds) == len(backends)
  events_log = tree.runs.run_dir(task_session.id, "run-x") / "events.jsonl"
  logged = [json.loads(line) for line in events_log.read_text(encoding="utf-8").splitlines() if line.strip()]
  refusals = [event for event in logged if event["type"] == ET.ERROR and ET.QUOTA_EXHAUSTED in event]
  assert [event[ET.QUOTA_EXHAUSTED] for event in refusals] == [quota_exhausted]
  assert fragment in refusals[0]["message"]
