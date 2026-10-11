"""Page parts: a package's static files, templates and page parts reach the pages through its line in PACKAGES.

A page package keeps them at fixed paths (``src.runtime.hooks.page_render``). The page reads the part files
at each render and the asset version walks the package static trees, so a fast-forward that adds a part
takes effect without a restart.
"""

import pathlib

import pytest
from fastapi.testclient import TestClient

import server
from src.app import registrations
from src.infra import constants
from src.runtime import templating
from src.runtime.hooks import page_render

FAKE_PACKAGES = ["src.features.alpha", "src.backends.beta", "src.features.gamma"]


@pytest.fixture
def fake_repo(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> pathlib.Path:
  """A repo root with web/templates and web/static, whose page packages are FAKE_PACKAGES."""
  (tmp_path / "web" / "templates").mkdir(parents=True)
  (tmp_path / "web" / "static" / "js").mkdir(parents=True)
  monkeypatch.setattr(constants, "REPO_ROOT", tmp_path)
  monkeypatch.setattr(page_render, "_PAGE_PACKAGES", list(FAKE_PACKAGES))
  monkeypatch.setattr(templating, "_templates_instance", None)
  monkeypatch.setattr(templating, "_ASSET_DIGEST_STATE", {"sig": (), "digests": {}, "digest": ""})
  return tmp_path


def write(path: pathlib.Path, text: str) -> None:
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(text, encoding="utf-8")


def test_every_registered_package_is_a_page_package_in_registration_order() -> None:
  assert page_render.page_packages() == registrations.PACKAGES


def test_a_slot_outside_the_page_slots_raises() -> None:
  with pytest.raises(ValueError, match="'header'"):
    templating.page_parts("header")


def test_page_parts_lists_the_slot_parts_on_disk_in_registration_order(fake_repo: pathlib.Path) -> None:
  write(fake_repo / "src/backends/beta/templates/beta/page_scripts.html", "")
  write(fake_repo / "src/features/alpha/templates/alpha/page_scripts.html", "")
  write(fake_repo / "src/features/gamma/templates/gamma/page_head.html", "")

  assert templating.page_parts("scripts") == ["alpha/page_scripts.html", "beta/page_scripts.html"]
  assert templating.page_parts("head") == ["gamma/page_head.html"]
  assert templating.page_parts("modals") == []


def test_a_page_includes_its_slot_parts_and_a_part_added_later_without_a_rebuild(fake_repo: pathlib.Path) -> None:
  write(fake_repo / "web/templates/page.html", "[{% for part in page_parts('head') %}{% include part %}{% endfor %}]")
  write(fake_repo / "src/features/alpha/templates/alpha/page_head.html", "A")
  page = templating.templates().get_template("page.html")

  first = page.render()
  write(fake_repo / "src/features/gamma/templates/gamma/page_head.html", "G")
  second = templating.templates().get_template("page.html").render()

  assert (first, second) == ("[A]", "[AG]")


def test_the_asset_version_covers_package_static_files_by_url_path(fake_repo: pathlib.Path) -> None:
  write(fake_repo / "web/static/js/core.js", "core")
  write(fake_repo / "src/features/alpha/static/alpha.js", "one")
  before = templating._asset_tree_digest()
  keys = set(templating._ASSET_DIGEST_STATE["digests"])

  write(fake_repo / "src/features/alpha/static/alpha.js", "two!")
  edited = templating._asset_tree_digest()
  write(fake_repo / "src/backends/beta/static/beta.css", "b")
  added = templating._asset_tree_digest()

  assert keys == {"js/core.js", "features/alpha/alpha.js"}
  assert len({before, edited, added}) == 3


def test_the_server_serves_a_package_static_file_under_its_url_path() -> None:
  path = constants.REPO_ROOT / "src/features/artifacts/static/artifact-comments.js"

  response = TestClient(server.app).get("/static/features/artifacts/artifact-comments.js")

  assert response.status_code == 200
  assert response.content == path.read_bytes()
