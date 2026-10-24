"""Tests for the publish action (src.features.artifacts.publish): both lanes' preflight, copy and URL."""

import json
import os
import pathlib
import re

import conftest
import pytest

from src.features.artifacts import publish
from src.infra import config

# The published URL for page.html: secrets.token_urlsafe(16) is 16 random bytes,
# base64url without padding, so 22 URL-safe characters.
_URL_RE = re.compile(r"https://pub\.example\.test/charliebot_pub/([A-Za-z0-9_-]{22})/page\.html")


@pytest.mark.parametrize(
    "base", [
        "https://pub.example.test/charliebot_pub",
        "https://pub.example.test/charliebot_pub/",
        "https://pub.example.test/charliebot_pub//",
    ])
def test_publish_twice_lands_two_token_copies_with_fixed_modes(tmp_path: pathlib.Path, base: str) -> None:
  artifact = conftest.write_artifact(tmp_path)
  lane_dir = conftest.deploy_publish_lane(tmp_path)
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home", publish={"dir": lane_dir, "public_base_url": base})
  # A restrictive umask would leave the copies unreadable to the host's static
  # server unless the modes are set explicitly.
  old_umask = os.umask(0o077)
  try:
    first = publish.publish_artifact(artifact, cfg)
    second = publish.publish_artifact(artifact, cfg)
  finally:
    os.umask(old_umask)

  tokens = []
  for result in (first, second):
    match = _URL_RE.fullmatch(result.url)
    assert match is not None, result.url
    token = match.group(1)
    assert result.path == lane_dir / token / "page.html"
    assert result.path.read_text(encoding="utf-8") == "<p>hello</p>"
    assert (lane_dir / token).stat().st_mode & 0o777 == 0o755
    assert result.path.stat().st_mode & 0o777 == 0o644
    tokens.append(token)
  assert tokens[0] != tokens[1]


def test_publish_without_index_html_refuses_and_writes_nothing(tmp_path: pathlib.Path) -> None:
  artifact = conftest.write_artifact(tmp_path)
  cfg = conftest.build_publish_cfg(tmp_path)
  (cfg.publish.dir / "index.html").unlink()

  with pytest.raises(publish.PublishError) as exc_info:
    publish.publish_artifact(artifact, cfg)

  assert str(cfg.publish.dir / "index.html") in str(exc_info.value)
  assert list(cfg.publish.dir.iterdir()) == []


def test_missing_artifact_raises_naming_the_path(tmp_path: pathlib.Path) -> None:
  absent = tmp_path / "artifacts" / "gone.html"

  with pytest.raises(publish.PublishError) as exc_info:
    publish.publish_artifact(absent, conftest.build_publish_cfg(tmp_path))

  assert str(absent) in str(exc_info.value)


def test_directory_lane_preflight_messages_stay_word_for_word(tmp_path: pathlib.Path) -> None:
  artifact = conftest.write_artifact(tmp_path)
  home = tmp_path / "home"
  no_index = tmp_path / "no-index"
  no_index.mkdir()
  missing = tmp_path / "missing"
  base = {"public_base_url": conftest.PUBLISH_BASE_URL}
  index_message = "publish directory has no index.html, so its listing would expose every link: "
  cases = [
      ({}, "publish.dir is not configured; the publish lane is unavailable"),
      ({
          "dir": no_index
      }, "publish.public_base_url is not configured; the publish lane is unavailable"),
      ({
          "dir": missing,
          **base
      }, f"publish directory does not exist: {missing}"),
      ({
          "dir": no_index,
          **base
      }, index_message + str(no_index / "index.html")),
  ]
  for section, message in cases:
    with pytest.raises(publish.PublishError) as exc_info:
      publish.publish_artifact(artifact, config.CharlieBotConfig(charliebot_home=home, publish=section))
    assert str(exc_info.value) == message

  absent = tmp_path / "artifacts" / "gone.html"
  with pytest.raises(publish.PublishError) as exc_info:
    publish.publish_artifact(absent, conftest.build_publish_cfg(tmp_path))
  assert str(exc_info.value) == f"artifact is not an existing regular file: {absent}"


def _command_cfg(tmp_path: pathlib.Path, command: list[str]) -> config.CharlieBotConfig:
  return config.CharlieBotConfig(charliebot_home=tmp_path / "home", publish={"command": command})


@pytest.mark.parametrize(
    "url", [conftest.PUBLISH_READER_URL,
            conftest.PUBLISH_READER_URL.replace("https://", "http://")],
    ids=["https", "http"])
