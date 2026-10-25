"""Tests for src/features/artifacts/artifact_check.py, the ``charliebot artifact check`` CLI,
and the cold-read task type end to end (delegate entry, run, report).

The goal-length and page-height measurements and their budgets live here; the DOM assertions are the
mechanical half of each genre's GRAMMAR.
Renderer work and the probe's model call are doubled out — no headless chrome, no backend
subprocess, no HTTP; the cold-read runs execute on scripted backends.
"""

import hashlib
import json
import pathlib
import re
import sys
import types
from unittest import mock

import conftest
import fastapi
import pytest

from src.features.artifacts import artifact_check, artifact_shared, cli
from src.infra import config, models
from src.infra import event_types as ET
from src.runtime import cold_read, spawner_backends, task_errors
from src.runtime.api import internal
from tests import test_task_execution


def _genre_doc(genre: str, body: str) -> str:
  """Full HTML document for *genre*: its template's <style> block verbatim plus *body*."""
  template = (conftest.ROOT / "prompts" / artifact_shared.GENRE_TEMPLATES[genre]).read_text(encoding="utf-8")
  style = re.search(r"<style>.*?</style>", template, re.DOTALL).group(0)
  return f"<html><head>{style}</head><body>{body}</body></html>"


def _write(tmp_path: pathlib.Path, content: str, name: str = "page.html") -> pathlib.Path:
  artifact = tmp_path / name
  artifact.write_text(content, encoding="utf-8")
  return artifact


def _by_name(outcomes: list[artifact_check.AssertionOutcome]) -> dict[str, list[artifact_check.AssertionOutcome]]:
  by_name: dict[str, list[artifact_check.AssertionOutcome]] = {}
  for outcome in outcomes:
    by_name.setdefault(outcome.name, []).append(outcome)
  return by_name


def _run(
    genre: str,
    artifact: pathlib.Path,
    cfg: config.CharlieBotConfig | None = None,
) -> dict[str, list[artifact_check.AssertionOutcome]]:
  return _by_name(artifact_check.run_assertions(genre, artifact, cfg))


def _chrome_cfg(tmp_path: pathlib.Path) -> types.SimpleNamespace:
  return types.SimpleNamespace(headless_chrome_bin=conftest.write_stub_chrome(tmp_path, 800))


def _sitrep_ok_doc() -> str:
  return _genre_doc(
      "sitrep", '<section><h2><span class="n">1</span> What waits on you?</h2><p>Nothing.</p></section>'
      '<section><h2><span class="n">2</span> What is this and why?</h2>'
      '<p>Why. <span class="req">r1</span> states the first requirement.</p></section>'
      '<section><h2><span class="n">3</span> What was verified?</h2><p>Done. <span class="src">s</span></p></section>'
      '<section><h2><span class="n">4</span> Risks</h2>'
      '<p><span class="tag fact">Fact</span> The reading holds. <span class="src">s</span></p></section>'
      '<section><h2><span class="n">5</span> What happens next?</h2><p>Next.</p></section>')


# ---------------------------------------------------------------------------
# style-verbatim
# ---------------------------------------------------------------------------


def test_style_verbatim_fails_on_tampered_style(tmp_path: pathlib.Path) -> None:
  doc = _sitrep_ok_doc().replace("</style>", "body{color:red}\n</style>", 1)
  outcomes = _run("sitrep", _write(tmp_path, doc))
  (outcome,) = outcomes["style-verbatim"]
  assert not outcome.passed
  assert "differs from the genre template prompts/sitrep_template.html" in outcome.detail


# ---------------------------------------------------------------------------
# Shipped templates pass their own genre
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("genre", "template"),
    [
        ("plan", "plan_template.html"),
        ("sitrep", "sitrep_template.html"),
        ("debug", "debug_template.html"),
        ("explain", "explain_template.html"),
    ],
)
def test_shipped_template_passes_its_own_genre(tmp_path: pathlib.Path, genre: str, template: str) -> None:
  cfg = _chrome_cfg(tmp_path)
  outcomes = artifact_check.run_assertions(genre, conftest.ROOT / "prompts" / template, cfg)
  assert [o for o in outcomes if not o.passed] == []


# ---------------------------------------------------------------------------
# req-chips: a requirement chip needs its gloss in the sentence that holds it
# ---------------------------------------------------------------------------


