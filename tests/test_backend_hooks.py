"""Backend type table and lifecycle hooks: what the runtime reads from a registered backend package.

Covers the registration contract (src/runtime/hooks/backend_types.py), the import isolation of the
runtime (a registration costs no backend import), the Claude CLI lifecycles of the variants and of
the non-pooled cc-claude entry (src/backends/claude_code/claude_lifecycle.py), and the
``quota_exhausted`` flag that a refused launch copies onto its error event, for turns and tasks.
"""

import json
import subprocess
import sys
import uuid
from pathlib import Path
from types import SimpleNamespace

import conftest
import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    FABLE_MODEL,
    POOLED_FABLE_ID,
    WORKER_BUILD_BACKEND_PATCH_TARGET,
    ScriptedRelayBackend,
    assistant_text_event,
    backend_option,
    fable_pool_cfg,
    fresh_state_fixture,
    install_scripted_backends,
    make_transcript,
    make_work_item,
    rate_limit_event,
    write_pool_credentials,
)

from src.backends.claude_code import claude_accounts, claude_lifecycle, claude_relay
from src.infra import event_types as ET
from src.infra import identity_env
from src.infra.config import CharlieBotConfig
from src.infra.models import SessionMetadata, ThreadMetadata
from src.runtime import master_cc_run
from src.runtime.hooks import backend_lifecycle, backend_type_registration, backend_types
from src.runtime.worker import Worker

CC_ID = "11111111-2222-3333-4444-555555555555"
CLI_VARIANT_TYPES = ["cc-kimi", "cc-openai-compatible"]

_fresh_pool_state = fresh_state_fixture(claude_accounts.reset_for_tests)


def _variant_option(backend_type: str):
  extra = {"credential": "test-kimi"} if backend_type == "cc-kimi" else {"api_base": "http://test.invalid"}
  return backend_option(id="variant", label="Variant", type=backend_type, model="claude-sonnet-4-5", **extra)


def _turn_context(
    cfg: CharlieBotConfig, option, held_native_id: str | None, tmp_path: Path) -> backend_lifecycle.LaunchContext:
  meta = SessionMetadata(profile="manager", id="s1", name="t", backend=option.id, cc_session_id=held_native_id)
  return master_cc_run._turn_launch_context(make_work_item(cfg, meta, option), option, str(tmp_path), held_native_id)


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
  option = SimpleNamespace(type="no-such-backend")

  with pytest.raises(ValueError, match="no-such-backend"):
    backend_types.build_backend(option, CharlieBotConfig())
  with pytest.raises(ValueError, match="no-such-backend"):
    backend_types.traits_for("no-such-backend")


def test_a_type_without_a_lifecycle_gets_the_default() -> None:
  option = backend_option(id="g", label="G", type="gemini", model="m")

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

  assert result.stdout.strip() == f"{len(backend_types.registered_types())} [] []"


def test_the_packages_register_the_credential_variables_an_isolated_trial_must_not_inherit() -> None:
  assert identity_env.inherited_identity_env_vars() == (
      "CHARLIEBOT_SESSION_ID",
      "CHARLIEBOT_RUN_TOKEN",
      "CHARLIE_CODE_API_KEY",
      "CLAUDE_CODE_OAUTH_TOKEN",
      "ANTHROPIC_API_KEY",
  )
  with pytest.raises(ValueError, match="already registered"):
    identity_env.register_identity_env_var("ANTHROPIC_API_KEY")


# ---------------------------------------------------------------------------
# The Claude CLI variants and the non-pooled cc-claude entry
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("backend_type", CLI_VARIANT_TYPES)
@pytest.mark.asyncio
async def test_cli_variant_turn_checks_the_transcript_and_adds_the_dynamic_sections_flag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, backend_type: str) -> None:
  config_dir = tmp_path / "claude-default"
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(config_dir))
  option = _variant_option(backend_type)
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends={"options": [option]})
  lifecycle = backend_types.lifecycle_for(option)

  missing = await lifecycle.place(_turn_context(cfg, option, CC_ID, tmp_path))
  make_transcript(config_dir, CC_ID)
  present = await lifecycle.place(_turn_context(cfg, option, CC_ID, tmp_path))
  fresh = await lifecycle.place(_turn_context(cfg, option, None, tmp_path))

  assert missing.resume_id is None
  assert present.resume_id == CC_ID
  assert fresh.resume_id is None
  for launch in (missing, present, fresh):
    assert launch.backend_kwargs == {"extra_flags": [claude_lifecycle.EXCLUDE_DYNAMIC_SECTIONS_FLAG]}


