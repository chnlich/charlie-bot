"""Tests for artifact review-UI injection in the file server (src/api/files.py)."""

from pathlib import Path
from types import SimpleNamespace

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import files as files_api
from src.core import plan_diff

SCRIPT = "<script src=/static/js/artifact-comments.js></script>"


@pytest.fixture
def sessions_root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """Point the configured sessions root and access key at test values.

  files.py imports ``get_config`` by name, so the patch lands on the module.
  A real app carries the auth middleware and routes /files/ as public; these
  tests cover the file server alone, so the fixture must supply the access key
  the middleware would otherwise own.
  """
  monkeypatch.setattr(
      files_api, "get_config", lambda: SimpleNamespace(sessions_dir=tmp_path, charliebot_access_key="secret"))
  return tmp_path


def _build_client() -> TestClient:
  app = FastAPI()
  app.include_router(files_api.router, prefix="/files")
  return TestClient(app)


def _write(path: Path) -> Path:
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text("<html><body><h1>Plan</h1></body></html>")
  return path


# --- pure string-transform helper: structural invariants ---


def test_inject_inserts_exactly_one_before_body() -> None:
  html = "<html><body><p>plan body</p></body></html>"
  out = files_api._inject_artifact_ui(html, "S")
  assert out.count("artifact-comments.js") == 1
  # The closing tag is preserved and the script sits before it.
  assert out.count("</body>") == 1
  assert out.index("artifact-comments.js") < out.index("</body>")
  # The inline id assignment precedes the external script tag, so the id is set
  # before artifact-comments.js runs.
  assert out.index("window.__cbcServerSessionId=") < out.index(SCRIPT)


def test_inject_targets_the_last_body() -> None:
  html = "<html><body>first</body>\n<!-- stray -->\n</body></html>"
  assert html.count("</body>") == 2
  out = files_api._inject_artifact_ui(html, "S")
  first = out.index("</body>")
  last = out.rindex("</body>")
  script = out.index("artifact-comments.js")
  # Script lands between the first and the last </body>, i.e. before the LAST one.
  assert first < script < last


def test_inject_appends_when_no_body() -> None:
  html = "<html><div>no closing body tag here</div></html>"
  out = files_api._inject_artifact_ui(html, "S")
  assert out.count("artifact-comments.js") == 1
  assert "</body>" not in out
  # Original content is left intact and the tags are appended after it.
  assert out.startswith(html)
  assert out.rstrip().endswith(SCRIPT)


# --- route-level: inject vs not-inject decision, anchored on the sessions root ---