def test_req_chips_passes_when_the_chip_is_glossed_in_its_sentence(tmp_path: pathlib.Path) -> None:
  (outcome,) = _run("sitrep", _write(tmp_path, _sitrep_ok_doc()))["req-chips"]
  assert outcome.passed


def test_req_chips_fails_when_the_sentence_ends_after_the_chip(tmp_path: pathlib.Path) -> None:
  doc = _sitrep_ok_doc().replace(
      '<span class="req">r1</span> states the first requirement.',
      '<span class="req">r1</span>. A later sentence names the requirement.')
  (outcome,) = _run("sitrep", _write(tmp_path, doc))["req-chips"]
  assert not outcome.passed
  assert "'r1'" in outcome.detail
  assert "no gloss" in outcome.detail


# ---------------------------------------------------------------------------
# run-stated: a numeric table needs a block naming the run's changed and held-fixed factors
# ---------------------------------------------------------------------------

_NUMERIC_TABLE = (
    '<table><thead><tr><th>checkpoint</th><th>score</th></tr></thead>'
    '<tbody><tr><td>10</td><td>0.002378</td></tr></tbody></table>')


def _sitrep_run_doc(variable_block: str = "") -> str:
  return _sitrep_ok_doc().replace(
      '<p>Done. <span class="src">s</span></p>',
      _NUMERIC_TABLE + variable_block + '<p>Done. <span class="src">s</span></p>')


@pytest.mark.parametrize(
    "variable_block",
    [
        (
            '<table><tr><th>role</th><th>value</th></tr>'
            '<tr><td>Changed</td><td>weights randomly initialized; 3,506 columns dropped</td></tr>'
            '<tr><td>Held fixed</td><td>commit e0852cbb; seed 3</td></tr></table>'),
        ('<dl><dt>改了什么</dt><dd>权重随机初始化，删掉 3,506 列</dd>'
         '<dt>保持不变</dt><dd>提交 e0852cbb，种子 3</dd></dl>'),
    ],
)
def test_run_stated_passes_when_a_table_or_dl_states_both_roles(tmp_path: pathlib.Path, variable_block: str) -> None:
  (outcome,) = _run("sitrep", _write(tmp_path, _sitrep_run_doc(variable_block)))["run-stated"]
  assert outcome.passed


def test_run_stated_fails_when_the_numeric_table_has_no_variable_block(tmp_path: pathlib.Path) -> None:
  (outcome,) = _run("sitrep", _write(tmp_path, _sitrep_run_doc()))["run-stated"]
  assert not outcome.passed
  assert "changed-factor row" in outcome.detail
  assert "held-fixed-factor row" in outcome.detail


def test_run_stated_names_the_one_missing_role_row(tmp_path: pathlib.Path) -> None:
  block = '<table><tr><td>Changed</td><td>3,506 columns dropped</td></tr></table>'
  (outcome,) = _run("sitrep", _write(tmp_path, _sitrep_run_doc(block)))["run-stated"]
  assert not outcome.passed
  assert "held-fixed-factor row" in outcome.detail
  assert "changed-factor row" not in outcome.detail


def test_run_stated_is_inert_without_a_numeric_table_cell(tmp_path: pathlib.Path) -> None:
  (outcome,) = _run("sitrep", _write(tmp_path, _sitrep_ok_doc()))["run-stated"]
  assert outcome.passed
  assert outcome.detail == "no numeric table cell"


# ---------------------------------------------------------------------------
# page-height: a measurement report against the 2000 px target, passing at any height
# ---------------------------------------------------------------------------


def test_page_height_over_the_target_passes_and_names_the_overage(tmp_path: pathlib.Path) -> None:
  cfg = types.SimpleNamespace(headless_chrome_bin=conftest.write_stub_chrome(tmp_path, 2600))
  (outcome,) = _run("plan", _write(tmp_path, conftest.plan_page_html()), cfg)["page-height"]
  assert outcome.passed
  assert outcome.detail == "2600 px: 600 px over the 2000 px target"


def test_page_height_at_or_under_the_target_reports_the_target(tmp_path: pathlib.Path) -> None:
  cfg = types.SimpleNamespace(headless_chrome_bin=conftest.write_stub_chrome(tmp_path, 1967))
  (outcome,) = _run("plan", _write(tmp_path, conftest.plan_page_html()), cfg)["page-height"]
  assert outcome.passed
  assert outcome.detail == "1967 px (target 2000)"


