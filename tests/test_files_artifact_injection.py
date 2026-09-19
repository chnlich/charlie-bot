"""Tests for artifact review-UI injection in the file server (src/api/files.py)."""

import gzip
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import assert_gzip_served, gzip_explode_compress, mount_production_gzip, stub_credentials
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import files as files_api
from src.api import pages as pages_api
from src.core import plan_diff

SCRIPT = f"<script src=/static/js/artifact-comments.js?v={pages_api._static_asset_version()}></script>"


@pytest.fixture
def sessions_root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """Point the configured sessions root at a test value.

  files.py imports ``get_config`` by name, so the patch lands on the module. A
  real app carries the auth middleware, which owns the credential gate; these
  tests cover the file server alone, where the tray injection no longer reads
  the request at all — no credential is stubbed or sent unless a test wants to
  pin that the router ignores it.
  """
  monkeypatch.setattr(files_api, "get_config", lambda: SimpleNamespace(sessions_dir=tmp_path))
  return tmp_path


def _build_client(access_key: str | None = None) -> TestClient:
  """A files-router client that sends *access_key* as the charliebot_access_key
  cookie; None sends no credential. The cookie rides the client because httpx
  deprecates per-request cookies. The router ignores the credential either way —
  the argument exists to pin that injection never branches on it."""
  app = FastAPI()
  app.include_router(files_api.router, prefix="/absolute_filepath")
  cookies = {"charliebot_access_key": access_key} if access_key is not None else None
  return TestClient(app, cookies=cookies)


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


def test_injected_script_tag_carries_the_cache_bust_version(monkeypatch: pytest.MonkeyPatch) -> None:
  """The artifact tray script is fetched with the same cache-bust query every
  template uses, so an upgrade cannot leave a browser on a stale copy."""
  monkeypatch.setattr(pages_api, "_GIT_VERSION", "abc1234 · 03-24")
  version = pages_api._static_asset_version()
  assert version.startswith("abc1234-03-24-")  # the git part, then the served tree's content digest
  out = files_api._inject_artifact_ui("<html><body></body></html>", "S")
  assert f"<script src=/static/js/artifact-comments.js?v={version}></script>" in out


# --- route-level: inject vs not-inject decision, anchored on the sessions root ---


def test_serve_file_injects_for_artifact_path(sessions_root: Path) -> None:
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


def test_serve_file_injects_thread_artifact_with_session_id_not_thread_id(sessions_root: Path) -> None:
  page = _write(sessions_root / "S" / "threads" / "T" / "artifacts" / "x.html")

  resp = _build_client("secret").get("/absolute_filepath" + str(page))
  assert resp.status_code == 200
  assert 'window.__cbcServerSessionId="S";' in resp.text
  assert '"T"' not in resp.text


