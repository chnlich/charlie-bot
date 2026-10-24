"""Tests for src/features/artifacts/publish_cli.py — URL on stdout, failure exit codes, lane-option forwarding."""

import json
import pathlib
import re
from unittest import mock

import conftest
import pytest
import yaml

from src.features.artifacts import publish_cli as publish
from src.infra import config


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


def test_arguments_after_double_dash_reach_the_program_unchanged(
    tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = conftest.write_artifact(tmp_path)
  command = conftest.write_publish_program(tmp_path, conftest.PUBLISH_RECORD_ARGV_PROGRAM)
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home", publish={"command": command})

  argv = ["publish", str(artifact), "--", "--reader", "reader@example.com", "--", "--group", "staff"]
  with (
      mock.patch("sys.argv", argv),
      mock.patch(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    publish.main()

  out = capsys.readouterr()
  assert out.out == f"{conftest.PUBLISH_READER_URL}\n"
  assert out.err == ""
  received = json.loads((tmp_path / "argv.json").read_text(encoding="utf-8"))
  assert received == [str(artifact), "--reader", "reader@example.com", "--", "--group", "staff"]


def test_command_lane_failure_exits_one_with_the_programs_stderr_and_no_url(
    tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = conftest.write_artifact(tmp_path)
  body = (
      "import sys\n"
      f"print({conftest.PUBLISH_READER_URL!r})\n"
      "print('reader list is locked', file=sys.stderr)\n"
      "sys.exit(4)\n")
  command = conftest.write_publish_program(tmp_path, body)
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home", publish={"command": command})

  with (
      mock.patch("sys.argv", ["publish", str(artifact)]),
      mock.patch(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, return_value=cfg),
      pytest.raises(SystemExit) as exc_info,
  ):
    publish.main()

  assert exc_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert "reader list is locked" in json.loads(captured.err)["error"]


def test_home_without_a_publish_section_exits_one_naming_publish_dir(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  home = tmp_path / "home"
  home.mkdir()
  (home / "config.yaml").write_text("server:\n  port: 2001\n", encoding="utf-8")
  monkeypatch.setenv(config.CHARLIEBOT_HOME_ENV, str(home))
  cfg = config.load_config()
  artifact = conftest.write_artifact(tmp_path)

  with (
      mock.patch("sys.argv", ["publish", str(artifact)]),
      mock.patch(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, return_value=cfg),
      pytest.raises(SystemExit) as exc_info,
  ):
    publish.main()

  assert exc_info.value.code == 1
  assert json.loads(capsys.readouterr().err) == {
      "error": "publish.dir is not configured; the publish lane is unavailable"
  }


def test_directory_lane_from_a_yaml_config_without_command_publishes_as_before(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = conftest.write_artifact(tmp_path)
  lane_dir = conftest.deploy_publish_lane(tmp_path)
  home = tmp_path / "home"
  home.mkdir()
  section = {"dir": str(lane_dir), "public_base_url": conftest.PUBLISH_BASE_URL}
  (home / "config.yaml").write_text(yaml.safe_dump({"publish": section}), encoding="utf-8")
  monkeypatch.setenv(config.CHARLIEBOT_HOME_ENV, str(home))

  with (
      mock.patch("sys.argv", ["publish", str(artifact)]),
      mock.patch(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, return_value=config.load_config()),
  ):
    publish.main()

  out = capsys.readouterr()
  match = re.fullmatch(re.escape(conftest.PUBLISH_BASE_URL) + r"/([A-Za-z0-9_-]{22})/page\.html\n", out.out)
  assert match is not None, out.out
  assert out.err == ""
  assert (lane_dir / match.group(1) / "page.html").read_text(encoding="utf-8") == "<p>hello</p>"
