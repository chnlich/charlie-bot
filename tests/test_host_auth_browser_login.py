"""Browser login: the start route's 303 and 502, attempt replacement, outcomes, the page, and stop_service.

The login and readback commands are small Python programs written into the test's temp dir. The login
prints the shape of the AWS device-code output with synthetic values, then waits for a release file.
"""

import asyncio
import contextlib
import dataclasses
import pathlib
import signal
import sys
from collections.abc import AsyncIterator, Callable

import fastapi
import httpx
import pydantic
import pytest
import pytest_asyncio

from src.features.host_auth import api, browser_login
from src.features.host_auth import config as host_auth_config
from src.infra import config
from src.runtime.api import deps

LOGIN_NAME = "fake-sso"
APPROVAL_PATTERN = r"https://\S*user_code=\S+"
START_URL = f"/api/host-auth/logins/{LOGIN_NAME}/start"
PAGE_URL = "/host-auth/logins"
ARN = "arn:aws:sts::000000000000:assumed-role/example-role/tester"

LOGIN_SCRIPT = '''
import pathlib
import sys
import time

counter, release = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
starts = int(counter.read_text()) + 1 if counter.exists() else 1
counter.write_text(str(starts))
code = f"ABCD-{starts:04d}"
for line in [
    "Browser will not be automatically opened.",
    "Please visit the following URL:",
    "",
    "https://device.example.invalid/start/#/device",
    "",
    "Then enter the code:",
    "",
    code,
    "",
    "Alternatively, you may visit the following URL which will autofill the code upon loading:",
    f"https://device.example.invalid/start/#/device?user_code={code}",
]:
    print(line, flush=True)
while not release.exists():
    time.sleep(0.02)
status = int(release.read_text())
if status:
    print("aws: [ERROR]: An error occurred (InvalidGrantException) when calling the CreateToken operation:", flush=True)
    print("error_description: Invalid device code provided", flush=True)
sys.exit(status)
'''

READBACK_SCRIPT = '''
import pathlib
import sys
import time

status, gate = int(sys.argv[1]), pathlib.Path(sys.argv[2])
while not gate.exists():
    time.sleep(0.02)
print("arn:aws:sts::000000000000:assumed-role/example-role/tester" if status == 0
      else "Token has expired and refresh failed", flush=True)
sys.exit(status)
'''


@dataclasses.dataclass(frozen=True)
class _Paths:
  home: pathlib.Path
  counter: pathlib.Path  # the login script counts its starts here
  release: pathlib.Path  # the login script exits with the status this file holds
  gate: pathlib.Path  # the readback script waits for this file
  login_script: pathlib.Path
  readback_script: pathlib.Path


@pytest.fixture
def paths(tmp_path: pathlib.Path) -> _Paths:
  login_script = tmp_path / "fake_login.py"
  login_script.write_text(LOGIN_SCRIPT, encoding="utf-8")
  readback_script = tmp_path / "fake_readback.py"
  readback_script.write_text(READBACK_SCRIPT, encoding="utf-8")
  gate = tmp_path / "readback.gate"
  gate.touch()
  return _Paths(
      home=tmp_path,
      counter=tmp_path / "starts",
      release=tmp_path / "release",
      gate=gate,
      login_script=login_script,
      readback_script=readback_script)


@pytest_asyncio.fixture(autouse=True)
async def _no_attempts_left() -> AsyncIterator[None]:
  """Each test starts with no attempt on record and ends with no login process alive."""
  browser_login._last.clear()
  yield
  await browser_login.stop_all()
  browser_login._last.clear()


def _login_argv(paths: _Paths) -> list[str]:
  return [sys.executable, "-I", str(paths.login_script), str(paths.counter), str(paths.release)]


def _readback_argv(paths: _Paths, status: int) -> list[str]:
  return [sys.executable, "-I", str(paths.readback_script), str(status), str(paths.gate)]


def _cfg(paths: _Paths, *, login_command: list[str] | None = None, readback_status: int = 0) -> config.CharlieBotConfig:
  login = {
      "name": LOGIN_NAME,
      "label": "Fake SSO",
      "login_command": login_command or _login_argv(paths),
      "approval_url_pattern": APPROVAL_PATTERN,
      "readback_command": _readback_argv(paths, readback_status),
  }
  return config.CharlieBotConfig(charliebot_home=paths.home, host_auth={"browser_logins": [login]})


@contextlib.asynccontextmanager
async def _client(cfg: config.CharlieBotConfig) -> AsyncIterator[httpx.AsyncClient]:
  """A client on the router in this event loop; lifespan does not run, so no service starts."""
  app = fastapi.FastAPI()
  app.include_router(api.router)
  app.dependency_overrides[deps.get_config_on_loop] = lambda: cfg
  async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://test") as client:
    yield client


