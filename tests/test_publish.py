"""Tests for the publish action (src.core.publish): preflight, the token-directory copy, and the URL join."""

import os
import pathlib
import re

import conftest
import pytest

from src.core import config, publish

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
