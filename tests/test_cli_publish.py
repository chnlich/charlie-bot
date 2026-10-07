"""Tests for src/features/artifacts/publish_cli.py — URL on stdout, preflight failure exit codes."""

import json
import pathlib
import re
from unittest import mock

import conftest
import pytest

from src.features.artifacts import publish_cli as publish


def test_publish_prints_the_url_on_stdout_and_exits_zero(
    tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = conftest.write_artifact(tmp_path)

  with mock.patch("sys.argv",
                  ["publish", str(artifact)]), mock.patch(conftest.CONFIG_GET_CONFIG_PATCH_TARGET,
                                                          return_value=conftest.build_publish_cfg(tmp_path)):
    publish.main()

  out = capsys.readouterr()
  match = re.fullmatch(re.escape(conftest.PUBLISH_BASE_URL) + r"/([A-Za-z0-9_-]{22})/page\.html\n", out.out)
  assert match is not None, out.out
  assert out.err == ""
  assert (tmp_path / "publish" / match.group(1) / "page.html").read_text(encoding="utf-8") == "<p>hello</p>"


def test_missing_artifact_exits_non_zero_naming_the_path(
    tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  absent = tmp_path / "gone.html"

  with (
      mock.patch("sys.argv", ["publish", str(absent)]),
      mock.patch(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, return_value=conftest.build_publish_cfg(tmp_path)),
      pytest.raises(SystemExit) as exc_info,
  ):
    publish.main()

  assert exc_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert str(absent) in json.loads(captured.err)["error"]
