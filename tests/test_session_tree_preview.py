"""The session-tree preview entry point: preparation, isolation, lifetime and gates.

The unit layer exercises the pure preparation/seed/gate mechanics in-process over
synthetic homes. The CLI layer invokes the actual ``session-tree preview`` parser
in a fresh process and asserts refusals happen before any side effect; the full
fresh-start behavioral case (real server, real fence, real HTTP, independent
second instance, restart) runs with the installed charlie-code launcher and is
marked local_only.
"""

from __future__ import annotations

import hashlib
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

import pytest
import yaml
from conftest import backend_option
from fastapi import WebSocket

import src.core.session_tree_preview as preview_module
from src.core.json_utils import atomic_write_text
from src.core.session_tree_preview import (
  PreviewRefusedError,
  PreviewUnavailableGate,
  PreviewWorkspaceError,
  _launcher_supports_session_dir,
  _new_access_key,
  activate_preview_environment,
  assert_no_bound_singletons,
  check_home_location,
  check_port,
  classify_home,
  install_native_session_isolation,
  is_preview_mode,
  make_workspace_guard,
  prepare_preview,
  preview_record_path,
  read_source_backend,
  read_source_backend_additions,
  refusal_reason,
  resolve_preview_home,
  seed_or_validate_preview_home,
  validate_existing_config,
  wrap_build_backend,
)

REPO_ROOT = Path(__file__).resolve().parents[1]

OPERATOR_KEY = "source-operator-key-0000"
PROVIDER_KEY = "provider-key-1111"


def write_source_home(home: Path, *, backend: dict | None = None, port: int = 18498,
                      with_binary: bool = True) -> dict:
  """One synthetic source (production-like) profile: minimal config + operator key."""
  home.mkdir(parents=True, exist_ok=True)
  if backend is None:
    backend = {
        "id": "clc-test", "label": "CLC Test", "type": "charlie-code",
        "model": "openai/fake-model", "api_base": "http://127.0.0.1:9/v1",
    }
  config = {
      "server": {"host": "127.0.0.1", "port": port},
      "paths": {"workspace_dirs": [str(home / "workspaces")], "worktree_dir": str(home / "worktrees")},
      "backends": {"options": [backend], "preference": [backend["id"]]},
  }
  (home / "config.yaml").write_text(yaml.safe_dump(config), encoding="utf-8")
  creds = f"charliebot:\n  access_key: {OPERATOR_KEY}\n"
  if backend.get("credential"):
    creds += f"{backend['credential']}:\n  api_key: {PROVIDER_KEY}\n"
    if backend["credential"] == "missing-section":
      creds = f"charliebot:\n  access_key: {OPERATOR_KEY}\n"
  (home / "credentials.yaml").write_text(creds, encoding="utf-8")
  return config


@pytest.fixture
def source_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """A synthetic source profile selected by the environment for this test."""
  home = tmp_path / "source-home"
  write_source_home(home)
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  monkeypatch.setattr(preview_module, "check_launcher", lambda: None)
  return home


# ---------------------------------------------------------------------------
# Pure path/ownership checks
# ---------------------------------------------------------------------------


def test_resolve_preview_home_requires_absolute(tmp_path: Path) -> None:
  with pytest.raises(PreviewRefusedError, match="absolute path"):
    resolve_preview_home("relative/dir")
  assert resolve_preview_home(str(tmp_path / "preview")) == tmp_path / "preview"
  target = tmp_path / "real-target"
  target.mkdir()
  link = tmp_path / "link"
  link.symlink_to(target)
  assert resolve_preview_home(str(link)) == target.resolve()


def test_check_home_location_refuses_overlaps(tmp_path: Path, source_home: Path) -> None:
  inside = source_home / "nested" / "preview"
  with pytest.raises(PreviewRefusedError, match="overlaps the production home"):
    check_home_location(inside, source_home=source_home, source_workspace_dirs=[])
  container = tmp_path / "container"
  prod_inside = container / ".charliebot"
  prod_inside.mkdir(parents=True)
  with pytest.raises(PreviewRefusedError, match="overlaps the production home"):
    check_home_location(container, source_home=prod_inside, source_workspace_dirs=[])
  workspace_inside = tmp_path / "workspaces" / "preview"
  with pytest.raises(PreviewRefusedError, match="overlaps the production workspace"):
    check_home_location(workspace_inside, source_home=tmp_path / "elsewhere",
                        source_workspace_dirs=[str(tmp_path / "workspaces")])
  with pytest.raises(PreviewRefusedError, match="overlaps the running checkout"):
    check_home_location(REPO_ROOT / "sub" / "dir", source_home=tmp_path / "elsewhere",
                        source_workspace_dirs=[])
  sibling = tmp_path / "sibling-preview"
  check_home_location(sibling, source_home=source_home,
                      source_workspace_dirs=[str(tmp_path / "workspaces")])


def test_check_port_refuses_source_port_and_occupied(source_home: Path) -> None:
  with pytest.raises(PreviewRefusedError, match="between 1 and 65535"):
    check_port(0, source_server_port=18498)
  with pytest.raises(PreviewRefusedError, match="source profile's server port"):
    check_port(18498, source_server_port=18498)
  with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    sock.listen(1)
    occupied = sock.getsockname()[1]
    with pytest.raises(PreviewRefusedError, match="not free"):
      check_port(occupied, source_server_port=18498)
  free = _free_port()
  check_port(free, source_server_port=18498)


def _free_port() -> int:
  with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    return int(sock.getsockname()[1])


def _write_record(home: Path, *, home_str: str | None = None, ready: bool = True) -> None:
  record = {
      "format_version": 1,
      "kind": "session-tree-preview",
      "home": home_str or str(home),
      "port": 1234,
      "ready": ready,
  }
  record_path = preview_record_path(home)
  record_path.parent.mkdir(parents=True, exist_ok=True)
  atomic_write_text(record_path, json.dumps(record))


def test_classify_home_fresh_existing_and_refusals(tmp_path: Path) -> None:
  assert classify_home(tmp_path / "absent") is True
  empty = tmp_path / "empty"
  empty.mkdir()
  assert classify_home(empty) is True
  as_file = tmp_path / "a-file"
  as_file.write_text("x")
  with pytest.raises(PreviewRefusedError, match="not a directory"):
    classify_home(as_file)
  unrelated = tmp_path / "unrelated"
  (unrelated / "sessions" / "abc").mkdir(parents=True)
  (unrelated / "config.yaml").write_text("server: {host: 127.0.0.1, port: 1}\n")
  with pytest.raises(PreviewRefusedError, match="not a session-tree preview home") as exc:
    classify_home(unrelated)
  assert any("config.yaml" in detail for detail in exc.value.details)
  legacy = tmp_path / "legacy"
  (legacy / "sessions" / "abc").mkdir(parents=True)
  (legacy / "sessions" / "abc" / "metadata.json").write_text(json.dumps({"id": "abc"}))
  with pytest.raises(PreviewRefusedError, match="not a session-tree preview home") as exc:
    classify_home(legacy)
  assert any("legacy v1 session" in detail for detail in exc.value.details)
  migrated = tmp_path / "migrated"
  migrated.mkdir()
  (migrated / "session_tree_migration.json").write_text("{}")
  with pytest.raises(PreviewRefusedError, match="not a session-tree preview home") as exc:
    classify_home(migrated)
  assert any("migration products" in detail for detail in exc.value.details)
  valid = tmp_path / "valid"
  valid.mkdir()
  _write_record(valid)
  assert classify_home(valid) is False
  foreign = tmp_path / "foreign"
  foreign.mkdir()
  _write_record(foreign, home_str="/somewhere/else")
  with pytest.raises(PreviewRefusedError, match="not a session-tree preview home"):
    classify_home(foreign)


# ---------------------------------------------------------------------------
# Backend selection and launcher preflight
# ---------------------------------------------------------------------------


def test_read_source_backend_validations(source_home: Path) -> None:
  entry, credential = read_source_backend("clc-test")
  assert entry["id"] == "clc-test"
  assert entry["type"] == "charlie-code"
  assert credential is None
  with pytest.raises(PreviewRefusedError, match="not configured in the source profile"):
    read_source_backend("no-such-backend")
  with pytest.raises(PreviewRefusedError, match="requires --backend"):
    read_source_backend("")


def test_read_source_backend_refuses_unisolated_backend_types(tmp_path: Path,
                                                              monkeypatch: pytest.MonkeyPatch) -> None:
  home = tmp_path / "source"
  write_source_home(home, backend={
      "id": "cc-claude-entry", "label": "CC", "type": "cc-claude", "model": "claude-x"})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  with pytest.raises(PreviewRefusedError, match="only for charlie-code"):
    read_source_backend("cc-claude-entry")