def test_serve_file_builds_the_1mib_chunk_response(sessions_root: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The file arm serves through _ServedFileResponse — the 1 MiB-chunk subclass
  is the route's whole transport knob, so a revert to the base FileResponse must
  fail this test, not silently re-price every artifact serve."""
  page = _write(sessions_root / "S" / "artifacts" / "x.png")
  built = []
  real = files_api._ServedFileResponse

  class _Recording(real):

    def __init__(self, *args: object, **kwargs: object) -> None:
      built.append(self)
      super().__init__(*args, **kwargs)

  monkeypatch.setattr(files_api, "_ServedFileResponse", _Recording)
  resp = _build_client("secret").get("/absolute_filepath" + str(page))
  assert resp.status_code == 200
  assert len(built) == 1
  assert built[0].chunk_size == 1 << 20


def test_serve_file_binary_body_is_byte_identical_across_chunks(sessions_root: Path) -> None:
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


def test_serve_file_injects_deeper_nested_artifact(sessions_root: Path) -> None:
  # A depth the old path-shape regex never matched: the predicate only cares that the
  # page sits under <session>/... with an `artifacts` parent, not how deep.
  page = _write(sessions_root / "S" / "threads" / "T" / "sub" / "artifacts" / "x.html")

  resp = _build_client("secret").get("/absolute_filepath" + str(page))
  assert resp.status_code == 200
  assert 'window.__cbcServerSessionId="S";' in resp.text


@pytest.mark.parametrize("access_key", [None, "wrong", "secret"], ids=["no-credential", "wrong-credential", "valid"])
def test_serve_file_injects_whatever_the_request_carried(sessions_root: Path, access_key: str | None) -> None:
  """The tray injects unconditionally: no credential state may suppress it.

  The credential gate lives in the auth middleware alone — a per-request branch
  here is what let a stale-cookie reader load an artifact whose comment buttons
  silently vanished. At this router the request's credential is inert."""
  page = _write(sessions_root / "S" / "artifacts" / "x.html")

  resp = _build_client(access_key).get("/absolute_filepath" + str(page))
  assert resp.status_code == 200
  assert "artifact-comments.js" in resp.text
  assert 'window.__cbcServerSessionId="S";' in resp.text


def test_serve_file_empty_configured_key_injects(sessions_root: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """No configured key = every reader authenticates, so injection stays on."""
  monkeypatch.setattr(files_api, "get_config", lambda: SimpleNamespace(sessions_dir=sessions_root))
  stub_credentials({"charliebot": {"access_key": ""}})
  page = _write(sessions_root / "S" / "artifacts" / "x.html")

  resp = _build_client(None).get("/absolute_filepath" + str(page))
  assert resp.status_code == 200
  assert 'window.__cbcServerSessionId="S";' in resp.text


def test_serve_file_session_html_outside_artifacts_not_injected(sessions_root: Path) -> None:
  page = _write(sessions_root / "S" / "notes" / "x.html")

  resp = _build_client("secret").get("/absolute_filepath" + str(page))
  assert resp.status_code == 200
  assert "artifact-comments.js" not in resp.text
  # Kept as a FileResponse: served from disk with a last-modified validator.
  assert "last-modified" in resp.headers


def test_serve_file_root_level_artifacts_dir_not_injected(sessions_root: Path) -> None:
  # <root>/artifacts/x.html belongs to no session — there is no session component.
  page = _write(sessions_root / "artifacts" / "x.html")

  resp = _build_client("secret").get("/absolute_filepath" + str(page))
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

  resp = _build_client("secret").get("/absolute_filepath" + str(page))
  assert resp.status_code == 200
  assert "artifact-comments.js" not in resp.text
  assert "last-modified" in resp.headers


# --- clean views: the injected-page memo serves repeat views without re-reading ---


def test_serve_file_clean_repeat_view_reinjects_nothing(sessions_root: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The injection is a pure function of the page bytes, so a repeat view of an
  unchanged page must serve the stored body with zero injection calls."""
  page = _write(sessions_root / "S" / "artifacts" / "x.html")
  client = _build_client("secret")
  url = "/absolute_filepath" + str(page)
  first = client.get(url)
  assert first.status_code == 200

  def explode(html_text: str, session_id: str) -> str:
    raise AssertionError("repeat view re-ran the artifact injection")

  monkeypatch.setattr(files_api, "_inject_artifact_ui", explode)
  resp = client.get(url)
  assert resp.status_code == 200
  assert resp.text == first.text
  assert SCRIPT in resp.text


def test_serve_file_clean_reinjects_when_page_is_rewritten(sessions_root: Path) -> None:
  """An artifact page is only ever written whole, so a rewrite always moves the
  (mtime_ns, size) signature the memo keys on — the new bytes must be served."""
  page = _write(sessions_root / "S" / "artifacts" / "x.html")
  client = _build_client("secret")
  url = "/absolute_filepath" + str(page)
  before = client.get(url)
  assert before.status_code == 200

  page.write_text("<html><body><p>rewritten plan</p></body></html>", encoding="utf-8")
  after = client.get(url)
  assert after.status_code == 200
  assert after.text != before.text
  assert "<p>rewritten plan</p>" in after.text
  assert SCRIPT in after.text


# --- clean views: the gzip form ships pre-compressed so the server's gzip
# middleware skips its own whole-body deflate ---


def _build_gzip_client(access_key: str | None = None) -> TestClient:
  """The files router behind the production gzip mount, so the test sees the
  skip the pre-compressed response buys. The credential argument mirrors
  _build_client: the router never reads it."""
  app = FastAPI()
  app.include_router(files_api.router, prefix="/absolute_filepath")
  mount_production_gzip(app)
  cookies = {"charliebot_access_key": access_key} if access_key is not None else None
  return TestClient(app, cookies=cookies)


def test_serve_file_gzip_view_ships_precompressed_injected_page(sessions_root: Path) -> None:
  page = _write(sessions_root / "S" / "artifacts" / "x.html")

  resp = _build_gzip_client("secret").get("/absolute_filepath" + str(page), headers={"Accept-Encoding": "gzip"})
  assert resp.status_code == 200
  assert_gzip_served(resp)
  assert resp.headers["content-type"].startswith("text/html")
  # What ships is the injected page, compressed: the decoded body is byte-exact
  # against the plain form, comment layer and session id included.
  assert resp.text == files_api._inject_artifact_ui(page.read_text(encoding="utf-8"), "S")
  assert SCRIPT in resp.text
  assert resp.headers["cache-control"] == "no-store"


def test_serve_file_without_gzip_accept_gets_plain_body(sessions_root: Path) -> None:
  """The compressed form is memoized per encoding negotiation: a client whose
  Accept-Encoding names no gzip reads the plain injected page, no encoding set."""
  page = _write(sessions_root / "S" / "artifacts" / "x.html")

  resp = _build_client("secret").get("/absolute_filepath" + str(page), headers={"Accept-Encoding": "br"})
  assert resp.status_code == 200
  assert "content-encoding" not in resp.headers
  assert resp.text == files_api._inject_artifact_ui(page.read_text(encoding="utf-8"), "S")
  assert resp.headers["cache-control"] == "no-store"


def test_serve_file_gzip_repeat_view_recompresses_nothing(sessions_root: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A repeat gzip view of an unchanged page must serve the stored compressed
  body with zero injection and zero deflate calls."""
  page = _write(sessions_root / "S" / "artifacts" / "x.html")
  client = _build_client("secret")
  url = "/absolute_filepath" + str(page)
  first = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert first.status_code == 200

  def explode_inject(html_text: str, session_id: str) -> str:
    raise AssertionError("repeat gzip view re-ran the artifact injection")

  monkeypatch.setattr(files_api, "_inject_artifact_ui", explode_inject)
  monkeypatch.setattr(files_api, "gzip_level1", gzip_explode_compress("repeat gzip view re-ran the deflate"))
  resp = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert resp.status_code == 200
  assert resp.headers["content-encoding"] == "gzip"
  assert resp.text == first.text


def test_serve_file_gzip_recompresses_when_page_is_rewritten(sessions_root: Path) -> None:
  """A rewrite moves the signature both memos key on — the compressed form must
  never serve bytes of the page it was not built from."""
  page = _write(sessions_root / "S" / "artifacts" / "x.html")
  client = _build_client("secret")
  url = "/absolute_filepath" + str(page)
  before = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert before.status_code == 200

  page.write_text("<html><body><p>rewritten plan</p></body></html>", encoding="utf-8")
  after = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert after.status_code == 200
  assert after.text != before.text
  assert "<p>rewritten plan</p>" in after.text
  assert after.headers["vary"] == "Accept-Encoding"


def test_injected_artifact_page_gzip_is_deterministic_and_round_trips(sessions_root: Path) -> None:
  """mtime=0 keeps the compressed bytes identical across processes, and the form
  decompresses to exactly the plain body the plain memo serves."""
  page = _write(sessions_root / "S" / "artifacts" / "x.html")
  first = files_api._injected_artifact_page_gzip(page, "S")
  second = files_api._injected_artifact_page_gzip(page, "S")
  assert first == second
  assert gzip.decompress(first) == files_api._injected_artifact_page(page, "S")


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

  resp = _build_client("secret").get("/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.html")
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
  assert resp.headers["cache-control"] == "no-store"


def test_serve_file_diff_without_credential_still_annotates_and_injects(sessions_root: Path) -> None:
  """No credential state may suppress the marks or the tray: the router never
  reads the request (the middleware owns the gate), so both layers are there."""
  _, new = _write_pages(sessions_root)

  resp = _build_client(None).get("/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.html")
  assert resp.status_code == 200
  assert "cbd-ins" in resp.text
  assert SCRIPT in resp.text


def test_serve_file_without_diff_is_byte_identical_to_pre_diff_response(sessions_root: Path) -> None:
  page = _write(sessions_root / "S" / "artifacts" / "x.html")
  original = page.read_text(encoding="utf-8")

  resp = _build_client("secret").get("/absolute_filepath" + str(page))
  assert resp.status_code == 200
  # Exactly the bytes the handler produced before the diff feature existed:
  # the page wrapped in the artifact UI, with no diff machinery involved.
  assert resp.text == files_api._inject_artifact_ui(original, "S")


def test_serve_file_diff_missing_base_is_404_naming_the_path(sessions_root: Path) -> None:
  new = sessions_root / "S" / "artifacts" / "plan_02.html"
  _write(new)

  resp = _build_client("secret").get("/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.html")
  assert resp.status_code == 404
  assert "plan_01.html" in resp.text


def test_serve_file_diff_base_outside_session_artifacts_is_400(sessions_root: Path) -> None:
  _, new = _write_pages(sessions_root)
  _write(sessions_root / "S" / "notes" / "plan_01.html")

  resp = _build_client("secret").get("/absolute_filepath" + str(new) + "?diff=notes/plan_01.html")
  assert resp.status_code == 400


def test_serve_file_diff_base_non_html_is_400(sessions_root: Path) -> None:
  _, new = _write_pages(sessions_root)
  (sessions_root / "S" / "artifacts" / "plan_01.txt").write_text("not html", encoding="utf-8")

  resp = _build_client("secret").get("/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.txt")
  assert resp.status_code == 400


def test_serve_file_diff_non_artifact_target_is_400(sessions_root: Path) -> None:
  _, _new = _write_pages(sessions_root)
  notes_page = _write(sessions_root / "S" / "notes" / "page.html")
  (sessions_root / "S" / "artifacts" / "directory.html").mkdir()
  text_file = sessions_root / "S" / "artifacts" / "file.txt"
  text_file.write_text("plain", encoding="utf-8")

  resp = _build_client("secret").get("/absolute_filepath" + str(notes_page) + "?diff=artifacts/plan_02.html")
  assert resp.status_code == 400
  resp = _build_client("secret").get("/absolute_filepath" + str(text_file) + "?diff=artifacts/plan_02.html")
  assert resp.status_code == 400
  resp = _build_client("secret").get(
      "/absolute_filepath" + str(sessions_root / "S" / "artifacts" / "directory.html") + "?diff=artifacts/plan_02.html")
  assert resp.status_code == 400


# --- diff requests: the annotate memo serves repeat views without re-annotating ---


def test_serve_file_diff_repeat_view_reannotates_nothing(sessions_root: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An annotate is a pure function of the two files' bytes, so a repeat view of an
  unchanged pair must serve the stored page with zero annotate calls."""
  _base, new = _write_pages(sessions_root)
  client = _build_client("secret")
  url = "/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.html"
  first = client.get(url)
  assert first.status_code == 200

  def explode(base_html: str, new_html: str) -> str:
    raise AssertionError("repeat view re-ran plan_diff.annotate")

  monkeypatch.setattr(plan_diff, "annotate", explode)
  resp = client.get(url)
  assert resp.status_code == 200
  assert resp.text == first.text


def test_serve_file_diff_reannotates_when_target_is_rewritten(sessions_root: Path) -> None:
  """An artifact page is only ever written whole, so a rewrite always moves the
  (mtime_ns, size) signature the memo keys on — the new bytes must be served."""
  _base, new = _write_pages(sessions_root)
  client = _build_client("secret")
  url = "/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.html"
  before = client.get(url)
  assert before.status_code == 200

  new.write_text("<html><body><p>hello whole new world</p></body></html>", encoding="utf-8")
  after = client.get(url)
  assert after.status_code == 200
  assert after.text != before.text
  # The inserted words are present under word-level marks, not as one literal run.
  assert "cbd-ins" in after.text
  assert ">whole new</ins>" in after.text


def test_serve_file_diff_reannotates_when_base_is_rewritten(sessions_root: Path) -> None:
  base, new = _write_pages(sessions_root)
  client = _build_client("secret")
  url = "/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.html"
  before = client.get(url)
  assert before.status_code == 200

  base.write_text("<html><body><p>goodbye world</p></body></html>", encoding="utf-8")
  after = client.get(url)
  assert after.status_code == 200
  assert after.text != before.text


def test_serve_file_diff_serves_one_page_to_any_credential(sessions_root: Path) -> None:
  """The tray rides every diff view unconditionally, so the credential a request
  carried no longer picks a memo variant: every reader gets the same bytes."""
  _base, new = _write_pages(sessions_root)
  url = "/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.html"

  credentialed = _build_client("secret").get(url)
  anonymous = _build_client(None).get(url)
  assert SCRIPT in credentialed.text
  assert SCRIPT in anonymous.text
  assert credentialed.text == anonymous.text


# --- diff requests: the gzip form ships pre-compressed so the server's gzip
# middleware skips its own whole-body deflate ---


def test_serve_file_diff_gzip_ships_precompressed_annotated_page(sessions_root: Path) -> None:
  base, new = _write_pages(sessions_root)

  resp = _build_gzip_client("secret").get(
      "/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.html", headers={"Accept-Encoding": "gzip"})
  assert resp.status_code == 200
  assert_gzip_served(resp)
  assert resp.headers["content-type"].startswith("text/html")
  # What ships is the annotated page, compressed: the decoded body is byte-exact
  # against the plain form, marks and comment layer included.
  expected = files_api._inject_artifact_ui(
      plan_diff.annotate(base.read_text(encoding="utf-8"), new.read_text(encoding="utf-8")), "S")
  assert resp.text == expected
  assert "cbd-ins" in resp.text
  assert resp.headers["cache-control"] == "no-store"


def test_serve_file_diff_without_gzip_accept_gets_plain_body(sessions_root: Path) -> None:
  """The compressed form is memoized per encoding negotiation: a client whose
  Accept-Encoding names no gzip reads the plain annotated page, no encoding set."""
  _base, new = _write_pages(sessions_root)

  resp = _build_client("secret").get(
      "/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.html", headers={"Accept-Encoding": "br"})
  assert resp.status_code == 200
  assert "content-encoding" not in resp.headers
  assert "cbd-ins" in resp.text


def test_serve_file_diff_gzip_repeat_view_recompresses_nothing(
    sessions_root: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A repeat gzip compare view of an unchanged pair must serve the stored
  compressed body with zero annotate and zero deflate calls."""
  _base, new = _write_pages(sessions_root)
  client = _build_client("secret")
  url = "/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.html"
  first = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert first.status_code == 200

  def explode(base_html: str, new_html: str) -> str:
    raise AssertionError("repeat gzip view re-ran plan_diff.annotate")

  monkeypatch.setattr(plan_diff, "annotate", explode)
  monkeypatch.setattr(files_api, "gzip_level1", gzip_explode_compress("repeat gzip view re-ran the deflate"))
  resp = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert resp.status_code == 200
  assert resp.headers["content-encoding"] == "gzip"
  assert resp.text == first.text


def test_serve_file_diff_gzip_reannotates_when_target_is_rewritten(sessions_root: Path) -> None:
  """A rewrite moves the signature both memos key on — the compressed form must
  never serve bytes of the page it was not built from."""
  _base, new = _write_pages(sessions_root)
  client = _build_client("secret")
  url = "/absolute_filepath" + str(new) + "?diff=artifacts/plan_01.html"
  before = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert before.status_code == 200

  new.write_text("<html><body><p>rewritten plan</p></body></html>", encoding="utf-8")
  after = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert after.status_code == 200
  assert after.text != before.text
  # The rewritten words are present under word-level marks.
  assert "cbd-ins" in after.text
  assert "rewritten" in after.text
  assert after.headers["vary"] == "Accept-Encoding"


def test_annotated_diff_page_gzip_is_deterministic_and_round_trips(sessions_root: Path) -> None:
  """mtime=0 keeps the compressed bytes identical across processes, and the form
  decompresses to exactly the plain body the plain memo serves."""
  base, new = _write_pages(sessions_root)
  first = files_api._annotated_diff_page_gzip(base, new, session_id="S")
  second = files_api._annotated_diff_page_gzip(base, new, session_id="S")
  assert first == second
  assert gzip.decompress(first) == files_api._annotated_diff_page(base, new, session_id="S").encode("utf-8")
