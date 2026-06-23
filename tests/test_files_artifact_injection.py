"""Tests for artifact review-UI injection in the file server (src/api/files.py)."""

from pathlib import Path

from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import files as files_api

SCRIPT = "<script src=/static/js/artifact-comments.js></script>"


def _build_client() -> TestClient:
  app = FastAPI()
  app.include_router(files_api.router, prefix="/files")
  return TestClient(app)


# --- pure string-transform helper: structural invariants ---


def test_inject_inserts_exactly_one_before_body() -> None:
  html = "<html><body><p>plan body</p></body></html>"
  out = files_api._inject_artifact_ui(html)
  assert out.count("artifact-comments.js") == 1
  # The closing tag is preserved and the script sits before it.
  assert out.count("</body>") == 1
  assert out.index("artifact-comments.js") < out.index("</body>")


def test_inject_targets_the_last_body() -> None:
  html = "<html><body>first</body>\n<!-- stray -->\n</body></html>"
  assert html.count("</body>") == 2
  out = files_api._inject_artifact_ui(html)
  first = out.index("</body>")
  last = out.rindex("</body>")
  script = out.index("artifact-comments.js")
  # Script lands between the first and the last </body>, i.e. before the LAST one.
  assert first < script < last


def test_inject_appends_when_no_body() -> None:
  html = "<html><div>no closing body tag here</div></html>"
  out = files_api._inject_artifact_ui(html)
  assert out.count("artifact-comments.js") == 1
  assert "</body>" not in out
  # Original content is left intact and the script is appended after it.
  assert out.startswith(html)
  assert out.rstrip().endswith(SCRIPT)


# --- route-level: match vs not-match decision ---


def test_serve_file_injects_for_artifact_path(tmp_path: Path) -> None:
  art_dir = tmp_path / "sessions" / "abc" / "artifacts"
  art_dir.mkdir(parents=True)
  page = art_dir / "plan_07.html"
  page.write_text("<html><body><h1>Plan</h1></body></html>")

  resp = _build_client().get("/files" + str(page))
  assert resp.status_code == 200
  assert resp.headers["content-type"].startswith("text/html")
  body = resp.text
  assert body.count("artifact-comments.js") == 1
  assert body.index("artifact-comments.js") < body.index("</body>")
  # Injected pages are rendered (HTMLResponse), not served from disk (FileResponse),
  # so they carry no file-serving validators.
  assert "last-modified" not in resp.headers


def test_serve_file_session_path_outside_artifacts_not_injected(tmp_path: Path) -> None:
  notes_dir = tmp_path / "sessions" / "abc" / "notes"
  notes_dir.mkdir(parents=True)
  page = notes_dir / "memo.html"
  original = "<html><body>not an artifact</body></html>"
  page.write_text(original)

  resp = _build_client().get("/files" + str(page))
  assert resp.status_code == 200
  assert "artifact-comments.js" not in resp.text
  assert resp.text == original
  # Kept as a FileResponse: served from disk with a last-modified validator.
  assert "last-modified" in resp.headers


def test_serve_file_plain_html_not_injected(tmp_path: Path) -> None:
  page = tmp_path / "standalone.html"
  original = "<html><body>plain page</body></html>"
  page.write_text(original)

  resp = _build_client().get("/files" + str(page))
  assert resp.status_code == 200
  assert "artifact-comments.js" not in resp.text
  assert resp.text == original
  assert "last-modified" in resp.headers
