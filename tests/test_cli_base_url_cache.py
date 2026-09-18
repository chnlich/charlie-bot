"""The CLI base-url cache: the request path resolves the server port without config's model stack.

The plan/memory/etc. verbs run as fresh processes per master turn, and the request
contract's only config read on the happy path is ``server.port`` — so the port rides a
fingerprint-keyed document under the profile home (``cache/cli_base_url.json``), written
only by a full get_config() resolution. A hit answers without importing src.core.config
(~150 ms of the M97 wall); a moved fingerprint, a missing document, or an unreadable one
pays the full read and rewrites.
"""

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest
from conftest import ROOT

from src.core import credentials as core_credentials
from src.core.home import CHARLIEBOT_HOME_ENV, _home_cache


def _cache_path(home: Path) -> Path:
  return home / "cache" / "cli_base_url.json"


def _current_fingerprint() -> list[list[float]]:
  from src.cli.common import _config_module_fingerprint

  return [list(core_credentials._file_fingerprint("config.yaml")), list(_config_module_fingerprint())]


def _seed_cache(home: Path, port: int) -> None:
  doc = {"fingerprint": _current_fingerprint(), "port": port}
  path = _cache_path(home)
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(json.dumps(doc), encoding="utf-8")


def test_miss_resolves_through_full_config_and_writes_the_document(profile_home: Path) -> None:
  from src.cli import common

  assert _cache_path(profile_home).is_file() is False
  assert common._internal_base_url() == "http://localhost:18498"

  doc = json.loads(_cache_path(profile_home).read_text(encoding="utf-8"))
  assert doc["port"] == 18498
  assert doc["fingerprint"] == _current_fingerprint()


def test_hit_answers_from_the_document_without_config_loaded(profile_home: Path) -> None:
  _seed_cache(profile_home, port=18999)
  probe = (
      "import json, sys; "
      f"sys.path.insert(0, {str(ROOT)!r}); "
      "from src.cli import common; "
      "print(json.dumps([common._internal_base_url(), 'src.core.config' in sys.modules, "
      "'pydantic' in sys.modules]))")
  result = subprocess.run([sys.executable, "-c", probe], capture_output=True, text=True, timeout=120, check=True)
  url, config_loaded, pydantic_loaded = json.loads(result.stdout.strip().splitlines()[-1])
  assert url == "http://localhost:18999"
  assert config_loaded is False
  assert pydantic_loaded is False


def test_stale_document_reprices_through_the_full_read(profile_home: Path) -> None:
  from src.cli import common

  _seed_cache(profile_home, port=18999)
  (profile_home / "config.yaml").write_text("server:\n  port: 18500\n", encoding="utf-8")
  assert common._internal_base_url() == "http://localhost:18500"

  doc = json.loads(_cache_path(profile_home).read_text(encoding="utf-8"))
  assert doc["port"] == 18500
  assert doc["fingerprint"] == _current_fingerprint()


def test_unreadable_document_is_a_miss_and_gets_replaced(profile_home: Path) -> None:
  from src.cli import common

  _seed_cache(profile_home, port=18999)
  _cache_path(profile_home).write_text("{not json", encoding="utf-8")
  assert common._internal_base_url() == "http://localhost:18498"
  assert json.loads(_cache_path(profile_home).read_text(encoding="utf-8"))["port"] == 18498


def test_foreign_document_shape_is_a_miss(profile_home: Path) -> None:
  from src.cli import common

  _seed_cache(profile_home, port=18999)
  _cache_path(profile_home).write_text(json.dumps({"port": "not-an-int", "fingerprint": []}), encoding="utf-8")
  assert common._internal_base_url() == "http://localhost:18498"


def test_sessions_seam_matches_the_config_model(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.cli import common
  from src.core.config import get_config

  sessions = (tmp_path / "sessions").resolve()
  sessions.mkdir()
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(tmp_path))
  _home_cache.clear()
  try:
    assert common._sessions_dir() == sessions
    assert common._sessions_dir() == get_config().sessions_dir.resolve()
  finally:
    _home_cache.clear()


def test_credentials_module_owns_the_names_config_reexports() -> None:
  from src.core import config as core_config

  assert core_config.Credentials is core_credentials.Credentials
  assert core_config.get_credentials is core_credentials.get_credentials
  assert core_config._credentials_cache is core_credentials._credentials_cache
  assert core_config.configured_access_key is core_credentials.configured_access_key


def test_config_module_fingerprint_tracks_config_py() -> None:
  from src.cli.common import _config_module_fingerprint

  config_py = Path(ROOT) / "src" / "core" / "config.py"
  st = os.stat(config_py)
  assert _config_module_fingerprint() == (st.st_mtime, st.st_size)


def test_get_credentials_keeps_its_hot_reload_contract(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(tmp_path))
  _home_cache.clear()
  try:
    cred_path = tmp_path / "credentials.yaml"
    cred_path.write_text("alpha:\n  key: one\n", encoding="utf-8")
    first = core_credentials.get_credentials()
    assert first.get("alpha", "key") == "one"
    assert core_credentials.get_credentials() is first
    cred_path.write_text("alpha:\n  key: two\n", encoding="utf-8")
    st = os.stat(cred_path)
    os.utime(cred_path, (st.st_atime, st.st_mtime + 10))
    second = core_credentials.get_credentials()
    assert second is not first
    assert second.get("alpha", "key") == "two"
  finally:
    _home_cache.clear()