def test_read_source_backend_requires_referenced_credential(tmp_path: Path,
                                                            monkeypatch: pytest.MonkeyPatch) -> None:
  home = tmp_path / "source"
  write_source_home(home, backend={
      "id": "clc-cred", "label": "CLC", "type": "charlie-code", "model": "openai/fake",
      "api_base": "http://127.0.0.1:9/v1", "credential": "missing-section"})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  with pytest.raises(PreviewRefusedError, match="credentials.missing-section.api_key"):
    read_source_backend("clc-cred")


def test_read_source_backend_copies_only_the_referenced_credential(tmp_path: Path,
                                                                   monkeypatch: pytest.MonkeyPatch) -> None:
  home = tmp_path / "source"
  write_source_home(home, backend={
      "id": "clc-cred", "label": "CLC", "type": "charlie-code", "model": "openai/fake",
      "api_base": "http://127.0.0.1:9/v1", "credential": "provider"})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  entry, credential = read_source_backend("clc-cred")
  assert credential == ("provider", {"api_key": PROVIDER_KEY})
  assert set(entry)  # the entry itself travels; the operator key never does
  assert OPERATOR_KEY not in json.dumps(entry)


def test_prepare_preview_wraps_unreadable_source_profile(tmp_path: Path,
                                                         monkeypatch: pytest.MonkeyPatch) -> None:
  home = tmp_path / "source"
  write_source_home(home)
  (home / "config.yaml").write_text("server: [broken, shape]\n", encoding="utf-8")
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  with pytest.raises(PreviewRefusedError, match="could not read the source profile"):
    prepare_preview(str(tmp_path / "trial"), _free_port(), "clc-test")
  assert not (tmp_path / "trial").exists()


