"""Tests for the publish action (src.core.publish): preflight, the copy, and the URL join."""

from pathlib import Path

import pytest
from conftest import write_artifact

from src.core.config import CharlieBotConfig
from src.core.publish import PublishError, publish_artifact


def make_cfg(
    tmp_path: Path, *, publish_dir: Path | None = None, public_base_url: str | None = None) -> CharlieBotConfig:
  """A config whose publish lane points inside tmp_path, with the directory present the way the
  host's deployment step leaves it; each argument overridable to test preflight."""
  resolved_dir = publish_dir if publish_dir is not None else tmp_path / "publish"
  resolved_dir.mkdir(parents=True, exist_ok=True)
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      publish_dir=resolved_dir,
      public_base_url=public_base_url if public_base_url is not None else "https://pub.example.test/charliebot_pub",
  )


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
  cfg = make_cfg(tmp_path, public_base_url=base)

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

  result = publish_artifact(artifact, make_cfg(tmp_path))

  assert (result.path.stat().st_mode & 0o777) == 0o644


def test_publish_of_a_differing_same_name_file_reports_the_overwrite(tmp_path: Path) -> None:
  artifact = write_artifact(tmp_path)
  cfg = make_cfg(tmp_path)
  replaced = cfg.publish_dir / "page.html"
  replaced.write_text("<p>old page</p>", encoding="utf-8")

  result = publish_artifact(artifact, cfg)

  assert result.overwrote is True
  assert replaced.read_text(encoding="utf-8") == "<p>hello</p>"


def test_publish_over_an_identical_file_reports_no_overwrite(tmp_path: Path) -> None:
  artifact = write_artifact(tmp_path)
  cfg = make_cfg(tmp_path)
  (cfg.publish_dir / "page.html").write_text("<p>hello</p>", encoding="utf-8")

  result = publish_artifact(artifact, cfg)

  assert result.overwrote is False


def test_publish_twice_reports_the_overwrite_only_when_the_content_changed(tmp_path: Path) -> None:
  artifact = write_artifact(tmp_path)
  cfg = make_cfg(tmp_path)

  assert publish_artifact(artifact, cfg).overwrote is False
  assert publish_artifact(artifact, cfg).overwrote is False
  artifact.write_text("<p>updated</p>", encoding="utf-8")
  assert publish_artifact(artifact, cfg).overwrote is True


def test_missing_artifact_raises_naming_the_path(tmp_path: Path) -> None:
  absent = tmp_path / "artifacts" / "gone.html"

  with pytest.raises(PublishError) as exc_info:
    publish_artifact(absent, make_cfg(tmp_path))

  assert str(absent) in str(exc_info.value)


def test_missing_publish_dir_key_raises_naming_the_key(tmp_path: Path) -> None:
  artifact = write_artifact(tmp_path)
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", public_base_url="https://pub.example.test/charliebot_pub")

  with pytest.raises(PublishError) as exc_info:
    publish_artifact(artifact, cfg)

  assert "publish_dir" in str(exc_info.value)


def test_absent_publish_directory_raises_naming_the_directory(tmp_path: Path) -> None:
  """The copy refuses rather than producing links the undeployed 443 lane cannot serve."""
  artifact = write_artifact(tmp_path)
  absent_dir = tmp_path / "undeployed"
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      publish_dir=absent_dir,
      public_base_url="https://pub.example.test/charliebot_pub")

  with pytest.raises(PublishError) as exc_info:
    publish_artifact(artifact, cfg)

  assert str(absent_dir) in str(exc_info.value)


def test_missing_public_base_url_key_raises_naming_the_key(tmp_path: Path) -> None:
  artifact = write_artifact(tmp_path)
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", publish_dir=tmp_path / "publish")

  with pytest.raises(PublishError) as exc_info:
    publish_artifact(artifact, cfg)

  assert "public_base_url" in str(exc_info.value)


def test_config_expands_tilde_in_publish_dir_like_the_other_path_fields() -> None:
  cfg = CharlieBotConfig(publish_dir="~/publish")

  assert cfg.publish_dir == Path.home() / "publish"
