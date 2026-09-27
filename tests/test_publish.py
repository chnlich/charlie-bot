"""Tests for the publish action (src.core.publish): preflight, the copy, and the URL join."""

from pathlib import Path

import pytest
from conftest import build_publish_cfg, write_artifact

from src.core.config import CharlieBotConfig
from src.core.publish import PublishError, publish_artifact


@pytest.mark.parametrize(
    ("base", "expected_url"),
    [
        ("https://pub.example.test/charliebot_pub", "https://pub.example.test/charliebot_pub/page.html"),
        ("https://pub.example.test/charliebot_pub/", "https://pub.example.test/charliebot_pub/page.html"),
        ("https://pub.example.test", "https://pub.example.test/page.html"),
    ],
)
def test_publish_copies_and_joins_the_url_with_a_single_slash(tmp_path: Path, base: str, expected_url: str) -> None:
  artifact = write_artifact(tmp_path)
  # The publish lane deployed the way the host's deployment step leaves it, with this
  # row's base URL (build_publish_cfg pins the shared default; the sectioned pair
  # carries the per-case override).
  lane_dir = tmp_path / "publish"
  lane_dir.mkdir(parents=True, exist_ok=True)
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", publish={"dir": lane_dir, "public_base_url": base})

  result = publish_artifact(artifact, cfg)

  assert isinstance(result, str)
  assert result == expected_url
  assert result.url == expected_url
  published = tmp_path / "publish" / "page.html"
  assert result.path == published
  assert published.read_text(encoding="utf-8") == "<p>hello</p>"


def test_missing_artifact_raises_naming_the_path(tmp_path: Path) -> None:
  absent = tmp_path / "artifacts" / "gone.html"

  with pytest.raises(PublishError) as exc_info:
    publish_artifact(absent, build_publish_cfg(tmp_path))

  assert str(absent) in str(exc_info.value)