def test_command_lane_returns_the_url_the_program_prints(tmp_path: pathlib.Path, url: str) -> None:
  artifact = conftest.write_artifact(tmp_path)
  command = conftest.write_publish_program(tmp_path, f"print({url!r})\n")

  result = publish.publish_artifact(artifact, _command_cfg(tmp_path, command))

  assert result == url
  assert result.path is None


def test_command_lane_runs_the_command_on_the_absolute_artifact_path_then_the_lane_options(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  artifact = conftest.write_artifact(tmp_path)
  command = [*conftest.write_publish_program(tmp_path, conftest.PUBLISH_RECORD_ARGV_PROGRAM), "--site", "main"]
  monkeypatch.chdir(tmp_path)

  result = publish.publish_artifact(
      "artifacts/page.html", _command_cfg(tmp_path, command), ["--reader", "reader@example.com"])

  assert result == conftest.PUBLISH_READER_URL
  received = json.loads((tmp_path / "argv.json").read_text(encoding="utf-8"))
  assert received == ["--site", "main", str(artifact), "--reader", "reader@example.com"]


def test_command_lane_nonzero_exit_raises_with_the_exit_code_and_stderr(tmp_path: pathlib.Path) -> None:
  artifact = conftest.write_artifact(tmp_path)
  body = (
      "import sys\n"
      f"print({conftest.PUBLISH_READER_URL!r})\n"
      "print('reader list is locked', file=sys.stderr)\n"
      "sys.exit(4)\n")

  with pytest.raises(publish.PublishError) as exc_info:
    publish.publish_artifact(artifact, _command_cfg(tmp_path, conftest.write_publish_program(tmp_path, body)))

  message = str(exc_info.value)
  assert "exited 4" in message
  assert "reader list is locked" in message
  assert conftest.PUBLISH_READER_URL not in message


@pytest.mark.parametrize(
    "stdout",
    ["", f"{conftest.PUBLISH_READER_URL}\n{conftest.PUBLISH_READER_URL}\n", "done\n"],
    ids=["empty", "two-lines", "no-url"],
)
def test_command_lane_refuses_stdout_that_is_not_exactly_one_url(tmp_path: pathlib.Path, stdout: str) -> None:
  artifact = conftest.write_artifact(tmp_path)
  command = conftest.write_publish_program(tmp_path, f"import sys\nsys.stdout.write({stdout!r})\n")

  with pytest.raises(publish.PublishError) as exc_info:
    publish.publish_artifact(artifact, _command_cfg(tmp_path, command))

  assert "exited 0" in str(exc_info.value)


def test_command_lane_names_a_program_that_cannot_start(tmp_path: pathlib.Path) -> None:
  artifact = conftest.write_artifact(tmp_path)
  missing = tmp_path / "no-such-program"

  with pytest.raises(publish.PublishError) as exc_info:
    publish.publish_artifact(artifact, _command_cfg(tmp_path, [str(missing)]))

  assert str(missing) in str(exc_info.value)


def test_command_lane_checks_the_artifact_before_running_the_program(tmp_path: pathlib.Path) -> None:
  absent = tmp_path / "artifacts" / "gone.html"
  ran = tmp_path / "ran"
  command = conftest.write_publish_program(tmp_path, f"import pathlib\npathlib.Path({str(ran)!r}).touch()\n")

  with pytest.raises(publish.PublishError) as exc_info:
    publish.publish_artifact(absent, _command_cfg(tmp_path, command))

  assert str(absent) in str(exc_info.value)
  assert not ran.exists()


@pytest.mark.parametrize(
    "lane_key, lane_value",
    [("dir", "/srv/charliebot_pub"), ("public_base_url", "https://pub.example.com/charliebot_pub")],
)
def test_command_with_a_directory_lane_key_is_a_config_error(lane_key: str, lane_value: str) -> None:
  with pytest.raises(ValueError) as exc_info:
    config.CharlieBotConfig(publish={"command": ["publish-program"], lane_key: lane_value})

  message = str(exc_info.value)
  assert "publish.command" in message
  assert f"publish.{lane_key}" in message


def test_empty_command_list_is_a_config_error() -> None:
  with pytest.raises(ValueError) as exc_info:
    config.CharlieBotConfig(publish={"command": []})

  assert "publish.command is an empty list" in str(exc_info.value)


def test_lane_options_on_the_directory_lane_are_refused_before_any_write(tmp_path: pathlib.Path) -> None:
  artifact = conftest.write_artifact(tmp_path)
  cfg = conftest.build_publish_cfg(tmp_path)

  with pytest.raises(publish.PublishError) as exc_info:
    publish.publish_artifact(artifact, cfg, ["--reader", "reader@example.com"])

  assert "publish.command" in str(exc_info.value)
  assert list(cfg.publish.dir.iterdir()) == [cfg.publish.dir / "index.html"]