def _release(paths: _Paths, status: int) -> None:
  """Let the login exit with *status*. The rename keeps the script from reading a half-written file."""
  temp = paths.release.with_suffix(".tmp")
  temp.write_text(str(status), encoding="utf-8")
  temp.replace(paths.release)


async def _until(predicate: Callable[[], bool], timeout_sec: float = 1.5) -> None:
  loop = asyncio.get_running_loop()
  deadline = loop.time() + timeout_sec
  while not predicate():
    if loop.time() > deadline:
      pytest.fail("condition not reached before the deadline")
    await asyncio.sleep(0.01)


def _live_or_fail() -> browser_login.Attempt:
  attempt = browser_login.live_attempt(LOGIN_NAME)
  assert attempt is not None
  return attempt


@pytest.mark.asyncio
async def test_click_redirects_to_the_approval_link_with_its_fragment(paths: _Paths) -> None:
  async with _client(_cfg(paths)) as client:
    response = await client.post(START_URL)
  assert response.status_code == 303
  assert response.headers["location"] == "https://device.example.invalid/start/#/device?user_code=ABCD-0001"


@pytest.mark.asyncio
async def test_double_click_during_a_start_joins_it(paths: _Paths) -> None:
  async with _client(_cfg(paths)) as client:
    first, second = await asyncio.gather(client.post(START_URL), client.post(START_URL))
  assert first.status_code == second.status_code == 303
  assert first.headers["location"] == second.headers["location"]
  assert paths.counter.read_text(encoding="utf-8") == "1"


@pytest.mark.asyncio
async def test_click_after_the_start_ends_the_old_attempt_and_starts_a_new_one(paths: _Paths) -> None:
  async with _client(_cfg(paths)) as client:
    await client.post(START_URL)
    old = _live_or_fail()
    second = await client.post(START_URL)
  assert second.headers["location"] == "https://device.example.invalid/start/#/device?user_code=ABCD-0002"
  await old.task
  assert old.outcome is browser_login.Outcome.REPLACED
  assert old.process is not None and old.process.returncode is not None
  assert browser_login.last_attempt(LOGIN_NAME) is old
  assert browser_login.live_attempt(LOGIN_NAME) is not old
  assert paths.counter.read_text(encoding="utf-8") == "2"


@pytest.mark.asyncio
async def test_login_and_readback_exit_zero_records_approved_with_the_readback_output(paths: _Paths) -> None:
  async with _client(_cfg(paths)) as client:
    await client.post(START_URL)
    attempt = _live_or_fail()
    _release(paths, 0)
    await attempt.task
    page = await client.get(PAGE_URL)
  assert attempt.outcome is browser_login.Outcome.APPROVED
  assert attempt.readback_output.strip() == ARN
  assert ARN in page.text


@pytest.mark.asyncio
async def test_nonzero_login_exit_records_login_failed_and_skips_the_readback(paths: _Paths) -> None:
  async with _client(_cfg(paths)) as client:
    await client.post(START_URL)
    attempt = _live_or_fail()
    _release(paths, 254)
    await attempt.task
    page = await client.get(PAGE_URL)
  assert attempt.outcome is browser_login.Outcome.LOGIN_FAILED
  assert attempt.login_exit == 254
  assert attempt.readback_exit is None
  assert attempt.output[-1] == "error_description: Invalid device code provided"
  assert "Invalid device code provided" in page.text