@pytest.mark.parametrize("backend_type", CLI_VARIANT_TYPES)
def test_cli_variant_round_reports_a_reply_from_outside_the_pinned_family(backend_type: str) -> None:
  option = _variant_option(backend_type)
  lifecycle = backend_types.lifecycle_for(option)
  outside = assistant_text_event("hi")
  outside["message"]["model"] = "claude-haiku-4-5"
  inside = assistant_text_event("hi")
  inside["message"]["model"] = "claude-sonnet-4-5"

  notices = lifecycle.round_notices(option, [outside])

  assert [(n["type"], n["backend"], n["served_models"]) for n in notices] == [
      (ET.MODEL_FALLBACK_NOTICE, option.id, ["claude-haiku-4-5"])
  ]
  assert lifecycle.round_notices(option, [inside]) == []


@pytest.mark.asyncio
async def test_non_pooled_cc_claude_turn_checks_the_transcript_and_selects_no_account(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  config_dir = tmp_path / "claude-default"
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(config_dir))

  def _no_selection(*args: object, **kwargs: object) -> None:
    raise AssertionError("a non-pooled turn must not select an account")

  monkeypatch.setattr(claude_accounts, "select", _no_selection)
  option = backend_option(id="cc", label="CC", type="cc-claude", model="claude-opus-4-8")
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends={"options": [option]})
  lifecycle = backend_types.lifecycle_for(option)

  missing = await lifecycle.place(_turn_context(cfg, option, CC_ID, tmp_path))
  make_transcript(config_dir, CC_ID)
  present = await lifecycle.place(_turn_context(cfg, option, CC_ID, tmp_path))

  assert (missing.resume_id, present.resume_id) == (None, CC_ID)
  for launch in (missing, present):
    assert launch.account_label is None
    assert launch.backend_kwargs == {"extra_flags": [claude_lifecycle.EXCLUDE_DYNAMIC_SECTIONS_FLAG]}
  assert lifecycle.watch(_turn_context(cfg, option, CC_ID, tmp_path), present) is None


@pytest.mark.parametrize("backend_type", ["codex", "cc-kimi"])
@pytest.mark.asyncio
async def test_non_claude_task_build_receives_no_claude_account(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, backend_type: str) -> None:
  option = _variant_option(backend_type) if backend_type == "cc-kimi" else backend_option(
      id="variant", label="Variant", type="codex", model="m")
  backend = ScriptedRelayBackend([assistant_text_event("done")], exit_code=0)
  builds = install_scripted_backends(monkeypatch, [backend], WORKER_BUILD_BACKEND_PATCH_TARGET)
  worker = Worker(
      ThreadMetadata(id="t1", session_id="s1", description="task"),
      tmp_path / "work",
      tmp_path / "data" / "events.jsonl",
      "do the thing",
      CharlieBotConfig(charliebot_home=tmp_path / "home"),
      backend_option=option,
      session_meta=SessionMetadata(profile="manager", id="s1", name="S", backend=option.id),
  )

  assert await worker.run() == 0

  assert "claude_account" not in builds[0]["kwargs"]


# ---------------------------------------------------------------------------
# quota_exhausted on the launch error event: master turns
# ---------------------------------------------------------------------------


def _reject(label: str) -> None:
  claude_accounts.observe_rate_limit(label, rate_limit_event("rejected", 1.0)["rate_limit_info"])


def _rejected_run() -> ScriptedRelayBackend:
  return ScriptedRelayBackend([rate_limit_event("rejected", 1.0)], exit_code=1)


