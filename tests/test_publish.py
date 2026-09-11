"""Tests for the publish action (src.core.publish): preflight, the copy, and the URL join."""

from pathlib import Path

import pytest
from conftest import PUBLISH_BASE_URL, build_publish_cfg, write_artifact

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


def test_publish_sets_mode_0644(tmp_path: Path) -> None:
  artifact = write_artifact(tmp_path)
  artifact.chmod(0o600)

  result = publish_artifact(artifact, build_publish_cfg(tmp_path))

  assert (result.path.stat().st_mode & 0o777) == 0o644


def test_publish_of_a_differing_same_name_file_reports_the_overwrite(tmp_path: Path) -> None:
  artifact = write_artifact(tmp_path)
  cfg = build_publish_cfg(tmp_path)
  replaced = cfg.publish.dir / "page.html"
  replaced.write_text("<p>old page</p>", encoding="utf-8")

  result = publish_artifact(artifact, cfg)

  assert result.overwrote is True
  assert replaced.read_text(encoding="utf-8") == "<p>hello</p>"


def test_publish_over_an_identical_file_reports_no_overwrite(tmp_path: Path) -> None:
  artifact = write_artifact(tmp_path)
  cfg = build_publish_cfg(tmp_path)
  (cfg.publish.dir / "page.html").write_text("<p>hello</p>", encoding="utf-8")

  result = publish_artifact(artifact, cfg)

  assert result.overwrote is False


def test_publish_twice_reports_the_overwrite_only_when_the_content_changed(tmp_path: Path) -> None:
  artifact = write_artifact(tmp_path)
  cfg = build_publish_cfg(tmp_path)

  assert publish_artifact(artifact, cfg).overwrote is False
  assert publish_artifact(artifact, cfg).overwrote is False
  artifact.write_text("<p>updated</p>", encoding="utf-8")
  assert publish_artifact(artifact, cfg).overwrote is True


def test_missing_artifact_raises_naming_the_path(tmp_path: Path) -> None:
  absent = tmp_path / "artifacts" / "gone.html"

  with pytest.raises(PublishError) as exc_info:
    publish_artifact(absent, build_publish_cfg(tmp_path))

  assert str(absent) in str(exc_info.value)


@pytest.mark.parametrize("missing_key", ["dir", "public_base_url"])
def test_missing_publish_key_raises_naming_the_key(tmp_path: Path, missing_key: str) -> None:
  artifact = write_artifact(tmp_path)
  section: dict[str, str | Path] = {"dir": tmp_path / "publish", "public_base_url": PUBLISH_BASE_URL}
  del section[missing_key]
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", publish=section)

  with pytest.raises(PublishError) as exc_info:
    publish_artifact(artifact, cfg)

  assert f"publish.{missing_key}" in str(exc_info.value)


def test_absent_publish_directory_raises_naming_the_directory(tmp_path: Path) -> None:
  """The copy refuses rather than producing links the undeployed 443 lane cannot serve."""
  artifact = write_artifact(tmp_path)
  absent_dir = tmp_path / "undeployed"
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home", publish={
          "dir": absent_dir,
          "public_base_url": PUBLISH_BASE_URL
      })

  with pytest.raises(PublishError) as exc_info:
    publish_artifact(artifact, cfg)

  assert str(absent_dir) in str(exc_info.value)


def test_config_expands_tilde_in_publish_dir_like_the_other_path_fields() -> None:
  cfg = CharlieBotConfig(publish={"dir": "~/publish"})

  assert cfg.publish.dir == Path.home() / "publish"