def test_check_launcher_refusals(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(preview_module, "shutil_which", lambda binary: None)
  with pytest.raises(PreviewRefusedError, match="launcher is not installed"):
    preview_module.check_launcher()
  monkeypatch.setattr(preview_module, "shutil_which", lambda binary: "/fake/charlie-code")
  monkeypatch.setattr(preview_module, "_launcher_supports_session_dir", lambda binary: False)
  with pytest.raises(PreviewRefusedError, match="--session-dir"):
    preview_module.check_launcher()
  monkeypatch.setattr(preview_module, "_launcher_supports_session_dir", lambda binary: True)
  preview_module.check_launcher()


def test_launcher_supports_session_dir_runs_the_real_binary() -> None:
  binary = preview_module.shutil_which("charlie-code")
  if binary is None:
    pytest.skip("charlie-code is not installed on this host")
  assert _launcher_supports_session_dir(binary) is True


# ---------------------------------------------------------------------------
# Existing preview home validation
# ---------------------------------------------------------------------------


def _existing_preview_home(tmp_path: Path, *, backend: dict | None = None) -> Path:
  home = tmp_path / "preview-home"
  seed_config = {
      "server": {"host": "127.0.0.1", "port": 18500},
      "paths": {"workspace_dirs": [str(home / "workspaces")], "worktree_dir": str(home / "worktrees")},
      "backends": {"options": [backend or {
          "id": "clc-test", "label": "CLC", "type": "charlie-code", "model": "openai/fake",
          "api_base": "http://127.0.0.1:9/v1"}], "preference": ["clc-test"]},
  }
  home.mkdir(parents=True)
  (home / "config.yaml").write_text(yaml.safe_dump(seed_config), encoding="utf-8")
  (home / "credentials.yaml").write_text("charliebot:\n  access_key: preview-key-old\n", encoding="utf-8")
  _write_record(home)
  return home


def test_validate_existing_config_accepts_the_seeded_shape(tmp_path: Path) -> None:
  home = _existing_preview_home(tmp_path)
  data = validate_existing_config(home)
  assert data["backends"]["options"][0]["id"] == "clc-test"


@pytest.mark.parametrize("mutate,match", [
    (lambda d: d.update(slack={"allowed_user_ids": [1]}), "outside the preview trial contract"),
    (lambda d: d.pop("paths"), "missing the preview trial sections"),
    (lambda d: d["backends"].update(options=[]), "at least one backend"),
    (lambda d: d["backends"].update(options=[
        {"id": "a", "label": "A", "type": "charlie-code", "model": "m", "api_base": "http://x"},
        {"id": "a", "label": "A2", "type": "charlie-code", "model": "m", "api_base": "http://x"}]),
     "more than once"),
    (lambda d: d["backends"].update(options=[
        {"id": "cc", "label": "CC", "type": "cc-claude", "model": "m"}]), "non-charlie-code backend"),
    (lambda d: d["paths"].update(workspace_dirs=["/usr/share"]), "workspace_dirs outside"),
    (lambda d: d["paths"].update(worktree_dir="/tmp/wt"), "worktree_dir outside"),
])
def test_validate_existing_config_refuses_unsafe_shapes(tmp_path: Path, mutate, match: str) -> None:
  home = _existing_preview_home(tmp_path)
  data = validate_existing_config(home)
  mutate(data)
  (home / "config.yaml").write_text(yaml.safe_dump(data), encoding="utf-8")
  with pytest.raises(PreviewRefusedError, match=match):
    validate_existing_config(home)


def test_validate_existing_config_requires_the_backend_credential(tmp_path: Path) -> None:
  home = _existing_preview_home(tmp_path, backend={
      "id": "clc-cred", "label": "CLC", "type": "charlie-code", "model": "openai/fake",
      "api_base": "http://127.0.0.1:9/v1", "credential": "provider"})
  with pytest.raises(PreviewRefusedError, match="provider.api_key"):
    validate_existing_config(home)


# ---------------------------------------------------------------------------
# Seeding and restart
# ---------------------------------------------------------------------------


def _setup_for(home: Path, port: int, *, fresh: bool) -> "preview_module.PreviewSetup":
  return preview_module.PreviewSetup(
      home=home, port=port, url=f"http://127.0.0.1:{port}", backend_id="clc-test",
      backend_entry={"id": "clc-test", "label": "CLC", "type": "charlie-code",
                     "model": "openai/fake", "api_base": "http://127.0.0.1:9/v1"},
      backend_additions=[], credential_section=None, fresh_hint=fresh, source_branch="b",
      source_sha="s" * 40, started_at="2026-01-01T00:00:00+00:00",
      log_path=home / "logs" / "preview.log", native_sessions_dir=home / "clc-sessions",
      access_key=_new_access_key())


def test_seed_fresh_home_writes_private_minimal_config(tmp_path: Path) -> None:
  home = tmp_path / "fresh-home"
  setup = _setup_for(home, 18501, fresh=True)
  seed_or_validate_preview_home(setup)
  data = yaml.safe_load((home / "config.yaml").read_text())
  assert set(data) == {"server", "paths", "backends"}
  assert data["server"] == {"host": "127.0.0.1", "port": 18501}
  assert data["paths"]["workspace_dirs"] == [str(home / "workspaces")]
  assert data["paths"]["worktree_dir"] == str(home / "worktrees")
  assert [entry["id"] for entry in data["backends"]["options"]] == ["clc-test"]
  assert data["backends"]["preference"] == ["clc-test"]
  creds_path = home / "credentials.yaml"
  assert yaml.safe_load(creds_path.read_text())["charliebot"]["access_key"] == setup.access_key
  assert "openai/fake" in (home / "config.yaml").read_text()
  mode = creds_path.stat().st_mode & 0o777
  assert mode == 0o600
  for dirname in ("clc-sessions", "workspaces", "worktrees", "logs"):
    assert (home / dirname).is_dir()
  record = json.loads(preview_record_path(home).read_text())
  assert record["ready"] is False
  assert record["backend"] == "clc-test"
  assert record["source_branch"] == "b"
  assert record["source_sha"] == "s" * 40
  assert record["pid"] == os.getpid()
  assert record["pid_start"]


def test_restart_preserves_config_tasks_and_key(tmp_path: Path) -> None:
  home = _existing_preview_home(tmp_path)
  (home / "sessions" / "task-1").mkdir(parents=True)
  (home / "sessions" / "task-1" / "metadata.json").write_text(
      json.dumps({"id": "task-1", "profile": "manager", "schema_version": 2}))
  config_before = (home / "config.yaml").read_text()
  key_before = yaml.safe_load((home / "credentials.yaml").read_text())["charliebot"]["access_key"]
  setup = _setup_for(home, 18500, fresh=False)
  setup.access_key = "should-be-replaced"
  seed_or_validate_preview_home(setup)
  assert (home / "config.yaml").read_text() == config_before
  assert yaml.safe_load((home / "credentials.yaml").read_text())["charliebot"]["access_key"] == key_before
  assert (home / "sessions" / "task-1" / "metadata.json").is_file()
  record = json.loads(preview_record_path(home).read_text())
  assert record["port"] == 18500


def test_restart_updates_only_the_bind_address(tmp_path: Path) -> None:
  home = _existing_preview_home(tmp_path)
  setup = _setup_for(home, 18600, fresh=False)
  seed_or_validate_preview_home(setup)
  data = yaml.safe_load((home / "config.yaml").read_text())
  assert data["server"] == {"host": "127.0.0.1", "port": 18600}
  assert data["paths"]["workspace_dirs"] == [str(home / "workspaces")]
  assert data["backends"]["options"][0]["id"] == "clc-test"


# ---------------------------------------------------------------------------
# Multi-entry catalog: explicitly selected additions
# ---------------------------------------------------------------------------


def _write_multi_source_home(home: Path, *, entries: list[dict], port: int = 18498) -> None:
  """A synthetic source profile with several charlie-code entries."""
  home.mkdir(parents=True, exist_ok=True)
  config = {
      "server": {"host": "127.0.0.1", "port": port},
      "paths": {"workspace_dirs": [str(home / "workspaces")], "worktree_dir": str(home / "worktrees")},
      "backends": {"options": entries, "preference": [entries[0]["id"]]},
  }
  (home / "config.yaml").write_text(yaml.safe_dump(config), encoding="utf-8")
  creds = f"charliebot:\n  access_key: {OPERATOR_KEY}\n"
  for entry in entries:
    if entry.get("credential") and entry["credential"] != "missing-section":
      creds += f"{entry['credential']}:\n  api_key: {PROVIDER_KEY}\n"
  (home / "credentials.yaml").write_text(creds, encoding="utf-8")


@pytest.fixture
def multi_source_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  home = tmp_path / "source-home"
  _write_multi_source_home(home, entries=[
      {"id": "clc-default", "label": "CLC Default", "type": "charlie-code",
       "model": "openai/default-model", "api_base": "http://127.0.0.1:9/v1"},
      {"id": "clc-second", "label": "CLC Second", "type": "charlie-code",
       "model": "openai/second-model", "api_base": "http://127.0.0.1:9/v1", "top_p": 0.95},
      {"id": "clc-cred", "label": "CLC Cred", "type": "charlie-code",
       "model": "openai/cred-model", "api_base": "http://127.0.0.1:9/v1", "credential": "provider"},
  ])
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  monkeypatch.setattr(preview_module, "check_launcher", lambda: None)
  return home


def test_read_source_backend_additions_read_entries_and_credentials_in_order(
    multi_source_home: Path) -> None:
  additions = read_source_backend_additions(["clc-second", "clc-cred"], exclude_ids={"clc-default"})
  assert [entry["id"] for entry, _ in additions] == ["clc-second", "clc-cred"]
  assert additions[0][0]["top_p"] == 0.95, "declared sampling settings travel with the entry"
  assert additions[0][1] is None
  assert additions[1][1] == ("provider", {"api_key": PROVIDER_KEY})


def test_read_source_backend_additions_refusals(multi_source_home: Path) -> None:
  with pytest.raises(PreviewRefusedError, match="requested more than once"):
    read_source_backend_additions(["clc-second", "clc-second"], exclude_ids=set())
  with pytest.raises(PreviewRefusedError, match="already part of this trial's backend selection"):
    read_source_backend_additions(["clc-default"], exclude_ids={"clc-default"})
  with pytest.raises(PreviewRefusedError, match="not configured in the source profile"):
    read_source_backend_additions(["no-such"], exclude_ids=set())


def test_read_source_backend_additions_refuse_unisolated_type_missing_credential_and_unrunnable_entry(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = tmp_path / "source"
  _write_multi_source_home(home, entries=[
      {"id": "clc-base", "label": "Base", "type": "charlie-code",
       "model": "openai/m", "api_base": "http://127.0.0.1:9/v1"},
      {"id": "other-family", "label": "Other", "type": "codex", "model": "m"},
      {"id": "clc-cred", "label": "Cred", "type": "charlie-code",
       "model": "openai/m", "api_base": "http://127.0.0.1:9/v1", "credential": "missing-section"},
      {"id": "clc-noapi", "label": "NoApi", "type": "charlie-code", "model": "openai/m"},
  ])
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  with pytest.raises(PreviewRefusedError, match="only for charlie-code"):
    read_source_backend_additions(["other-family"], exclude_ids=set())
  with pytest.raises(PreviewRefusedError, match="credentials.missing-section.api_key"):
    read_source_backend_additions(["clc-cred"], exclude_ids=set())
  with pytest.raises(PreviewRefusedError, match="declares no api_base"):
    read_source_backend_additions(["clc-noapi"], exclude_ids=set())


def test_seed_fresh_home_writes_all_requested_entries(tmp_path: Path) -> None:
  home = tmp_path / "fresh-home"
  setup = _setup_for(home, 18501, fresh=True)
  setup.backend_additions = [
      ({"id": "clc-second", "label": "CLC Second", "type": "charlie-code",
        "model": "openai/second", "api_base": "http://127.0.0.1:9/v1", "top_p": 0.95}, None),
      ({"id": "clc-cred", "label": "CLC Cred", "type": "charlie-code",
        "model": "openai/cred", "api_base": "http://127.0.0.1:9/v1"}, ("provider", {"api_key": PROVIDER_KEY})),
  ]
  seed_or_validate_preview_home(setup)
  data = yaml.safe_load((home / "config.yaml").read_text())
  assert [entry["id"] for entry in data["backends"]["options"]] == ["clc-test", "clc-second", "clc-cred"]
  assert data["backends"]["preference"] == ["clc-test"], "the explicitly requested default never moves"
  creds = yaml.safe_load((home / "credentials.yaml").read_text())
  assert creds["provider"] == {"api_key": PROVIDER_KEY}
  assert creds["charliebot"]["access_key"] == setup.access_key
  record = json.loads(preview_record_path(home).read_text())
  assert record["backend"] == "clc-test"


def _existing_home_with_task(tmp_path: Path) -> tuple[Path, Path]:
  home = _existing_preview_home(tmp_path)
  task_dir = home / "sessions" / "task-1"
  task_dir.mkdir(parents=True)
  (task_dir / "metadata.json").write_text(
      json.dumps({"id": "task-1", "profile": "manager", "schema_version": 2}))
  (task_dir / "data").mkdir()
  (task_dir / "data" / "chat_events.jsonl").write_text('{"type": "task_created"}\n')
  native = home / "clc-sessions"
  native.mkdir(exist_ok=True)
  (native / "native-run-1").mkdir()
  return home, task_dir


def _two_additions() -> list[tuple[dict, tuple[str, str] | None]]:
  return [
      ({"id": "clc-add-1", "label": "CLC Add 1", "type": "charlie-code",
        "model": "openai/add-1", "api_base": "http://127.0.0.1:9/v1"}, None),
      ({"id": "clc-add-2", "label": "CLC Add 2", "type": "charlie-code",
        "model": "openai/add-2", "api_base": "http://127.0.0.1:9/v1"},
       ("provider", {"api_key": PROVIDER_KEY})),
  ]


def test_seed_existing_home_extends_catalog_and_keeps_everything_else(tmp_path: Path) -> None:
  home, task_dir = _existing_home_with_task(tmp_path)
  config_before = (home / "config.yaml").read_text()
  key_before = yaml.safe_load((home / "credentials.yaml").read_text())["charliebot"]["access_key"]
  setup = _setup_for(home, 18500, fresh=False)
  setup.backend_additions = _two_additions()
  seed_or_validate_preview_home(setup)
  data = yaml.safe_load((home / "config.yaml").read_text())
  assert [entry["id"] for entry in data["backends"]["options"]] == ["clc-test", "clc-add-1", "clc-add-2"]
  assert data["backends"]["preference"] == ["clc-test"], "the stored default stays the default"
  assert data["server"] == {"host": "127.0.0.1", "port": 18500}
  assert data["paths"] == yaml.safe_load(config_before)["paths"]
  creds = yaml.safe_load((home / "credentials.yaml").read_text())
  assert creds["charliebot"]["access_key"] == key_before, "the access key is never rekeyed"
  assert creds["provider"] == {"api_key": PROVIDER_KEY}
  assert (task_dir / "metadata.json").is_file()
  assert (task_dir / "data" / "chat_events.jsonl").is_file()
  assert (home / "clc-sessions" / "native-run-1").is_dir()


def test_seed_existing_home_refuses_a_non_mapping_credentials_store_untouched(tmp_path: Path) -> None:
  """Every validation fires before the first write: a credentials store the extension cannot
  read refuses the addition and leaves the home byte-identical (a half-extended config with
  no credential would be a silent broken entry)."""
  home, _task_dir = _existing_home_with_task(tmp_path)
  (home / "credentials.yaml").write_text("- not\n- a\n- mapping\n", encoding="utf-8")
  config_before = (home / "config.yaml").read_text()
  creds_before = (home / "credentials.yaml").read_text()
  setup = _setup_for(home, 18500, fresh=False)
  setup.backend_additions = _two_additions()
  with pytest.raises(PreviewRefusedError, match="not a credentials mapping"):
    seed_or_validate_preview_home(setup)
  assert (home / "config.yaml").read_text() == config_before
  assert (home / "credentials.yaml").read_text() == creds_before


def test_seed_existing_home_refuses_an_already_present_addition_untouched(tmp_path: Path) -> None:
  home, _task_dir = _existing_home_with_task(tmp_path)
  config_before = (home / "config.yaml").read_text()
  creds_before = (home / "credentials.yaml").read_text()
  setup = _setup_for(home, 18500, fresh=False)
  setup.backend_additions = [
      ({"id": "clc-test", "label": "CLC", "type": "charlie-code",
        "model": "openai/fake", "api_base": "http://127.0.0.1:9/v1"}, None),
  ]
  with pytest.raises(PreviewRefusedError, match="already in the preview home's catalog"):
    seed_or_validate_preview_home(setup)
  assert (home / "config.yaml").read_text() == config_before
  assert (home / "credentials.yaml").read_text() == creds_before


def test_existing_multi_entry_home_restarts_untouched(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(preview_module, "check_ui_assets", lambda: None)
  home = _existing_preview_home(tmp_path)
  data = yaml.safe_load((home / "config.yaml").read_text())
  data["backends"]["options"] = [
      {"id": "clc-a", "label": "A", "type": "charlie-code", "model": "openai/a", "api_base": "http://x"},
      {"id": "clc-b", "label": "B", "type": "charlie-code", "model": "openai/b", "api_base": "http://x"},
      {"id": "clc-c", "label": "C", "type": "charlie-code", "model": "openai/c", "api_base": "http://x",
       "credential": "provider", "top_p": 0.9},
  ]
  data["backends"]["preference"] = ["clc-a"]
  (home / "config.yaml").write_text(yaml.safe_dump(data), encoding="utf-8")
  (home / "credentials.yaml").write_text(
      "charliebot:\n  access_key: preview-key-old\nprovider:\n  api_key: provider-key-1111\n",
      encoding="utf-8")
  config_before = (home / "config.yaml").read_text()
  setup = prepare_preview(str(home), 18500, None)
  assert setup.backend_id == "clc-a"
  assert setup.backend_additions == []
  assert setup.access_key == "preview-key-old"
  seed_or_validate_preview_home(setup)
  assert (home / "config.yaml").read_text() == config_before, "a no-flag restart rewrites nothing"


def test_seeded_sampling_entry_keeps_top_p_through_the_real_argv(tmp_path: Path,
                                                                 monkeypatch: pytest.MonkeyPatch) -> None:
  """The declared sampling setting survives the seed and reaches the real backend argv."""
  home = tmp_path / "fresh-home"
  setup = _setup_for(home, 18501, fresh=True)
  setup.backend_entry = {"id": "clc-test", "label": "CLC", "type": "charlie-code",
                         "model": "openai/fake", "api_base": "http://127.0.0.1:9/v1", "top_p": 0.95}
  seed_or_validate_preview_home(setup)
  seeded = yaml.safe_load((home / "config.yaml").read_text())["backends"]["options"][0]
  assert seeded["top_p"] == 0.95, "the consumed setting is never stripped from the copied entry"

  import shutil as real_shutil

  fake_binary = tmp_path / "fake-charlie-code"
  fake_binary.write_text("#!/bin/sh\nexit 0\n")
  fake_binary.chmod(0o755)
  monkeypatch.setattr(real_shutil, "which",
                      lambda name, *a, **k: str(fake_binary) if name == "charlie-code" else None)
  registry = __import__("src.agents.backends.registry", fromlist=["build_backend"])
  original = registry.build_backend
  registry.build_backend = wrap_build_backend(original, home / "clc-sessions")
  try:
    option = backend_option(**seeded)
    from src.core.config import CharlieBotConfig

    backend_obj = registry.build_backend(option, CharlieBotConfig(charliebot_home=home))
    backend_obj._prepare_transport(tmp_path)
    argv = backend_obj._build_command("prompt")
    top_p_idx = argv.index("--top-p")
    assert argv[top_p_idx + 1] == "0.95"
    assert f"--session-dir {home / 'clc-sessions'}" in " ".join(argv)
  finally:
    registry.build_backend = original


# ---------------------------------------------------------------------------
# Preparation
# ---------------------------------------------------------------------------


def test_prepare_preview_fresh_happy_path(tmp_path: Path, source_home: Path,
                                          monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(preview_module, "check_ui_assets", lambda: None)
  port = _free_port()
  home = tmp_path / "trial"
  setup = prepare_preview(str(home), port, "clc-test")
  assert setup.home == home
  assert setup.url == f"http://127.0.0.1:{port}"
  assert setup.backend_id == "clc-test"
  assert setup.backend_entry["api_base"] == "http://127.0.0.1:9/v1"
  assert setup.fresh_hint is True
  assert setup.access_key.startswith("preview-key-")
  assert setup.access_key != OPERATOR_KEY
  assert not home.exists(), "preparation must not write before the fence is held"


def test_prepare_preview_restart_uses_stored_backend(tmp_path: Path, source_home: Path,
                                                     monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(preview_module, "check_ui_assets", lambda: None)
  home = _existing_preview_home(tmp_path)
  setup = prepare_preview(str(home), 18500, None)
  assert setup.backend_id == "clc-test"
  assert setup.fresh_hint is False
  assert setup.access_key == "preview-key-old"
  with pytest.raises(PreviewRefusedError, match="does not match the preview home's configured backend"):
    prepare_preview(str(home), 18500, "other-backend")


def test_prepare_preview_refuses_source_home_and_port(tmp_path: Path, source_home: Path) -> None:
  with pytest.raises(PreviewRefusedError, match="overlaps the production home"):
    prepare_preview(str(source_home / "nested"), _free_port(), "clc-test")
  with pytest.raises(PreviewRefusedError, match="source profile's server port"):
    prepare_preview(str(tmp_path / "trial"), 18498, "clc-test")


def test_prepare_preview_refuses_unsafe_existing_content(tmp_path: Path, source_home: Path) -> None:
  unrelated = tmp_path / "unrelated"
  unrelated.mkdir()
  (unrelated / "config.yaml").write_text("server: {port: 1}\n")
  with pytest.raises(PreviewRefusedError, match="not a session-tree preview home"):
    prepare_preview(str(unrelated), _free_port(), "clc-test")


# ---------------------------------------------------------------------------
# Environment selection
# ---------------------------------------------------------------------------


def test_activate_preview_environment_switches_and_clears(tmp_path: Path, source_home: Path,
                                                          monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "inherited-session")
  monkeypatch.setenv("CHARLIEBOT_RUN_TOKEN", "inherited-token")
  monkeypatch.setenv("CHARLIE_CODE_API_KEY", "inherited-provider-key")
  home = tmp_path / "trial-home"
  home.mkdir()
  (home / "config.yaml").write_text(yaml.safe_dump({
      "server": {"host": "127.0.0.1", "port": 18501},
      "paths": {"workspace_dirs": [str(home / "workspaces")], "worktree_dir": str(home / "worktrees")},
      "backends": {"options": [{
          "id": "clc-test", "label": "CLC", "type": "charlie-code", "model": "openai/fake",
          "api_base": "http://127.0.0.1:9/v1"}], "preference": ["clc-test"]},
  }), encoding="utf-8")
  (home / "credentials.yaml").write_text("charliebot:\n  access_key: preview-key-x\n", encoding="utf-8")
  setup = _setup_for(home, 18501, fresh=False)
  activate_preview_environment(setup)
  assert os.environ["CHARLIEBOT_HOME"] == str(home)
  for var in ("CHARLIEBOT_SESSION_ID", "CHARLIEBOT_RUN_TOKEN", "CHARLIE_CODE_API_KEY"):
    assert var not in os.environ
  from src.core.config import charliebot_home_dir, load_config

  assert charliebot_home_dir() == home
  assert load_config().charliebot_home == home
  monkeypatch.delenv("CHARLIEBOT_HOME")


def test_assert_no_bound_singletons(monkeypatch: pytest.MonkeyPatch) -> None:
  """The guard accepts an explicitly unbound deps module and refuses any bound singleton.

  The deps singletons are process-global and the suite legitimately binds them before this
  test runs (a fired trigger resolves its task-tree provider through ``deps.task_manager``,
  which constructs and caches the real managers), so the clean case is arranged explicitly
  here instead of assuming an untouched module; monkeypatch restores the previous state, so
  the assertion stays independent of the tests that precede it.
  """
  from src.api import deps

  singleton_names = ("_session_manager", "_thread_manager", "_trigger_manager", "_task_manager")
  for name in singleton_names:
    monkeypatch.setattr(deps, name, None, raising=False)
  assert_no_bound_singletons()
  for name in singleton_names:
    monkeypatch.setattr(deps, name, object())
    with pytest.raises(PreviewRefusedError, match="bound before the preview environment") as excinfo:
      assert_no_bound_singletons()
    assert name in str(excinfo.value)


# ---------------------------------------------------------------------------
# Native-session isolation wrapper and real argv
# ---------------------------------------------------------------------------


def test_wrap_build_backend_passes_session_dir_only_to_charlie_code(tmp_path: Path) -> None:
  calls: list[tuple[object, dict]] = []

  def original(option, cfg, **kwargs):
    calls.append((option, kwargs))
    return "backend"

  wrapped = wrap_build_backend(original, tmp_path / "clc-sessions")
  clc = backend_option(id="clc", label="CLC", type="charlie-code", model="m", api_base="http://x")
  assert wrapped(clc, None) == "backend"
  assert calls[-1][1]["extra_flags"] == ["--session-dir", str(tmp_path / "clc-sessions")]
  cc = backend_option(id="cc", label="CC", type="cc-claude", model="m")
  assert wrapped(cc, None) == "backend"
  assert "extra_flags" not in calls[-1][1]
  clc_resume = backend_option(id="clc2", label="CLC2", type="charlie-code", model="m",
                              api_base="http://x")
  wrapped(clc_resume, None, extra_flags=["--resume", "abc"])
  assert calls[-1][1]["extra_flags"] == ["--resume", "abc", "--session-dir",
                                         str(tmp_path / "clc-sessions")]


def test_wrapped_registry_build_backend_builds_real_charlie_code_argv(tmp_path: Path,
                                                                      monkeypatch: pytest.MonkeyPatch) -> None:
  """The real backend construction path: the wrapper's flag reaches the child argv."""
  import shutil as real_shutil

  fake_binary = tmp_path / "fake-charlie-code"
  fake_binary.write_text("#!/bin/sh\nexit 0\n")
  fake_binary.chmod(0o755)
  monkeypatch.setattr(real_shutil, "which",
                      lambda name, *a, **k: str(fake_binary) if name == "charlie-code" else None)
  clc_sessions = tmp_path / "clc-sessions"
  clc_sessions.mkdir()
  registry = __import__("src.agents.backends.registry", fromlist=["build_backend"])
  original = registry.build_backend
  registry.build_backend = wrap_build_backend(original, clc_sessions)
  try:
    option = backend_option(id="clc", label="CLC", type="charlie-code", model="openai/fake",
                            api_base="http://127.0.0.1:9/v1")
    from src.core.config import CharlieBotConfig

    cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
    backend_obj = registry.build_backend(option, cfg)
    backend_obj._prepare_transport(tmp_path)
    argv = backend_obj._build_command("prompt")
    joined = " ".join(argv)
    assert f"--session-dir {clc_sessions}" in joined
    assert "--task-file" in joined
  finally:
    registry.build_backend = original


def test_every_catalog_entry_builds_with_the_preview_native_session_dir(tmp_path: Path,
                                                                        monkeypatch: pytest.MonkeyPatch) -> None:
  """All selectable models' constructions (manager turns, workers, reviews,
  retries and continuations all resolve through the registry) carry the
  preview-owned --session-dir, and only charlie-code entries do."""
  import shutil as real_shutil

  fake_binary = tmp_path / "fake-charlie-code"
  fake_binary.write_text("#!/bin/sh\nexit 0\n")
  fake_binary.chmod(0o755)
  monkeypatch.setattr(real_shutil, "which",
                      lambda name, *a, **k: str(fake_binary) if name == "charlie-code" else None)
  home = tmp_path / "fresh-home"
  setup = _setup_for(home, 18501, fresh=True)
  setup.backend_additions = _two_additions()
  seed_or_validate_preview_home(setup)
  from src.core.config import load_config

  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  cfg = load_config()
  assert [option.id for option in cfg.backends.options] == ["clc-test", "clc-add-1", "clc-add-2"], \
      "the seeded catalog is what the constructions resolve"
  clc_sessions = home / "clc-sessions"
  registry = __import__("src.agents.backends.registry", fromlist=["build_backend"])
  original = registry.build_backend
  registry.build_backend = wrap_build_backend(original, clc_sessions)
  try:
    for option in cfg.backends.options:
      built = registry.build_backend(option, cfg)
      built._prepare_transport(tmp_path)
      argv = built._build_command("prompt")
      assert f"--session-dir {clc_sessions}" in " ".join(argv), option.id
  finally:
    registry.build_backend = original


def test_install_native_session_isolation_patches_both_build_targets(tmp_path: Path) -> None:
  registry = __import__("src.agents.backends.registry", fromlist=["build_backend"])
  worker_module = __import__("src.agents.worker", fromlist=["build_backend"])
  original_registry = registry.build_backend
  original_worker = worker_module.__dict__.get("build_backend")
  try:
    install_native_session_isolation(tmp_path / "clc-sessions")
    assert registry.build_backend is not original_registry
    assert worker_module.__dict__["build_backend"] is registry.build_backend
  finally:
    registry.build_backend = original_registry
    if original_worker is None:
      worker_module.__dict__.pop("build_backend", None)
    else:
      worker_module.__dict__["build_backend"] = original_worker


# ---------------------------------------------------------------------------
# Launcher workspace boundary
# ---------------------------------------------------------------------------


def test_workspace_guard_refuses_outside_and_accepts_inside(tmp_path: Path) -> None:
  from src.core.config import CharlieBotConfig

  home = tmp_path / "trial-home"
  cfg = CharlieBotConfig(
      charliebot_home=home,
      paths={"workspace_dirs": [str(home / "workspaces")], "worktree_dir": str(home / "worktrees")})
  guard = make_workspace_guard(cfg)
  with pytest.raises(PreviewWorkspaceError, match="workspace boundary"):
    guard(REPO_ROOT)
  inside = home / "workspaces" / "synthetic-repo"
  inside.mkdir(parents=True)
  guard(inside)
  guard(home / "workspaces")


@pytest.mark.asyncio
async def test_prepare_worktree_refuses_outside_repo_before_any_git_call(tmp_path: Path) -> None:
  from src.core.config import CharlieBotConfig
  from src.core.sessions import SessionManager
  from src.core.task_execution import TaskExecutionAdapter
  from src.core.task_sessions import TaskTreeManager

  home = tmp_path / "trial-home"
  cfg = CharlieBotConfig(
      charliebot_home=home,
      paths={"workspace_dirs": [str(home / "workspaces")], "worktree_dir": str(home / "worktrees")})
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  adapter = TaskExecutionAdapter(cfg, session_mgr, tree)
  adapter.launch_workspace_guard = make_workspace_guard(cfg)
  run_record = type("RunRecord", (), {"id": "r"})()
  meta = type("Meta", (), {"id": "s"})()
  with pytest.raises(PreviewWorkspaceError, match="workspace boundary"):
    await adapter._prepare_worktree(meta, run_record, REPO_ROOT)


# ---------------------------------------------------------------------------
# Reachable-mechanism gate
# ---------------------------------------------------------------------------


def _gated_probe_app():
  from fastapi import FastAPI
  from fastapi.routing import APIRouter

  probe = APIRouter()

  @probe.get("/api/cron/tasks")
  async def list_tasks():
    return []

  @probe.post("/api/cron/tasks")
  async def create_task():
    return {"ok": True}

  @probe.put("/api/cron/tasks/{name}")
  async def update_task(name: str):
    return {"ok": True}

  @probe.delete("/api/cron/tasks/{name}")
  async def delete_task(name: str):
    return {"ok": True}

  @probe.post("/api/internal/schedule-trigger")
  async def schedule_trigger():
    return {"ok": True}

  @probe.post("/api/internal/slack/reply")
  async def slack_reply():
    return {"ok": True}

  @probe.post("/api/internal/slack/ack")
  async def slack_ack():
    return {"ok": True}

  @probe.post("/api/sessions/")
  async def create_session():
    return {"ok": True}

  @probe.websocket("/ws/terminal")
  async def terminal(websocket: WebSocket):
    await websocket.accept()
    await websocket.send_text("should never be reached")

  @probe.websocket("/ws/sessions/{session_id}")
  async def session_ws(websocket: WebSocket, session_id: str):
    await websocket.accept()
    await websocket.send_text("hello " + session_id)
    await websocket.close()

  app = FastAPI()
  app.include_router(probe)
  app.add_middleware(PreviewUnavailableGate)
  return app


def test_preview_gate_refuses_disabled_mutations_and_passes_the_rest() -> None:
  from fastapi.testclient import TestClient

  client = TestClient(_gated_probe_app())
  assert client.get("/api/cron/tasks").status_code == 200
  refused = client.post("/api/cron/tasks", json={})
  assert refused.status_code == 403
  assert "Scheduled task management is disabled" in refused.json()["detail"]
  assert client.put("/api/cron/tasks/x", json={}).status_code == 403
  assert client.delete("/api/cron/tasks/x").status_code == 403
  for path in ("/api/internal/schedule-trigger", "/api/internal/slack/reply", "/api/internal/slack/ack"):
    response = client.post(path, json={})
    assert response.status_code == 403, path
    assert "External messaging and delayed triggers are disabled" in response.json()["detail"]
  assert client.get("/api/unknown").status_code == 404  # read paths pass through
  assert client.post("/api/sessions/", json={}).status_code == 200
  assert refusal_reason("/api/sessions/", "POST") is None
  assert refusal_reason("/api/cron/tasks", "GET") is None


def test_preview_gate_closes_the_host_global_terminal_websocket() -> None:
  from fastapi.testclient import TestClient
  from starlette.websockets import WebSocketDisconnect

  client = TestClient(_gated_probe_app())
  with pytest.raises(WebSocketDisconnect):
    with client.websocket_connect("/ws/terminal"):
      pass


def test_preview_gate_leaves_ordinary_websockets_connected() -> None:
  from fastapi.testclient import TestClient

  client = TestClient(_gated_probe_app())
  with client.websocket_connect("/ws/sessions/x") as ws:
    assert ws.receive_text() == "hello x"


def test_preview_mode_flag() -> None:
  assert is_preview_mode() is False
  preview_module._activate_preview_mode()
  assert is_preview_mode() is True
  preview_module._preview_active = False


# ---------------------------------------------------------------------------
# The real CLI in a fresh process: refusals happen before any side effect
# ---------------------------------------------------------------------------


def _cli_env(source: Path, *, strip_launcher: bool = False) -> dict:
  env = {k: v for k, v in os.environ.items()}
  env["CHARLIEBOT_HOME"] = str(source)
  env["PYTHONUNBUFFERED"] = "1"
  if strip_launcher:
    env["PATH"] = "/usr/bin:/bin"
  return env


def _run_cli(args: list[str], source: Path, *, strip_launcher: bool = False) -> subprocess.CompletedProcess:
  return subprocess.run(
      [sys.executable, "-m", "src.cli.main", "session-tree", "preview", *args],
      cwd=str(REPO_ROOT), env=_cli_env(source, strip_launcher=strip_launcher),
      capture_output=True, text=True, timeout=120)


def _refusal(tmp_path: Path, source: Path, args: list[str], match: str, *,
             strip_launcher: bool = False) -> subprocess.CompletedProcess:
  proc = _run_cli(args, source, strip_launcher=strip_launcher)
  assert proc.returncode == 1, f"expected refusal, got {proc.returncode}: {proc.stdout} {proc.stderr}"
  diagnostic = proc.stderr.strip()
  assert match in diagnostic
  if diagnostic.startswith("{"):
    # The refusal diagnostic is one structured JSON document on stderr.
    assert match in json.dumps(json.loads(diagnostic))
  return proc


def test_cli_requires_home_and_port(source_home: Path) -> None:
  for args in ([], ["--home", "/tmp/x"], ["--port", "1234"]):
    proc = subprocess.run(
        [sys.executable, "-m", "src.cli.main", "session-tree", "preview", *args],
        cwd=str(REPO_ROOT), env=_cli_env(source_home), capture_output=True, text=True, timeout=60)
    assert proc.returncode == 2, (args, proc.stderr)


def test_cli_fresh_home_requires_backend(tmp_path: Path, source_home: Path) -> None:
  home = tmp_path / "trial"
  _refusal(tmp_path, source_home,
           ["--home", str(home), "--port", str(_free_port())], "requires --backend")
  assert not home.exists(), "a refused fresh start must not create the home"


def test_cli_refuses_unsafe_home_before_side_effects(tmp_path: Path, source_home: Path) -> None:
  port = _free_port()
  _refusal(tmp_path, source_home,
           ["--home", str(source_home / "nested"), "--port", str(port), "--backend", "clc-test"],
           "overlaps the production home")
  assert not (source_home / "nested").exists()
  _refusal(tmp_path, source_home,
           ["--home", str(source_home), "--port", str(port), "--backend", "clc-test"],
           "overlaps the production home")
  _refusal(tmp_path, source_home,
           ["--home", str(tmp_path / "trial"), "--port", "18498", "--backend", "clc-test"],
           "source profile's server port")
  with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    sock.listen(1)
    occupied = sock.getsockname()[1]
    _refusal(tmp_path, source_home,
             ["--home", str(tmp_path / "trial"), "--port", str(occupied), "--backend", "clc-test"],
             "not free")
  assert not (tmp_path / "trial").exists()


def test_cli_refuses_symlinked_home_resolving_into_production(tmp_path: Path, source_home: Path) -> None:
  link = tmp_path / "innocent-name"
  link.symlink_to(source_home / "nested-deeper")
  _refusal(tmp_path, source_home,
           ["--home", str(link), "--port", str(_free_port()), "--backend", "clc-test"],
           "overlaps the production home")
  assert not (source_home / "nested-deeper").exists()


def test_cli_refuses_unrelated_existing_config_untouched(tmp_path: Path, source_home: Path) -> None:
  unrelated = tmp_path / "unrelated"
  unrelated.mkdir()
  config = unrelated / "config.yaml"
  config.write_text("server: {host: 127.0.0.1, port: 18501}\n", encoding="utf-8")
  before = hashlib.sha256(config.read_bytes()).hexdigest()
  _refusal(tmp_path, source_home,
           ["--home", str(unrelated), "--port", str(_free_port()), "--backend", "clc-test"],
           "not a session-tree preview home")
  assert hashlib.sha256(config.read_bytes()).hexdigest() == before
  assert not (unrelated / "credentials.yaml").exists()


def test_cli_refuses_legacy_and_migrated_content(tmp_path: Path, source_home: Path) -> None:
  legacy = tmp_path / "legacy"
  (legacy / "sessions" / "abc").mkdir(parents=True)
  (legacy / "sessions" / "abc" / "metadata.json").write_text(json.dumps({"id": "abc"}))
  _refusal(tmp_path, source_home,
           ["--home", str(legacy), "--port", str(_free_port()), "--backend", "clc-test"],
           "not a session-tree preview home")
  assert (legacy / "sessions" / "abc" / "metadata.json").is_file()
  migrated = tmp_path / "migrated"
  migrated.mkdir()
  (migrated / "session_tree_migration.json").write_text("{}")
  _refusal(tmp_path, source_home,
           ["--home", str(migrated), "--port", str(_free_port()), "--backend", "clc-test"],
           "not a session-tree preview home")
  assert (migrated / "session_tree_migration.json").read_text() == "{}"


def test_cli_refuses_when_launcher_missing(tmp_path: Path, source_home: Path) -> None:
  _refusal(tmp_path, source_home,
           ["--home", str(tmp_path / "trial"), "--port", str(_free_port()), "--backend", "clc-test"],
           "launcher is not installed", strip_launcher=True)
  assert not (tmp_path / "trial").exists()


def test_cli_refuses_unisolated_backend_type(tmp_path: Path, source_home: Path) -> None:
  other = tmp_path / "source-cc"
  write_source_home(other, backend={"id": "cc", "label": "CC", "type": "cc-claude", "model": "m"})
  _refusal(tmp_path, other,
           ["--home", str(tmp_path / "trial"), "--port", str(_free_port()), "--backend", "cc"],
           "only for charlie-code")
  assert not (tmp_path / "trial").exists()


def test_cli_refuses_missing_provider_credential(tmp_path: Path, source_home: Path) -> None:
  cred_home = tmp_path / "source-cred"
  write_source_home(cred_home, backend={
      "id": "clc-cred", "label": "CLC", "type": "charlie-code", "model": "openai/fake",
      "api_base": "http://127.0.0.1:9/v1", "credential": "provider"})
  text = (cred_home / "credentials.yaml").read_text().replace(
      f"provider:\n  api_key: {PROVIDER_KEY}\n", "")
  (cred_home / "credentials.yaml").write_text(text, encoding="utf-8")
  _refusal(tmp_path, cred_home,
           ["--home", str(tmp_path / "trial"), "--port", str(_free_port()), "--backend", "clc-cred"],
           "credentials.provider.api_key")
  assert not (tmp_path / "trial").exists()


def test_cli_refuses_non_clc_addition_before_side_effects(tmp_path: Path, source_home: Path) -> None:
  other = tmp_path / "source-codex"
  _write_multi_source_home(other, entries=[
      {"id": "clc-test", "label": "CLC", "type": "charlie-code",
       "model": "openai/fake", "api_base": "http://127.0.0.1:9/v1"},
      {"id": "codex-entry", "label": "Codex", "type": "codex", "model": "m"},
  ])
  trial = tmp_path / "trial"
  _refusal(tmp_path, other,
           ["--home", str(trial), "--port", str(_free_port()), "--backend", "clc-test",
            "--add-backend", "codex-entry"],
           "only for charlie-code")
  assert not trial.exists()


def test_cli_refuses_addition_with_missing_credential_before_side_effects(
    tmp_path: Path, source_home: Path) -> None:
  other = tmp_path / "source-cred"
  _write_multi_source_home(other, entries=[
      {"id": "clc-test", "label": "CLC", "type": "charlie-code",
       "model": "openai/fake", "api_base": "http://127.0.0.1:9/v1"},
      {"id": "clc-cred", "label": "Cred", "type": "charlie-code",
       "model": "openai/m", "api_base": "http://127.0.0.1:9/v1", "credential": "missing-section"},
  ])
  trial = tmp_path / "trial"
  _refusal(tmp_path, other,
           ["--home", str(trial), "--port", str(_free_port()), "--backend", "clc-test",
            "--add-backend", "clc-cred"],
           "credentials.missing-section.api_key")
  assert not trial.exists()


def test_cli_refuses_duplicate_addition_of_the_requested_default(tmp_path: Path, source_home: Path) -> None:
  trial = tmp_path / "trial"
  _refusal(tmp_path, source_home,
           ["--home", str(trial), "--port", str(_free_port()), "--backend", "clc-test",
            "--add-backend", "clc-test"],
           "already part of this trial's backend selection")
  assert not trial.exists()


def test_cli_addition_to_a_live_holder_refuses_structured_and_untouched(
    tmp_path: Path, source_home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A live fence holder (the running preview instance) refuses additions before any change."""
  from src.core.home_writer_fence import acquire_home_writer_fence

  monkeypatch.setattr(preview_module, "check_ui_assets", lambda: None)
  home = _existing_preview_home(tmp_path)
  config_before = (home / "config.yaml").read_text()
  creds_before = (home / "credentials.yaml").read_text()
  other = tmp_path / "source-two"
  _write_multi_source_home(other, entries=[
      {"id": "clc-test", "label": "CLC", "type": "charlie-code",
       "model": "openai/fake", "api_base": "http://127.0.0.1:9/v1"},
      {"id": "clc-add", "label": "Add", "type": "charlie-code",
       "model": "openai/add", "api_base": "http://127.0.0.1:9/v1"},
  ])
  fence = acquire_home_writer_fence(home, purpose="running preview instance")
  try:
    proc = _run_cli(["--home", str(home), "--port", str(_free_port()),
                     "--add-backend", "clc-add"], other)
    assert proc.returncode == 1, proc.stdout + proc.stderr
    diagnostic = json.loads(proc.stderr.strip())
    assert "refused for home" in diagnostic["error"]
    assert "running preview instance" in diagnostic["error"]
    assert (home / "config.yaml").read_text() == config_before
    assert (home / "credentials.yaml").read_text() == creds_before
  finally:
    fence.release()


def test_cli_addition_refusals_leave_an_existing_home_byte_identical(
    tmp_path: Path, source_home: Path) -> None:
  home = _existing_preview_home(tmp_path)
  config_before = (home / "config.yaml").read_text()
  creds_before = (home / "credentials.yaml").read_text()
  other = tmp_path / "source-codex"
  _write_multi_source_home(other, entries=[
      {"id": "clc-test", "label": "CLC", "type": "charlie-code",
       "model": "openai/fake", "api_base": "http://127.0.0.1:9/v1"},
      {"id": "clc-new", "label": "New", "type": "charlie-code",
       "model": "openai/new", "api_base": "http://127.0.0.1:9/v1"},
      {"id": "codex-entry", "label": "Codex", "type": "codex", "model": "m"},
  ])
  _refusal(tmp_path, other,
           ["--home", str(home), "--port", str(_free_port()),
            "--add-backend", "clc-new", "--add-backend", "clc-new"],
           "requested more than once")
  _refusal(tmp_path, other,
           ["--home", str(home), "--port", str(_free_port()),
            "--add-backend", "no-such-backend"],
           "not configured in the source profile")
  _refusal(tmp_path, other,
           ["--home", str(home), "--port", str(_free_port()),
            "--add-backend", "clc-test"],
           "already part of this trial's backend selection")
  assert (home / "config.yaml").read_text() == config_before
  assert (home / "credentials.yaml").read_text() == creds_before


# ---------------------------------------------------------------------------
# Full behavioral case through the real CLI (needs the installed launcher)
# ---------------------------------------------------------------------------


def _wait_ready(home: Path, timeout: float = 90.0) -> dict:
  deadline = time.monotonic() + timeout
  record_path = preview_record_path(home)
  while time.monotonic() < deadline:
    if record_path.is_file():
      record = json.loads(record_path.read_text())
      if record.get("ready"):
        return record
    time.sleep(0.2)
  raise AssertionError(f"preview instance at {home} never became ready")


def _http(base: str, method: str, path: str, key: str | None,
          payload: dict | None = None) -> tuple[int, dict]:
  from urllib import error as urlerror
  from urllib import request as urlrequest

  headers = {}
  if key:
    headers["Authorization"] = f"Bearer {key}"
  body = None
  if payload is not None:
    body = json.dumps(payload).encode()
    headers["Content-Type"] = "application/json"
  req = urlrequest.Request(base + path, data=body, headers=headers, method=method)
  try:
    with urlrequest.urlopen(req, timeout=20) as resp:
      return resp.status, json.loads(resp.read().decode() or "{}")
  except urlerror.HTTPError as e:
    return e.code, json.loads(e.read().decode() or "{}")


def _snapshot_tree(root: Path) -> dict[str, str]:
  return {
      str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
      for p in sorted(root.rglob("*")) if p.is_file()
  }


@pytest.mark.local_only
def test_cli_fresh_start_two_instances_and_restart(tmp_path: Path) -> None:
  """The full fresh-start case: real server, real fence, real HTTP, restart, isolation proof."""
  source = tmp_path / "source-home"
  write_source_home(source)
  independent = tmp_path / "independent-service"
  write_source_home(independent)
  # The independent service is a live writer on its own home: it holds its own
  # fence, keeps sentinel files, and must receive nothing from the trial.
  from src.core.home_writer_fence import acquire_home_writer_fence, probe_writer_fence

  (independent / "clc-sessions").mkdir()
  (independent / "workspaces").mkdir()
  sentinel = independent / "state" / "independent_sentinel.json"
  sentinel.parent.mkdir(parents=True, exist_ok=True)
  sentinel.write_text('{"independent": true}')
  independent_fence = acquire_home_writer_fence(independent, purpose="independent service")
  try:
    independent_before = _snapshot_tree(independent)

    port = _free_port()
    home = tmp_path / "trial-home"
    env = _cli_env(source)
    proc = subprocess.Popen(
        [sys.executable, "-m", "src.cli.main", "session-tree", "preview",
         "--home", str(home), "--port", str(port), "--backend", "clc-test"],
        cwd=str(REPO_ROOT), env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    try:
      record = _wait_ready(home)
      base = f"http://127.0.0.1:{port}"
      assert record["home"] == str(home)
      assert record["port"] == port
      assert record["url"] == base
      assert record["backend"] == "clc-test"
      assert record["pid"] == proc.pid
      assert record["source_branch"]
      assert len(record["source_sha"]) == 40
      config = yaml.safe_load((home / "config.yaml").read_text())
      assert config["backends"]["options"][0]["id"] == "clc-test"
      assert config["paths"]["workspace_dirs"] == [str(home / "workspaces")]
      key = yaml.safe_load((home / "credentials.yaml").read_text())["charliebot"]["access_key"]
      assert key != OPERATOR_KEY
      assert key.startswith("preview-key-")
      assert (home / "logs").is_dir() and any((home / "logs").iterdir())
      holder = probe_writer_fence(home)
      assert holder["exclusive_holder_alive"] is True
      assert holder["identity_recorded"].pid == proc.pid

      # Second start on the owned home refuses with the holder's identity.
      refusal = _run_cli(["--home", str(home), "--port", str(_free_port())], source)
      assert refusal.returncode == 1
      assert str(proc.pid) in refusal.stderr

      # The task model works over the real API: create a root manager task.
      status, created = _http(base, "POST", "/api/sessions/", key, {
          "request_id": "preview-root-1", "profile": "manager",
          "name": "trial-root", "task": {"goal": "Try the task tree"}})
      assert status == 200, (status, created)
      root_id = created["id"]
      status, tree = _http(base, "GET", "/api/sessions/tree", key)
      assert status == 200
      assert [row["id"] for row in tree["items"]] == [root_id]

      # Disabled mechanisms refuse at the real HTTP boundary.
      status, body = _http(base, "POST", "/api/cron/tasks", key, {"name": "x"})
      assert status == 403 and "Scheduled task management is disabled" in body["detail"]
      status, body = _http(base, "POST", "/api/internal/schedule-trigger", key, {})
      assert status == 403 and "External messaging and delayed triggers are disabled" in body["detail"]
      status, body = _http(base, "POST", "/api/internal/slack/reply", key, {})
      assert status == 403
      # The API workspace boundary: the runtime checkout is not selectable.
      status, body = _http(base, "GET",
                           f"/api/git/diff/files?repo={REPO_ROOT}&base=main&head=main", key)
      assert status == 400 and "workspace_dirs" in json.dumps(body)

      # The independent instance's sentinel files are untouched while the
      # preview creates its own tasks; its native/workspace dirs stay empty.
      assert _snapshot_tree(independent) == independent_before
      assert list((independent / "clc-sessions").iterdir()) == []
      assert list((home / "clc-sessions").is_dir() for _ in [0]) == [True]

      # Graceful stop: the fence is released, the ready flag clears.
      proc.terminate()
      proc.wait(timeout=60)
      assert probe_writer_fence(home)["exclusive_holder_alive"] is False
      stopped = json.loads(preview_record_path(home).read_text())
      assert stopped["ready"] is False and "stopped_at" in stopped
    finally:
      if proc.poll() is None:
        proc.kill()
        proc.wait(timeout=30)

    # A reviewed restart preserves the task, the config and the key.
    key_before = yaml.safe_load((home / "credentials.yaml").read_text())["charliebot"]["access_key"]
    config_before = (home / "config.yaml").read_text()
    proc2 = subprocess.Popen(
        [sys.executable, "-m", "src.cli.main", "session-tree", "preview",
         "--home", str(home), "--port", str(port)],
        cwd=str(REPO_ROOT), env=_cli_env(source), stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, text=True)
    try:
      record2 = _wait_ready(home)
      assert record2["port"] == port
      key = yaml.safe_load((home / "credentials.yaml").read_text())["charliebot"]["access_key"]
      assert key == key_before
      assert (home / "config.yaml").read_text() == config_before
      status, tree = _http(f"http://127.0.0.1:{port}", "GET", "/api/sessions/tree", key)
      assert status == 200
      assert root_id in [row["id"] for row in tree["items"]]
      assert _snapshot_tree(independent) == independent_before
    finally:
      proc2.terminate()
      try:
        proc2.wait(timeout=60)
      except subprocess.TimeoutExpired:
        proc2.kill()
        proc2.wait(timeout=30)
    assert probe_writer_fence(home)["exclusive_holder_alive"] is False
    assert _snapshot_tree(independent) == independent_before
  finally:
    independent_fence.release()


@pytest.mark.local_only
def test_cli_adds_backends_to_existing_home_and_restarts_preserved(tmp_path: Path) -> None:
  """The real add flow: a one-model preview gains two entries without losing anything.

  Runs the actual CLI: a fresh one-model start, a refused addition while the
  instance holds the fence, the successful two-entry addition after a clean
  stop, and a no-flag restart that preserves the extended catalog.
  """
  from src.core.home_writer_fence import probe_writer_fence

  source = tmp_path / "source-home"
  _write_multi_source_home(source, entries=[
      {"id": "clc-default", "label": "CLC Default", "type": "charlie-code",
       "model": "openai/default-model", "api_base": "http://127.0.0.1:9/v1"},
      {"id": "clc-add-1", "label": "CLC Add 1", "type": "charlie-code",
       "model": "openai/add-1", "api_base": "http://127.0.0.1:9/v1", "top_p": 0.95},
      {"id": "clc-add-2", "label": "CLC Add 2", "type": "charlie-code",
       "model": "openai/add-2", "api_base": "http://127.0.0.1:9/v1", "credential": "provider"},
  ])
  port = _free_port()
  home = tmp_path / "trial-home"
  env = _cli_env(source)

  def start(*extra: str) -> subprocess.Popen:
    return subprocess.Popen(
        [sys.executable, "-m", "src.cli.main", "session-tree", "preview",
         "--home", str(home), "--port", str(port), *extra],
        cwd=str(REPO_ROOT), env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)

  def stop(proc: subprocess.Popen) -> None:
    proc.terminate()
    proc.wait(timeout=60)

  proc = start("--backend", "clc-default")
  try:
    record = _wait_ready(home)
    assert record["backend"] == "clc-default"
    key = yaml.safe_load((home / "credentials.yaml").read_text())["charliebot"]["access_key"]
    # One real task plus one native run dir: the things a later addition must keep.
    status, created = _http(record["url"], "POST", "/api/sessions/", key, {
        "request_id": "add-root-1", "profile": "manager",
        "name": "pre-add task", "task": {"goal": "before the catalog grew"}})
    assert status == 200, (status, created)
    root_id = created["id"]
    native = home / "clc-sessions"
    (native / "pre-add-native-dir").mkdir(exist_ok=True)
    config_before_add = (home / "config.yaml").read_text()

    # While the instance holds the fence, an addition refuses structurally and changes nothing.
    live = _run_cli(["--home", str(home), "--port", str(_free_port()),
                     "--add-backend", "clc-add-1"], source)
    assert live.returncode == 1
    assert "refused for home" in live.stderr
    assert (home / "config.yaml").read_text() == config_before_add
  finally:
    stop(proc)
  assert probe_writer_fence(home)["exclusive_holder_alive"] is False

  proc = start("--add-backend", "clc-add-1", "--add-backend", "clc-add-2")
  try:
    _wait_ready(home)
    data = yaml.safe_load((home / "config.yaml").read_text())
    assert [e["id"] for e in data["backends"]["options"]] == ["clc-default", "clc-add-1", "clc-add-2"]
    assert data["backends"]["preference"] == ["clc-default"]
    assert data["server"] == {"host": "127.0.0.1", "port": port}
    assert [e["id"] for e in yaml.safe_load(config_before_add)["backends"]["options"]] == ["clc-default"]
    creds = yaml.safe_load((home / "credentials.yaml").read_text())
    assert creds["charliebot"]["access_key"] == key, "the access key survives the addition"
    assert creds["provider"] == {"api_key": PROVIDER_KEY}
    assert (home / "sessions" / root_id / "metadata.json").is_file(), "the task survives"
    assert (home / "clc-sessions" / "pre-add-native-dir").is_dir(), "native history survives"
    status, tree = _http(f"http://127.0.0.1:{port}", "GET", "/api/sessions/tree", key)
    assert status == 200 and root_id in [row["id"] for row in tree["items"]]
    # An already-selected id refuses on a later run instead of re-adding (the
    # preparation reads the home's own catalog for the exclusion set).
    dup = _run_cli(["--home", str(home), "--port", str(_free_port()),
                    "--add-backend", "clc-add-1"], source)
    assert dup.returncode == 1 and "already part of this trial's backend selection" in dup.stderr
  finally:
    stop(proc)
  assert probe_writer_fence(home)["exclusive_holder_alive"] is False

  # A no-flag restart keeps the extended catalog byte-identically.
  config_before_restart = (home / "config.yaml").read_text()
  proc = start()
  try:
    _wait_ready(home)
    assert (home / "config.yaml").read_text() == config_before_restart
    creds = yaml.safe_load((home / "credentials.yaml").read_text())
    assert creds["charliebot"]["access_key"] == key
    status, tree = _http(f"http://127.0.0.1:{port}", "GET", "/api/sessions/tree", key)
    assert status == 200 and root_id in [row["id"] for row in tree["items"]]
  finally:
    stop(proc)
  assert probe_writer_fence(home)["exclusive_holder_alive"] is False
