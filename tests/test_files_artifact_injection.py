"""Tests for the artifact view the file server reaches through the wiring registry (src/features/artifacts/artifact_view.py)."""

import asyncio
import pathlib
import types

import fastapi
import pytest
from fastapi import testclient

from src.features.artifacts import artifact_view
from src.features.files import api as files_api
from src.infra import config
from src.runtime import templating

SCRIPT = f"<script src=/static/js/artifact-comments.js?v={templating.static_asset_version()}></script>"


@pytest.fixture
def sessions_root(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> pathlib.Path:
  """Point the configured sessions root at a test value.

  files.py reaches ``get_config`` through the config module, so the patch lands
  there. A
  real app carries the auth middleware, which owns the credential gate; these
  tests cover the file server alone, where the tray injection no longer reads
  the request at all — no credential is stubbed or sent unless a test wants to
  pin that the router ignores it.
  """
  monkeypatch.setattr(config, "get_config", lambda: types.SimpleNamespace(sessions_dir=tmp_path))
  return tmp_path


def _build_client(access_key: str | None) -> testclient.TestClient:
  """A files-router client that sends *access_key* as the charliebot_access_key
  cookie; None sends no credential. The cookie rides the client because httpx
  deprecates per-request cookies. The router ignores the credential either way —
  the argument exists to pin that injection never branches on it."""
  app = fastapi.FastAPI()
  app.include_router(files_api.router, prefix="/absolute_filepath")
  cookies = {"charliebot_access_key": access_key} if access_key is not None else None
  return testclient.TestClient(app, cookies=cookies)


def _write(path: pathlib.Path) -> pathlib.Path:
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text("<html><body><h1>Plan</h1></body></html>")
  return path


# --- pure string-transform helper: structural invariants ---


def test_inject_inserts_exactly_one_before_body() -> None:
  html = "<html><body><p>plan body</p></body></html>"
  out = artifact_view._inject_artifact_ui(html, "S")
  assert out.count("artifact-comments.js") == 1
  # The closing tag is preserved and the script sits before it.
  assert out.count("</body>") == 1
  assert out.index("artifact-comments.js") < out.index("</body>")
  # The inline id assignment precedes the external script tag, so the id is set
  # before artifact-comments.js runs.
  assert out.index("window.__cbcServerSessionId=") < out.index(SCRIPT)


# --- route-level: inject vs not-inject decision, anchored on the sessions root ---


def test_serve_file_injects_for_artifact_path(sessions_root: pathlib.Path) -> None:
  page = _write(sessions_root / "S" / "artifacts" / "x.html")

  resp = _build_client("secret").get("/absolute_filepath" + str(page))
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
  # The injected page carries per-session state, so no cache may keep it.
  assert resp.headers["cache-control"] == "no-store"


def test_serve_file_binary_body_is_byte_identical_across_chunks(sessions_root: pathlib.Path) -> None:
  """A payload larger than one 1 MiB chunk rides the chunked read path; the
  served body must be exactly the file's bytes with identity transport."""
  page = sessions_root / "S" / "artifacts" / "trace.bin"
  page.parent.mkdir(parents=True, exist_ok=True)
  payload = bytes(range(256)) * ((2 << 20) // 256) + b"tail"
  assert len(payload) > (1 << 20)
  page.write_bytes(payload)

  resp = _build_client("secret").get("/absolute_filepath" + str(page))
  assert resp.status_code == 200
  assert "content-encoding" not in resp.headers
  assert resp.content == payload


def test_serve_file_injects_deeper_nested_artifact(sessions_root: pathlib.Path) -> None:
  # A deeply nested page: the predicate only cares that the
  # page sits under <session>/... with an `artifacts` parent, not how deep.
  page = _write(sessions_root / "S" / "threads" / "T" / "sub" / "artifacts" / "x.html")

  resp = _build_client("secret").get("/absolute_filepath" + str(page))
  assert resp.status_code == 200
  assert 'window.__cbcServerSessionId="S";' in resp.text


# --- clean views: the injected-page memo serves repeat views without re-reading ---

# --- clean views: the gzip form ships pre-compressed so the server's gzip
# middleware skips its own whole-body deflate ---

# --- diff requests: ?diff=<base artifact path> serves the annotated page ---


def _write_pages(sessions_root: pathlib.Path) -> tuple[pathlib.Path, pathlib.Path]:
  """A two-version artifact pair whose word-level difference plan_diff can mark."""
  base = sessions_root / "S" / "artifacts" / "plan_01.html"
  new = sessions_root / "S" / "artifacts" / "plan_02.html"
  base.parent.mkdir(parents=True, exist_ok=True)
  base.write_text("<html><body><p>hello world</p></body></html>", encoding="utf-8")
  new.write_text("<html><body><p>hello brave world</p></body></html>", encoding="utf-8")
  return base, new


def test_serve_file_without_diff_is_byte_identical_to_pre_diff_response(sessions_root: pathlib.Path) -> None:
  page = _write(sessions_root / "S" / "artifacts" / "x.html")
  original = page.read_text(encoding="utf-8")

  resp = _build_client("secret").get("/absolute_filepath" + str(page))
  assert resp.status_code == 200
  # Exactly the bytes the handler produced before the diff feature existed:
  # the page wrapped in the artifact UI, with no diff machinery involved.
  assert resp.text == artifact_view._inject_artifact_ui(original, "S")


def test_serve_file_diff_missing_base_is_404_naming_the_path(sessions_root: pathlib.Path) -> None:
  new = sessions_root / "S" / "artifacts" / "plan_02.html"
  _write(new)

  resp = _build_client("secret").get("/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.html")
  assert resp.status_code == 404
  assert "plan_01.html" in resp.text


def test_serve_file_diff_base_outside_session_artifacts_is_400(sessions_root: pathlib.Path) -> None:
  _, new = _write_pages(sessions_root)
  _write(sessions_root / "S" / "notes" / "plan_01.html")

  resp = _build_client("secret").get("/absolute_filepath" + str(new) + "?diff=notes/plan_01.html")
  assert resp.status_code == 400


# --- the view costs a plain file and a listing no executor hop ---


@pytest.mark.parametrize(
    ("target", "accept_encoding", "hops"),
    [("notes.txt", "identity", 1), ("notes.txt", "gzip", 2), ("", "identity", 1), ("", "gzip", 1)],
    ids=["file", "file-gzip", "listing", "listing-gzip"])
def test_a_plain_file_and_a_listing_take_the_executor_hops_they_took_before_the_view(
    sessions_root: pathlib.Path, monkeypatch: pytest.MonkeyPatch, target: str, accept_encoding: str, hops: int) -> None:
  """The hop counts are the file server's own: its resolve-and-list hop, plus the bare-file gzip memo's."""
  served = sessions_root / "plain"
  served.mkdir()
  (served / "notes.txt").write_text("plain text\n" * 20, encoding="utf-8")
  calls: list[object] = []
  real_to_thread = asyncio.to_thread

  async def counting_to_thread(func, /, *args, **kwargs):
    calls.append(func)
    return await real_to_thread(func, *args, **kwargs)

  monkeypatch.setattr(asyncio, "to_thread", counting_to_thread)

  resp = _build_client(None).get(f"/absolute_filepath{served}/{target}", headers={"Accept-Encoding": accept_encoding})

  assert resp.status_code == 200
  assert len(calls) == hops