@pytest.mark.asyncio
async def test_hold_limit_kills_the_process_and_records_hold_limit(
    paths: _Paths, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(browser_login, "HOLD_LIMIT_SEC", 0.2)
  async with _client(_cfg(paths)) as client:
    await client.post(START_URL)
    attempt = _live_or_fail()
    await attempt.task
  assert attempt.outcome is browser_login.Outcome.HOLD_LIMIT
  assert attempt.process is not None and attempt.process.returncode == -signal.SIGKILL


@pytest.mark.asyncio
async def test_attempt_stays_in_progress_while_the_readback_runs(paths: _Paths) -> None:
  paths.gate.unlink()
  async with _client(_cfg(paths)) as client:
    await client.post(START_URL)
    attempt = _live_or_fail()
    _release(paths, 0)
    await _until(lambda: attempt.phase is browser_login.Phase.READING_BACK)
    assert browser_login.live_attempt(LOGIN_NAME) is attempt
    assert attempt.outcome is browser_login.Outcome.UNKNOWN
    page = await client.get(PAGE_URL)
    assert attempt.approval_url in page.text
    paths.gate.touch()
    await attempt.task
  assert attempt.outcome is browser_login.Outcome.APPROVED


@pytest.mark.asyncio
async def test_no_link_before_the_process_exits_returns_502_and_leaves_no_process(paths: _Paths) -> None:
  exit_early = [sys.executable, "-I", "-c", "print('boom', flush=True); raise SystemExit(3)"]
  async with _client(_cfg(paths, login_command=exit_early)) as client:
    response = await client.post(START_URL)
  assert response.status_code == 502
  assert response.text.splitlines() == ["exit 3", "boom"]
  assert browser_login.live_attempt(LOGIN_NAME) is None
  last = browser_login.last_attempt(LOGIN_NAME)
  assert last is not None and last.outcome is browser_login.Outcome.START_FAILED
  assert last.process is not None and last.process.returncode == 3


@pytest.mark.asyncio
async def test_no_link_within_the_start_timeout_kills_the_process(
    paths: _Paths, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(browser_login, "START_TIMEOUT_SEC", 0.2)
  silent = [sys.executable, "-I", "-c", "import time; time.sleep(60)"]
  async with _client(_cfg(paths, login_command=silent)) as client:
    response = await client.post(START_URL)
  assert response.status_code == 502
  assert response.text == "exit -9"
  last = browser_login.last_attempt(LOGIN_NAME)
  assert last is not None and last.outcome is browser_login.Outcome.START_FAILED
  assert last.process is not None and last.process.returncode == -signal.SIGKILL


@pytest.mark.asyncio
async def test_unknown_login_name_returns_404_and_starts_nothing(paths: _Paths) -> None:
  async with _client(_cfg(paths)) as client:
    response = await client.post("/api/host-auth/logins/no-such-login/start")
  assert response.status_code == 404
  assert not paths.counter.exists()


@pytest.mark.asyncio
async def test_page_without_logins_says_none_are_configured(paths: _Paths) -> None:
  cfg = config.CharlieBotConfig(charliebot_home=paths.home)
  async with _client(cfg) as client:
    response = await client.get(PAGE_URL)
  assert response.status_code == 200
  assert "No browser logins are configured" in response.text
  assert "host_auth.browser_logins" in response.text


@pytest.mark.asyncio
async def test_stop_service_ends_a_live_login_process(paths: _Paths) -> None:
  async with _client(_cfg(paths)) as client:
    await client.post(START_URL)
  attempt = _live_or_fail()
  await api.stop_service()
  assert attempt.task is not None and attempt.task.done()
  assert attempt.process is not None and attempt.process.returncode is not None
  assert attempt.outcome is browser_login.Outcome.LOGIN_FAILED
  assert browser_login.live_attempt(LOGIN_NAME) is None


@pytest.mark.asyncio
async def test_page_view_runs_only_the_readback_command(paths: _Paths) -> None:
  async with _client(_cfg(paths)) as client:
    response = await client.get(PAGE_URL)
  assert response.status_code == 200
  assert not paths.counter.exists()
  assert ARN in response.text


@pytest.mark.parametrize(
    ("logins", "field_path"),
    [
        pytest.param(
            [
                {
                    "name": "dup",
                    "label": "A",
                    "login_command": ["x"],
                    "approval_url_pattern": "a",
                    "readback_command": ["y"]
                },
                {
                    "name": "dup",
                    "label": "B",
                    "login_command": ["x"],
                    "approval_url_pattern": "a",
                    "readback_command": ["y"]
                },
            ],
            r"browser_logins",
            id="duplicate-name"),
        pytest.param(
            [
                {
                    "name": "Bad_Name",
                    "label": "A",
                    "login_command": ["x"],
                    "approval_url_pattern": "a",
                    "readback_command": ["y"]
                }
            ],
            r"browser_logins\.0\.name",
            id="bad-name"),
        pytest.param(
            [{
                "name": "ok",
                "label": "A",
                "login_command": [],
                "approval_url_pattern": "a",
                "readback_command": ["y"]
            }],
            r"browser_logins\.0\.login_command",
            id="empty-login-argv"),
        pytest.param(
            [
                {
                    "name": "ok",
                    "label": "A",
                    "login_command": ["x"],
                    "approval_url_pattern": "https://(",
                    "readback_command": ["y"]
                }
            ],
            r"browser_logins\.0\.approval_url_pattern",
            id="regex-does-not-compile"),
    ],
)
def test_config_rejects_an_invalid_login_entry(logins: list[dict], field_path: str) -> None:
  with pytest.raises(pydantic.ValidationError, match=field_path):
    host_auth_config.HostAuthConfig.model_validate({"browser_logins": logins})
