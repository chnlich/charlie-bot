"""Tests for src/cli/publish.py — URL on stdout, overwrite note, preflight failure exit codes."""

import json
from pathlib import Path
from unittest.mock import patch

import pytest
from conftest import CONFIG_GET_CONFIG_PATCH_TARGET, PUBLISH_BASE_URL, build_publish_cfg, write_artifact

from src.cli.publish import main


def test_publish_prints_the_url_on_stdout_and_exits_zero(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = write_artifact(tmp_path)

  with patch("sys.argv", ["publish", str(artifact)]), patch(CONFIG_GET_CONFIG_PATCH_TARGET,
                                                            return_value=build_publish_cfg(tmp_path)):
    main()

  out = capsys.readouterr()
  assert out.out == PUBLISH_BASE_URL + "/page.html\n"
  assert out.err == ""
  assert (tmp_path / "publish" / "page.html").read_text(encoding="utf-8") == "<p>hello</p>"


def test_missing_artifact_exits_non_zero_naming_the_path(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  absent = tmp_path / "gone.html"

  with (
      patch("sys.argv", ["publish", str(absent)]),
      patch(CONFIG_GET_CONFIG_PATCH_TARGET, return_value=build_publish_cfg(tmp_path)),
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert exc_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert str(absent) in json.loads(captured.err)["error"]