# A turn scenario returns the pooled config and the backends that serve the turn's processes.
def _all_logins_rejected(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[CharlieBotConfig, list]:
  cfg = fable_pool_cfg(tmp_path)
  for label in ("main", "ext-1", "ext-2"):
    _reject(label)
  return cfg, []


def _every_login_failed(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[CharlieBotConfig, list]:
  cfg = fable_pool_cfg(tmp_path)
  for label in ("main", "ext-1", "ext-2"):
    write_pool_credentials(tmp_path / f"claude-{label}", access_token="")
  return cfg, []


def _pool_spent_at_the_relay(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[CharlieBotConfig, list]:
  cfg = fable_pool_cfg(tmp_path)
  for label in ("ext-1", "ext-2"):
    write_pool_credentials(tmp_path / f"claude-{label}", access_token="")
  return cfg, [_rejected_run()]


def _fail_every_move(monkeypatch: pytest.MonkeyPatch) -> None:

  def _fail(*args: object, **kwargs: object) -> None:
    raise claude_accounts.TranscriptMoveError("disk full")

  monkeypatch.setattr(claude_accounts, "move_transcript", _fail)


def _relay_move_fails(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[CharlieBotConfig, list]:
  _fail_every_move(monkeypatch)
  return fable_pool_cfg(tmp_path), [_rejected_run()]


def _relay_limit(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[CharlieBotConfig, list]:
  cfg = fable_pool_cfg(tmp_path, labels=("main", "a", "b", "c"))
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
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, scenario, held_id: bool, quota_exhausted: bool,
    fragment: str) -> None:
  cfg, backends = scenario(tmp_path, monkeypatch)
  make_transcript(tmp_path / "claude-main", CC_ID)
  meta = SessionMetadata(
      profile="manager",
      id="s1",
      name="t",
      backend=POOLED_FABLE_ID,
      cc_session_id=CC_ID if held_id else None,
      claude_account="main")
  install_scripted_backends(monkeypatch, backends, BUILD_BACKEND_PATCH_TARGET)
  item = make_work_item(cfg, meta, cfg.get_backend_option(POOLED_FABLE_ID))

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
    _reject(label)
  return labels, [], ()


def _task_every_login_failed(monkeypatch: pytest.MonkeyPatch) -> tuple[tuple[str, ...], list, tuple[str, ...]]:
  labels = ("main", "ext-1", "ext-2")
  return labels, [], labels


def _task_pool_spent_at_relay(monkeypatch: pytest.MonkeyPatch) -> tuple[tuple[str, ...], list, tuple[str, ...]]:
  _reject("ext-1")
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
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, scenario, quota_exhausted: bool, fragment: str) -> None:
  from tests import test_task_execution as tte

  labels, backends, failed_login_labels = scenario(monkeypatch)
  cfg, session_mgr, tree = tte.build_pooled_env(tmp_path, monkeypatch, labels=labels)
  for label in failed_login_labels:
    write_pool_credentials(tmp_path / f"claude-{label}", access_token="")
  worker = await tte.create_task(
      tree, parent=None, request_id="w", profile="worker", task=tte.TaskSpec(goal="ship it", task_type="quick-edit"))
  tree.dispatch.executor = tte._adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  # The fresh launch binds a new Claude session id; pin it so the transcript the relay moves exists.
  monkeypatch.setattr(tte.task_execution_module.uuid, "uuid4", lambda: uuid.UUID(CC_ID))
  make_transcript(tmp_path / "claude-main", CC_ID)
  builds = install_scripted_backends(monkeypatch, backends, WORKER_BUILD_BACKEND_PATCH_TARGET)

  await tte._register_work_run(tree, worker.id, "run-x", POOLED_FABLE_ID, FABLE_MODEL)
  tree.dispatch.executor.launch(worker.id, "run-x")
  _run, outcome = await tte.wait_for_terminal_run(tree, worker.id, "run-x", timeout=20)

  assert outcome == "failed"
  assert len(builds) == len(backends)
  events_log = tree.runs.run_dir(worker.id, "run-x") / "events.jsonl"
  logged = [json.loads(line) for line in events_log.read_text(encoding="utf-8").splitlines() if line.strip()]
  refusals = [event for event in logged if event["type"] == ET.ERROR and ET.QUOTA_EXHAUSTED in event]
  assert [event[ET.QUOTA_EXHAUSTED] for event in refusals] == [quota_exhausted]
  assert fragment in refusals[0]["message"]