def test_page_height_without_a_renderer_fails(tmp_path: pathlib.Path) -> None:
  cfg = types.SimpleNamespace(headless_chrome_bin=None)
  (outcome,) = _run("plan", _write(tmp_path, conftest.plan_page_html()), cfg)["page-height"]
  assert not outcome.passed


# ---------------------------------------------------------------------------
# CLI: charliebot artifact check
# ---------------------------------------------------------------------------


def _invoke(argv_tail: list[str]) -> SystemExit:
  with mock.patch("sys.argv", ["artifact", "check", *argv_tail]), pytest.raises(SystemExit) as exc_info:
    cli.main()
  return exc_info.value


def _run_cli(argv_tail: list[str]) -> int:
  exit_ = _invoke(argv_tail)
  assert isinstance(exit_.code, int)
  return exit_.code


def _probe_cfg(tmp_path: pathlib.Path) -> tuple[types.SimpleNamespace, dict[str, types.SimpleNamespace]]:
  options = {name: types.SimpleNamespace(id=name) for name in ("alpha", "beta")}
  cfg = types.SimpleNamespace(
      headless_chrome_bin=conftest.write_stub_chrome(tmp_path, 800),
      backends=types.SimpleNamespace(pool=["alpha", "beta"]),
      get_backend_option=options.get,
      sessions_dir=tmp_path,
      charlie_bot_repo=conftest.ROOT,
  )
  return cfg, options


class _FakeBackend:

  def __init__(self, answer: str | None = None, error: str | BaseException | None = None) -> None:
    self._answer = answer
    self._error = RuntimeError(error) if isinstance(error, str) else error
    self.calls: list[dict] = []

  async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: float | None) -> str:
    self.calls.append({"prompt": prompt, "system_prompt": system_prompt, "timeout": timeout})
    if self._error is not None:
      raise self._error
    assert self._answer is not None
    return self._answer


def _patch_backends(monkeypatch: pytest.MonkeyPatch, backends: dict[str, _FakeBackend]) -> None:
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, lambda option, cfg: backends[option.id])