def test_serve_file_injects_for_artifact_path(sessions_root: Path) -> None:
  page = _write(sessions_root / "S" / "artifacts" / "x.html")

  resp = _build_client().get("/files" + str(page), cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 200
  assert resp.headers["content-type"].startswith("text/html")
  body = resp.text
  assert body.count(SCRIPT) == 1
  assert 'window.__cbcServerSessionId="S";' in body
  # The inline id assignment precedes the external script tag.
  assert body.index("window.__cbcServerSessionId=") < body.index(SCRIPT)
  assert body.index(SCRIPT) < body.index("</body>")
  # Injected pages are rendered (HTMLResponse), not served from disk (FileResponse),
  # so they carry no file-serving validators.
  assert "last-modified" not in resp.headers


def test_serve_file_injects_thread_artifact_with_session_id_not_thread_id(sessions_root: Path) -> None:
  page = _write(sessions_root / "S" / "threads" / "T" / "artifacts" / "x.html")

  resp = _build_client().get("/files" + str(page), cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 200
  assert 'window.__cbcServerSessionId="S";' in resp.text
  assert '"T"' not in resp.text


def test_serve_file_injects_deeper_nested_artifact(sessions_root: Path) -> None:
  # A depth the old path-shape regex never matched: the predicate only cares that the
  # page sits under <session>/... with an `artifacts` parent, not how deep.
  page = _write(sessions_root / "S" / "threads" / "T" / "sub" / "artifacts" / "x.html")

  resp = _build_client().get("/files" + str(page), cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 200
  assert 'window.__cbcServerSessionId="S";' in resp.text


def test_serve_file_injects_via_bearer_header(sessions_root: Path) -> None:
  page = _write(sessions_root / "S" / "artifacts" / "x.html")

  resp = _build_client().get("/files" + str(page), headers={"Authorization": "Bearer secret"})
  assert resp.status_code == 200
  assert "artifact-comments.js" in resp.text
  assert 'window.__cbcServerSessionId="S";' in resp.text


def test_serve_file_no_credential_returns_original_bytes(sessions_root: Path) -> None:
  page = _write(sessions_root / "S" / "artifacts" / "x.html")
  original = page.read_text(encoding="utf-8")

  resp = _build_client().get("/files" + str(page))
  assert resp.status_code == 200
  assert resp.text == original
  assert "artifact-comments.js" not in resp.text
  assert 'window.__cbcServerSessionId="S";' not in resp.text


def test_serve_file_wrong_credential_returns_original_bytes(sessions_root: Path) -> None:
  page = _write(sessions_root / "S" / "artifacts" / "x.html")
  original = page.read_text(encoding="utf-8")

  resp = _build_client().get("/files" + str(page), cookies={"charliebot_access_key": "wrong"})
  assert resp.status_code == 200
  assert resp.text == original
  assert "artifact-comments.js" not in resp.text


def test_serve_file_empty_configured_key_injects(sessions_root: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """No configured key = every reader authenticates, so injection stays on."""
  monkeypatch.setattr(
      files_api, "get_config", lambda: SimpleNamespace(sessions_dir=sessions_root, charliebot_access_key=""))
  page = _write(sessions_root / "S" / "artifacts" / "x.html")

  resp = _build_client().get("/files" + str(page))
  assert resp.status_code == 200
  assert 'window.__cbcServerSessionId="S";' in resp.text


def test_serve_file_session_html_outside_artifacts_not_injected(sessions_root: Path) -> None:
  page = _write(sessions_root / "S" / "notes" / "x.html")

  resp = _build_client().get("/files" + str(page), cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 200
  assert "artifact-comments.js" not in resp.text
  # Kept as a FileResponse: served from disk with a last-modified validator.
  assert "last-modified" in resp.headers


def test_serve_file_root_level_artifacts_dir_not_injected(sessions_root: Path) -> None:
  # <root>/artifacts/x.html belongs to no session — there is no session component.
  page = _write(sessions_root / "artifacts" / "x.html")

  resp = _build_client().get("/files" + str(page), cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 200
  assert "artifact-comments.js" not in resp.text
  assert "last-modified" in resp.headers


def test_serve_file_artifact_shape_outside_sessions_root_not_injected(
    sessions_root: Path,
    tmp_path_factory: pytest.TempPathFactory,
) -> None:
  # Same shape as a real artifact page but outside the configured sessions root. This
  # is the assertion that fails if anyone reintroduces a path-shape regex on the server.
  outside = tmp_path_factory.mktemp("outside")
  page = _write(outside / "a" / "sessions" / "S" / "artifacts" / "x.html")

  resp = _build_client().get("/files" + str(page), cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 200
  assert "artifact-comments.js" not in resp.text
  assert "last-modified" in resp.headers


# --- diff requests: ?diff=<base artifact path> serves the annotated page ---


def _write_pages(sessions_root: Path) -> tuple[Path, Path]:
  """A two-version artifact pair whose word-level difference plan_diff can mark."""
  base = sessions_root / "S" / "artifacts" / "plan_01.html"
  new = sessions_root / "S" / "artifacts" / "plan_02.html"
  base.parent.mkdir(parents=True, exist_ok=True)
  base.write_text("<html><body><p>hello world</p></body></html>", encoding="utf-8")
  new.write_text("<html><body><p>hello brave world</p></body></html>", encoding="utf-8")
  return base, new


def test_serve_file_diff_annotates_and_keeps_injection_layer(sessions_root: Path) -> None:
  base, new = _write_pages(sessions_root)

  resp = _build_client().get(
      "/files" + str(new) + "?diff=artifacts/plan_01.html", cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 200
  assert resp.headers["content-type"].startswith("text/html")
  # Byte-exact against composing the two layers the way the handler must:
  # plan_diff marks spliced into the new page, then the comment layer wrapped
  # around the annotated result exactly as it wraps a clean page.
  expected = files_api._inject_artifact_ui(
      plan_diff.annotate(base.read_text(encoding="utf-8"), new.read_text(encoding="utf-8")), "S")
  assert resp.text == expected
  assert "cbd-ins" in resp.text  # the word-level marks are actually present
  assert resp.text.count(SCRIPT) == 1
  assert 'window.__cbcServerSessionId="S";' in resp.text


def test_serve_file_diff_without_credential_does_not_fall_back_to_clean_page(sessions_root: Path) -> None:
  _, new = _write_pages(sessions_root)

  resp = _build_client().get("/files" + str(new) + "?diff=artifacts/plan_01.html")
  assert resp.status_code == 200
  assert "cbd-ins" in resp.text
  assert SCRIPT not in resp.text


def test_serve_file_without_diff_is_byte_identical_to_pre_diff_response(sessions_root: Path) -> None:
  page = _write(sessions_root / "S" / "artifacts" / "x.html")
  original = page.read_text(encoding="utf-8")

  resp = _build_client().get("/files" + str(page), cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 200
  # Exactly the bytes the handler produced before the diff feature existed:
  # the page wrapped in the artifact UI, with no diff machinery involved.
  assert resp.text == files_api._inject_artifact_ui(original, "S")


def test_serve_file_diff_missing_base_is_404_naming_the_path(sessions_root: Path) -> None:
  new = sessions_root / "S" / "artifacts" / "plan_02.html"
  _write(new)

  resp = _build_client().get(
      "/files" + str(new) + "?diff=artifacts/plan_01.html", cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 404
  assert "plan_01.html" in resp.text


def test_serve_file_diff_base_outside_session_artifacts_is_400(sessions_root: Path) -> None:
  _, new = _write_pages(sessions_root)
  _write(sessions_root / "S" / "notes" / "plan_01.html")

  resp = _build_client().get(
      "/files" + str(new) + "?diff=notes/plan_01.html", cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 400


def test_serve_file_diff_base_non_html_is_400(sessions_root: Path) -> None:
  _, new = _write_pages(sessions_root)
  (sessions_root / "S" / "artifacts" / "plan_01.txt").write_text("not html", encoding="utf-8")

  resp = _build_client().get(
      "/files" + str(new) + "?diff=artifacts/plan_01.txt", cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 400


def test_serve_file_diff_non_artifact_target_is_400(sessions_root: Path) -> None:
  _, new = _write_pages(sessions_root)
  notes_page = _write(sessions_root / "S" / "notes" / "page.html")
  (sessions_root / "S" / "artifacts" / "directory.html").mkdir()
  text_file = sessions_root / "S" / "artifacts" / "file.txt"
  text_file.write_text("plain", encoding="utf-8")

  resp = _build_client().get(
      "/files" + str(notes_page) + "?diff=artifacts/plan_02.html", cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 400
  resp = _build_client().get(
      "/files" + str(text_file) + "?diff=artifacts/plan_02.html", cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 400
  resp = _build_client().get(
      "/files" + str(sessions_root / "S" / "artifacts" / "directory.html") + "?diff=artifacts/plan_02.html",
      cookies={"charliebot_access_key": "secret"})
  assert resp.status_code == 400