def test_cli_probe_runs_after_assertions_pass_and_prints_backend_and_answers(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  backends = {
      "alpha": _FakeBackend(error="boom"),
      "beta": _FakeBackend(answer="(1) The reader's problem.\n(2)-(6) fine.")
  }
  _patch_backends(monkeypatch, backends)
  monkeypatch.setattr(conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?"]) == 0
  lines = capsys.readouterr().out.splitlines()
  digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
  assert lines[-6:] == [
      f"cold read of {artifact}",
      f"page sha256 {digest}",
      "attempt alpha failed: RuntimeError: boom",
      "backend beta",
      "(1) The reader's problem.",
      "(2)-(6) fine.",
  ]
  # The page text and the substituted trigger ride in one prompt; the placeholder is gone.
  prompt = backends["beta"].calls[0]["prompt"]
  assert artifact.read_text(encoding="utf-8") in prompt
  assert '"where are we?"' in prompt
  assert "(7) Read as an engineer who knows the domain" in prompt
  assert "<trigger message verbatim>" not in prompt
  assert backends["beta"].calls[0]["timeout"] is None


def test_probe_records_each_failed_attempt_with_its_exception_type(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An exception with an empty message still names its type in the attempt record."""
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  backends = {"alpha": _FakeBackend(error=TimeoutError()), "beta": _FakeBackend(error="kaput")}
  _patch_backends(monkeypatch, backends)
  result = artifact_check.run_probe(cfg, artifact, "where are we?")
  assert result.attempts == [("alpha", "TimeoutError: "), ("beta", "RuntimeError: kaput")]
  assert result.backend_id is None


def test_cli_assertions_only_passes_a_page_over_the_height_target(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, conftest.plan_page_html())
  cfg = types.SimpleNamespace(headless_chrome_bin=conftest.write_stub_chrome(tmp_path, 2600))
  monkeypatch.setattr(conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  assert _run_cli([str(artifact), "--genre", "plan", "--assertions-only"]) == 0
  assert "ok page-height 2600 px: 600 px over the 2000 px target" in capsys.readouterr().out


def test_cli_assertions_only_fails_when_the_height_cannot_be_measured(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, conftest.plan_page_html())
  monkeypatch.setattr(
      conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: types.SimpleNamespace(headless_chrome_bin=None))
  assert _run_cli([str(artifact), "--genre", "plan", "--assertions-only"]) == 1
  assert "FAIL page-height" in capsys.readouterr().out


def test_cli_unknown_genre_is_usage_error() -> None:
  assert _run_cli(["page.html", "--genre", "weird"]) == 2


# ---------------------------------------------------------------------------
# CLI: the cold read follows the session (manager delegates, worker foreground)
# ---------------------------------------------------------------------------


def test_background_flag_is_a_usage_error(tmp_path: pathlib.Path) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "t", "--background"]) == 2


def _write_session_metadata(cfg: types.SimpleNamespace, session_id: str, profile: str) -> None:
  """Persist a session metadata.json holding just the profile the CLI's routing reads."""
  meta_dir = cfg.sessions_dir / session_id
  meta_dir.mkdir(parents=True)
  (meta_dir / "metadata.json").write_text(json.dumps({"id": session_id, "name": "S", "profile": profile}))


def test_cli_in_a_manager_session_delegates_the_cold_read(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """The manager-session command POSTs the cold-read delegation and prints the started line."""
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  session_id = "sess-manager-1"
  _write_session_metadata(cfg, session_id, "manager")
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", session_id)
  monkeypatch.setattr(conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  calls: list[dict] = []

  def post(endpoint: str, payload: dict, *, readback: object = None, rejection_exit_codes: object = None) -> dict:
    calls.append({"endpoint": endpoint, "payload": payload})
    return {
        "session_id": "child-1",
        "parent_session_id": session_id,
        "run_id": "run-1",
        "thread_id": "run-1",
        "description": payload["description"],
    }

  monkeypatch.setattr(conftest.CLI_COMMON_POST_INTERNAL_API_PATCH_TARGET, post)
  assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?"]) == 0
  (call,) = calls
  assert call["endpoint"] == "/api/internal/delegate"
  payload = call["payload"]
  digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
  assert payload["session_id"] == session_id
  assert payload["task_type"] == "cold-read"
  assert payload["cold_read"] == {"page_path": str(artifact), "genre": "sitrep", "trigger": "where are we?"}
  assert "repo_path" not in payload and "base_branch" not in payload and "backend" not in payload
  assert payload["request_id"].startswith(f"cold-read:{session_id}:{digest[:12]}:")
  assert "cold read started: session child-1 run run-1" in capsys.readouterr().out


def test_cli_in_a_manager_session_exits_1_when_the_server_refuses(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  _write_session_metadata(cfg, "sess-manager-2", "manager")
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "sess-manager-2")
  monkeypatch.setattr(conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)

  def refusing_post(
      endpoint: str, payload: dict, *, readback: object = None, rejection_exit_codes: object = None) -> dict:
    print(json.dumps({"error": "delegation blocked", "code": "server_error", "effect": "none"}), file=sys.stderr)
    raise SystemExit(1)

  monkeypatch.setattr(conftest.CLI_COMMON_POST_INTERNAL_API_PATCH_TARGET, refusing_post)
  assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?"]) == 1
  assert "delegation blocked" in capsys.readouterr().err


def test_cli_in_a_worker_session_runs_the_cold_read_in_the_foreground(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  _write_session_metadata(cfg, "sess-worker-1", "worker")
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "sess-worker-1")
  monkeypatch.setattr(conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  backends = {"alpha": _FakeBackend(answer="(1) answered.")}
  _patch_backends(monkeypatch, backends)
  post = mock.MagicMock()
  monkeypatch.setattr(conftest.CLI_COMMON_POST_INTERNAL_API_PATCH_TARGET, post)

  assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?"]) == 0
  digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
  assert capsys.readouterr().out.splitlines()[-4:] == [
      f"cold read of {artifact}",
      f"page sha256 {digest}",
      "backend alpha",
      "(1) answered.",
  ]
  post.assert_not_called()


def test_cli_with_no_session_identity_runs_the_cold_read_in_the_foreground(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """No identity: CHARLIEBOT_SESSION_ID absent and the cwd not a session directory."""
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  monkeypatch.delenv("CHARLIEBOT_SESSION_ID", raising=False)
  monkeypatch.setattr(conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  backends = {"alpha": _FakeBackend(answer="(1) answered.")}
  _patch_backends(monkeypatch, backends)
  post = mock.MagicMock()
  monkeypatch.setattr(conftest.CLI_COMMON_POST_INTERNAL_API_PATCH_TARGET, post)

  assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?"]) == 0
  digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
  assert capsys.readouterr().out.splitlines()[-4:] == [
      f"cold read of {artifact}",
      f"page sha256 {digest}",
      "backend alpha",
      "(1) answered.",
  ]
  post.assert_not_called()


def test_cli_foreground_prints_every_model_failed_and_exits_1(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = _write(tmp_path, _sitrep_ok_doc())
  cfg, _options = _probe_cfg(tmp_path)
  monkeypatch.delenv("CHARLIEBOT_SESSION_ID", raising=False)
  monkeypatch.setattr(conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  backends = {"alpha": _FakeBackend(error="boom"), "beta": _FakeBackend(error="kaput")}
  _patch_backends(monkeypatch, backends)

  assert _run_cli([str(artifact), "--genre", "sitrep", "--trigger", "where are we?"]) == 1
  digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
  assert capsys.readouterr().out.splitlines()[-5:] == [
      f"cold read of {artifact}",
      f"page sha256 {digest}",
      "attempt alpha failed: RuntimeError: boom",
      "attempt beta failed: RuntimeError: kaput",
      "every model failed (2 tried)",
  ]


# ---------------------------------------------------------------------------
# The cold-read task type at the real delegate entry
# ---------------------------------------------------------------------------


async def _cold_read_tree_env(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, *, execute: bool):
  """(cfg, session_blocks, tree, manager): a real task tree and delegate entry.

  The manager is pinned to the "opus" backend (options[0]); backends.pool
  is ["alpha", "beta"]. With execute=False the launch seam is stubbed so no run
  starts; with execute=True runs really execute on the scripted backends.
  """
  options = [conftest.backend_option(id="opus", label="opus", type="codex", model="opus-model")]
  options += [conftest.backend_option(id=b, label=b, type="codex", model=f"{b}-model") for b in ("alpha", "beta")]
  cfg, session_blocks, tree = test_task_execution.build_spawning_env(
      tmp_path, monkeypatch, options=options, pool=["alpha", "beta"])
  adapter = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  if not execute:
    monkeypatch.setattr(adapter, "_schedule_launch", lambda *args, **kwargs: None)
  tree.dispatch.executor = adapter
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  return cfg, session_blocks, tree, manager


def _cold_read_request(manager_id: str, page: pathlib.Path, **overrides) -> models.DelegateRequest:
  payload: dict = {
      "session_id": manager_id,
      "description": f"cold read of {page}",
      "task_type": models.TaskType.COLD_READ,
      "cold_read": models.ColdReadSpec(page_path=str(page), genre="sitrep", trigger="where are we?"),
  }
  payload.update(overrides)
  return models.DelegateRequest(**payload)


@pytest.mark.asyncio
async def test_cold_read_delegation_from_a_manager_succeeds_without_takeoff(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """No take off in the manager's chat log: the read-only exemption covers cold-read, and the
  child lands under the requesting session named after the page file."""
  page = _write(tmp_path, _sitrep_ok_doc())
  _cfg, session_blocks, tree, manager = await _cold_read_tree_env(tmp_path, monkeypatch, execute=False)

  result = await internal.delegate_task(
      _cold_read_request(manager.id, page),
      events=session_blocks.events,
      store=session_blocks.store,
      task_mgr=tree,
      caller=conftest.OPERATOR)

  child = await tree.load_meta(result["session_id"])
  assert child is not None
  assert child.task_parent_id == manager.id and child.profile == "worker"
  assert child.name == f"Cold read: {page.name}"
  assert child.task is not None and child.task.task_type == models.TaskType.COLD_READ
  assert child.task.cold_read == models.ColdReadSpec(page_path=str(page), genre="sitrep", trigger="where are we?")
  assert child.task.repo_path is None and child.task.base_branch is None
  first = await tree.runs.get_run(child.id, result["run_id"])
  assert first is not None and first.backend == "alpha"


@pytest.mark.asyncio
@pytest.mark.parametrize("field", ["repo_path", "base_branch"])
async def test_cold_read_delegation_with_a_repo_field_gets_400(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, field: str) -> None:
  page = _write(tmp_path, _sitrep_ok_doc())
  _cfg, session_blocks, tree, manager = await _cold_read_tree_env(tmp_path, monkeypatch, execute=False)
  overrides = {field: ("/tmp/repo" if field == "repo_path" else "main")}

  with pytest.raises(fastapi.HTTPException) as exc_info:
    await internal.delegate_task(
        _cold_read_request(manager.id, page, **overrides),
        events=session_blocks.events,
        store=session_blocks.store,
        task_mgr=tree,
        caller=conftest.OPERATOR)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == f"cold-read delegations are repo-less; omit {field}"


@pytest.mark.asyncio
async def test_cold_read_delegation_requires_cold_read(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  page = _write(tmp_path, _sitrep_ok_doc())
  _cfg, session_blocks, tree, manager = await _cold_read_tree_env(tmp_path, monkeypatch, execute=False)

  with pytest.raises(fastapi.HTTPException) as exc_info:
    await internal.delegate_task(
        _cold_read_request(manager.id, page, cold_read=None),
        events=session_blocks.events,
        store=session_blocks.store,
        task_mgr=tree,
        caller=conftest.OPERATOR)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "cold-read delegations require cold_read"


@pytest.mark.asyncio
async def test_other_task_types_reject_cold_read(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  page = _write(tmp_path, _sitrep_ok_doc())
  _cfg, session_blocks, tree, manager = await _cold_read_tree_env(tmp_path, monkeypatch, execute=False)

  with pytest.raises(fastapi.HTTPException) as exc_info:
    await internal.delegate_task(
        _cold_read_request(manager.id, page, task_type=models.TaskType.VERIFY),
        events=session_blocks.events,
        store=session_blocks.store,
        task_mgr=tree,
        caller=conftest.OPERATOR)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "verify delegations reject cold_read"


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", ["verify", "cold-read"])
async def test_agent_message_to_a_read_only_child_passes_without_takeoff(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, task_type: str) -> None:
  """An agent message to a cold-read child passes the same exemption a verify child passes."""
  _cfg, _session_blocks, tree, manager = await _cold_read_tree_env(tmp_path, monkeypatch, execute=False)
  cold = models.ColdReadSpec(page_path="/tmp/p.html", genre="plan", trigger="t") if task_type == "cold-read" else None
  child = await conftest.create_task(
      tree,
      parent=manager.id,
      profile="worker",
      request_id=f"child-{task_type}",
      task=models.TaskSpec(goal="check it", task_type=task_type, cold_read=cold))

  event = await tree.dispatch.admit_input(
      child.id, event_type=ET.AGENT_MESSAGE, content="a note from the parent", actor="agent", from_session=manager.id)
  assert event["content"] == "a note from the parent"

  # The control: an implement child refuses the same message without takeoff.
  implement = await conftest.create_task(
      tree,
      parent=manager.id,
      profile="worker",
      request_id=f"child-implement-{task_type}",
      task=models.TaskSpec(goal="build it", task_type="implement"))
  with pytest.raises(task_errors.TaskForbiddenError):
    await tree.dispatch.admit_input(
        implement.id, event_type=ET.AGENT_MESSAGE, content="x", actor="agent", from_session=manager.id)


# ---------------------------------------------------------------------------
# The cold-read run and its report
# ---------------------------------------------------------------------------


def _error_event(message: str) -> dict:
  """A backend error event for a scripted failure (the events-log error the attempt line reads)."""
  return {"type": "error", "message": message, "content": message}


async def _wait_child_report(tree, parent_id: str) -> dict:
  """Poll the parent's fact history until the cold-read child's report lands; return it."""
  await test_task_execution.poll_until(
      lambda: conftest.child_reports(tree, parent_id), timeout=15.0, what="the cold-read child report")
  return conftest.child_reports(tree, parent_id)[-1]


async def _delegate_cold_read(tree, session_blocks, manager, page: pathlib.Path) -> str:
  """Delegate one cold read through the real entry; return the child session id."""
  result = await internal.delegate_task(
      _cold_read_request(manager.id, page),
      events=session_blocks.events,
      store=session_blocks.store,
      task_mgr=tree,
      caller=conftest.OPERATOR)
  return result["session_id"]


@pytest.mark.asyncio
async def test_cold_read_run_instructions_hold_only_the_system_prompt_and_the_input_is_the_page_and_questions(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  page = _write(tmp_path, _sitrep_ok_doc())
  _cfg, session_blocks, tree, manager = await _cold_read_tree_env(tmp_path, monkeypatch, execute=True)
  backend = test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("(1) fine.")])
  test_task_execution.install_worker_launch_and_resume_backends(monkeypatch, [backend])

  child_id = await _delegate_cold_read(tree, session_blocks, manager, page)
  (run,) = [r for r in tree.runs.list_run_records_sync(child_id) if r.kind == "work"]
  await test_task_execution.wait_for_terminal_run(tree, child_id, run.id)
  await _wait_child_report(tree, manager.id)

  system_prompt, questions = cold_read.load_texts(conftest.ROOT)
  stored = json.loads((tree.runs.run_dir(child_id, run.id) / "prompt_snapshot.json").read_text(encoding="utf-8"))
  assert [b["text"] for b in stored["blocks"]] == [system_prompt]
  launch_text = (tree.runs.run_dir(child_id, run.id) / "launch_prompt.md").read_text(encoding="utf-8")
  assert launch_text == cold_read.probe_input(page.read_text(encoding="utf-8"), questions, "where are we?")
  assert backend.prompt == launch_text


@pytest.mark.asyncio
async def test_cold_read_success_report_carries_attempts_backend_and_answers(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  page = _write(tmp_path, _sitrep_ok_doc())
  _cfg, session_blocks, tree, manager = await _cold_read_tree_env(tmp_path, monkeypatch, execute=True)
  scripted = [
      test_task_execution.SpawningScriptedBackend([_error_event("alpha exploded")], exit_code=1),
      test_task_execution.SpawningScriptedBackend(
          [test_task_execution.result_event("(1) The reader's problem.\n(2) confirmed.")]),
  ]
  builds = test_task_execution.install_worker_launch_and_resume_backends(monkeypatch, scripted)

  child_id = await _delegate_cold_read(tree, session_blocks, manager, page)
  report = await _wait_child_report(tree, manager.id)

  digest = hashlib.sha256(page.read_bytes()).hexdigest()
  assert report["outcome"] == "completed"
  assert report["summary"] == "\n".join(
      [
          f"cold read of {page}",
          f"page sha256 {digest}",
          "attempt alpha failed: alpha exploded",
          "backend beta",
          "(1) The reader's problem.",
          "(2) confirmed.",
      ])
  assert tree.task_state(child_id) == "completed"
  # The retry took the next pool backend past the tried one.
  assert [b["option"].id for b in builds] == ["alpha", "beta"]


@pytest.mark.asyncio
async def test_cold_read_empty_answer_counts_as_a_failed_run(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  page = _write(tmp_path, _sitrep_ok_doc())
  _cfg, session_blocks, tree, manager = await _cold_read_tree_env(tmp_path, monkeypatch, execute=True)
  scripted = [
      test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("")]),
      test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("(1) answered.")]),
  ]
  test_task_execution.install_worker_launch_and_resume_backends(monkeypatch, scripted)

  await _delegate_cold_read(tree, session_blocks, manager, page)
  report = await _wait_child_report(tree, manager.id)

  assert report["outcome"] == "completed"
  assert "attempt alpha failed: empty final answer" in report["summary"].splitlines()
  assert "backend beta" in report["summary"].splitlines()


@pytest.mark.asyncio
async def test_cold_read_retry_selects_the_next_backend_with_the_tried_list(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  page = _write(tmp_path, _sitrep_ok_doc())
  _cfg, session_blocks, tree, manager = await _cold_read_tree_env(tmp_path, monkeypatch, execute=True)
  tried_lists: list[list[str]] = []
  real_select = spawner_backends.select_verify_backend

  async def spy(session_id: str, cfg, store, tried: list[str], *, caller: str):
    tried_lists.append(list(tried))
    return await real_select(session_id, cfg, store, tried, caller=caller)

  monkeypatch.setattr(spawner_backends, "select_verify_backend", spy)
  scripted = [
      test_task_execution.SpawningScriptedBackend([_error_event("alpha exploded")], exit_code=1),
      test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("(1) answered.")]),
  ]
  test_task_execution.install_worker_launch_and_resume_backends(monkeypatch, scripted)

  await _delegate_cold_read(tree, session_blocks, manager, page)
  await _wait_child_report(tree, manager.id)

  # The delegation resolves the first backend with an empty tried list; the
  # failed run's retry passes every backend tried so far.
  assert tried_lists == [[], ["alpha"]]


@pytest.mark.asyncio
async def test_cold_read_every_backend_failed_reports_and_the_child_is_archived(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  page = _write(tmp_path, _sitrep_ok_doc())
  _cfg, session_blocks, tree, manager = await _cold_read_tree_env(tmp_path, monkeypatch, execute=True)
  scripted = [
      test_task_execution.SpawningScriptedBackend([_error_event("alpha exploded")], exit_code=1),
      test_task_execution.SpawningScriptedBackend([_error_event("beta exploded")], exit_code=1),
      test_task_execution.SpawningScriptedBackend([_error_event("opus exploded")], exit_code=1),
  ]
  test_task_execution.install_worker_launch_and_resume_backends(monkeypatch, scripted)

  child_id = await _delegate_cold_read(tree, session_blocks, manager, page)
  report = await _wait_child_report(tree, manager.id)

  digest = hashlib.sha256(page.read_bytes()).hexdigest()
  assert report["outcome"] == "failed"
  assert report["summary"] == "\n".join(
      [
          f"cold read of {page}",
          f"page sha256 {digest}",
          "attempt alpha failed: alpha exploded",
          "attempt beta failed: beta exploded",
          "attempt opus failed: opus exploded",
          "every model failed (3 tried)",
      ])
  # The all-failed report's delivery archived the stopped child.
  await test_task_execution.poll_until(
      lambda: tree.task_state(child_id) == "archived", timeout=15.0, what="the failed cold-read child's archive")
  closes = [e for e in tree.events.load_events(child_id) if e["type"] == "task_closed"]
  assert len(closes) == 1
  assert closes[0]["outcome"] == "archived"
  assert closes[0]["actor"] == "system"


# ---------------------------------------------------------------------------
# Registration parity: plan present enforces exactly artifact check --genre plan
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_plan_present_and_artifact_check_reject_the_same_assertions_on_one_fixture(
    tmp_path: pathlib.Path) -> None:
  broken = conftest.plan_page_html().replace('<span class="n">4</span>', '<span class="n">9</span>').replace(
      '<div class="foot"><p>How to respond.</p></div>', "")
  cfg, _session_blocks, plan_mgr, meta = await conftest.make_plan_setup(tmp_path)
  file_rel = conftest.write_plan_artifact(cfg, meta.id, "plan_01.html", content=broken)

  cli_failures = {
      o.name for o in artifact_check.run_assertions("plan", cfg.sessions_dir / meta.id / file_rel, cfg) if not o.passed
  }
  assert cli_failures == {"sections-numbered", "foot-present"}

  with pytest.raises(ValueError) as exc_info:
    await plan_mgr.present(meta.id, file=file_rel, title="P1")
  message = str(exc_info.value)
  for name in cli_failures:
    assert name in message
  assert message.endswith(
      "Measure locally with: charliebot artifact check <artifact.html> --genre plan --assertions-only")


@pytest.mark.asyncio
async def test_plan_present_accepts_a_page_over_the_height_target(tmp_path: pathlib.Path) -> None:
  cfg, _session_blocks, plan_mgr, meta = await conftest.make_plan_setup(tmp_path)
  cfg.headless_chrome_bin = conftest.write_stub_chrome(tmp_path, 2600)
  file_rel = conftest.write_plan_artifact(cfg, meta.id, "plan_01.html")

  result = await plan_mgr.present(meta.id, file=file_rel, title="P1")

  assert result["v"] == 1


# ---------------------------------------------------------------------------
# byte-integrity
# ---------------------------------------------------------------------------

_DAMAGED_BYTES = [(b"\t", "0x09"), (b"\x0c", "0x0c"), (b"\r", "0x0d")]


@pytest.mark.parametrize("damaged_byte,hex_name", _DAMAGED_BYTES)
def test_byte_integrity_fails_on_non_lf_control_byte(
    tmp_path: pathlib.Path, damaged_byte: bytes, hex_name: str) -> None:
  """TAB / formfeed / CR in the raw source bytes fail the gate, each offset and value named."""
  doc = _sitrep_ok_doc().replace("Nothing.", f"Before{damaged_byte.decode('latin-1')}After")
  artifact = tmp_path / "damaged.html"
  artifact.write_bytes(doc.encode("utf-8"))
  (outcome,) = _run("sitrep", artifact)["byte-integrity"]
  assert not outcome.passed
  assert hex_name in outcome.detail
  assert "at offset" in outcome.detail
