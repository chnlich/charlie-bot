"""Browser login: one click starts one login process, shows its approval link, and holds it to an outcome.

Vocabulary:

- A *login* is one ``host_auth.browser_logins`` entry.
- An *attempt* is one login process started by one click, from the click until its outcome is recorded.
  The readback phase is part of the attempt. A live attempt holds its login's slot; a click that starts a
  new attempt ends the live one with the outcome REPLACED.
- The *readback* is the login's check command. It exits 0 while the login is valid.

Only ``start_login`` starts a login command. The page runs readback commands alone. Records live in memory.
"""

import asyncio
import collections
import dataclasses
import datetime
import enum
import re

from src.features.host_auth import config as host_auth_config
from src.infra import log_once, models, tasks

log = log_once.LazyStructlogLogger()

# The approval link must print within this many seconds of the click. The AWS CLI prints it after at most
# two requests, so a longer wait means the network or the config is broken.
START_TIMEOUT_SEC = 30.0
# An AWS device code expires after 600 seconds. This limit only bounds a wedged child process.
HOLD_LIMIT_SEC = 30 * 60
READBACK_TIMEOUT_SEC = 30.0
PAGE_READBACK_TIMEOUT_SEC = 20.0
OUTPUT_KEEP_LINES = 200
START_FAILURE_TAIL_LINES = 20

_USER_CODE_PARAM = re.compile(r"[?&]user_code=([^&#]*)")


class Outcome(enum.Enum):
  """How an attempt ended. UNKNOWN marks an attempt still in progress, so no finished attempt holds it."""

  UNKNOWN = 0
  APPROVED = 1
  LOGIN_FAILED = 2
  READBACK_FAILED = 3
  REPLACED = 4
  HOLD_LIMIT = 5
  START_FAILED = 6

  @property
  def label(self) -> str:
    """The page's word for this outcome."""
    match self:
      case Outcome.APPROVED:
        return "approved"
      case Outcome.LOGIN_FAILED:
        return "login failed"
      case Outcome.READBACK_FAILED:
        return "readback failed"
      case Outcome.REPLACED:
        return "replaced"
      case Outcome.HOLD_LIMIT:
        return "hold limit"
      case Outcome.START_FAILED:
        return "start failure"
      case _:
        raise ValueError(f"{self!r} has no page label; UNKNOWN marks an attempt still in progress")


class Phase(enum.Enum):
  """Where an attempt in progress stands."""

  STARTING = 1
  WAITING_FOR_APPROVAL = 2
  READING_BACK = 3

  @property
  def label(self) -> str:
    """The page's word for this phase."""
    match self:
      case Phase.STARTING:
        return "starting"
      case Phase.WAITING_FOR_APPROVAL:
        return "waiting for approval"
      case Phase.READING_BACK:
        return "reading back"
      case _:
        raise ValueError(f"{self!r} has no page label")


class StartFailedError(Exception):
  """A start that produced no approval link. Its message is the 502 body of the click that waited on it."""


def user_code_of(url: str) -> str:
  """The value of the link's first ``user_code=`` parameter, in the query or the fragment; empty when none."""
  match = _USER_CODE_PARAM.search(url)
  return match.group(1) if match else ""


@dataclasses.dataclass(eq=False)
class Attempt:
  """One login process started by one click, and the record the page reads.

  The attempt's driver task is the only writer. Reads and writes run on the one event loop, so a page
  render never sees a half-written record.
  """

  login: host_auth_config.BrowserLoginConfig
  started_at: datetime.datetime
  # Resolves to the approval link, or fails with StartFailedError. Every click of this start awaits it.
  link: asyncio.Future[str]
  phase: Phase = Phase.STARTING
  # The live child: the login process, then the readback process.
  process: asyncio.subprocess.Process | None = None
  task: asyncio.Task | None = None
  approval_url: str | None = None
  output: collections.deque[str] = dataclasses.field(
      default_factory=lambda: collections.deque(maxlen=OUTPUT_KEEP_LINES))
  login_exit: int | None = None
  readback_exit: int | None = None
  readback_output: str = ""
  outcome: Outcome = Outcome.UNKNOWN
  finished_at: datetime.datetime | None = None

  @property
  def ended(self) -> bool:
    """True once the outcome is recorded."""
    return self.outcome is not Outcome.UNKNOWN

  @property
  def user_code(self) -> str:
    """The device code the approval link carries; empty before the link is known."""
    return user_code_of(self.approval_url) if self.approval_url is not None else ""


# The live attempt of each login name, and the last attempt that ended for it. An attempt is written into
# these only while it is live, and a newer attempt is always the live one, so an older attempt never
# overwrites a newer record.
_live: dict[str, Attempt] = {}
_last: dict[str, Attempt] = {}


def live_attempt(name: str) -> Attempt | None:
  """The login's attempt in progress, or None."""
  return _live.get(name)


def last_attempt(name: str) -> Attempt | None:
  """The login's most recently ended attempt, or None."""
  return _last.get(name)


async def start_login(login: host_auth_config.BrowserLoginConfig) -> str:
  """Start an attempt of *login* and return its approval link; a click during a start joins that start.

  The attempt is registered before the first await, so a click that arrives while this start waits joins
  it rather than starting a second process. Raises StartFailedError when the login gives no link.
  """
  current = _live.get(login.name)
  if current is not None and not current.link.done():
    return await current.link
  if current is not None:
    _end(current, Outcome.REPLACED)
  attempt = Attempt(login=login, started_at=models.utc_now(), link=asyncio.get_running_loop().create_future())
  _live[login.name] = attempt
  attempt.task = tasks.create_logged_task(_drive(attempt), name=f"browser-login-{login.name}")
  return await attempt.link


async def page_readback(login: host_auth_config.BrowserLoginConfig) -> tuple[int, str]:
  """Run the login's readback once for the page: ``(exit code, combined output)``. The login never runs here."""
  return await _finish_capture(await _spawn(login.readback_command), PAGE_READBACK_TIMEOUT_SEC)


async def stop_all() -> None:
  """End every live attempt, and return once each of its processes has been reaped.

  Cancelling a driver ends its attempt: the driver's cleanup kills the child and records the outcome.
  """
  live = list(_live.values())
  drivers = [attempt.task for attempt in live if attempt.task is not None]
  for driver in drivers:
    driver.cancel()
  if drivers:
    await asyncio.wait(drivers)
  for attempt in live:
    if attempt.process is not None:
      await attempt.process.wait()


async def _drive(attempt: Attempt) -> None:
  """Run one attempt from its start to its recorded outcome. A driver that fails or is cancelled ends it too."""
  try:
    await _run(attempt)
  finally:
    if not attempt.ended:
      _abandon(attempt)


async def _run(attempt: Attempt) -> None:
  login = attempt.login
  try:
    proc = await _spawn(login.login_command)
  except OSError as exc:
    _fail_start(attempt, f"could not start the login command: {exc}")
    return
  attempt.process = proc
  log.info("browser_login_started", login=login.name)
  link = await _await_link(attempt, proc)
  if link is None:
    attempt.login_exit = await proc.wait()
    _fail_start(attempt, _failure_text(attempt.login_exit, attempt.output))
    return
  attempt.approval_url = link
  attempt.phase = Phase.WAITING_FOR_APPROVAL
  attempt.link.set_result(link)
  log.info("browser_login_link_found", login=login.name)
  await _hold(attempt, proc)


async def _await_link(attempt: Attempt, proc: asyncio.subprocess.Process) -> str | None:
  """The approval link from the first matching output line; None when the output ends first or the start times out."""
  try:
    return await asyncio.wait_for(_read_until_match(attempt, proc), timeout=START_TIMEOUT_SEC)
  except TimeoutError:
    _kill(proc)
    return None


async def _read_until_match(attempt: Attempt, proc: asyncio.subprocess.Process) -> str | None:
  while (line := await _read_line(attempt, proc)) is not None:
    match = re.search(attempt.login.approval_url_pattern, line)
    if match:
      return match.group(0)
  return None


async def _hold(attempt: Attempt, proc: asyncio.subprocess.Process) -> None:
  """Read the login's output to its end and wait for it to exit, within HOLD_LIMIT_SEC; then read back."""
  try:
    attempt.login_exit = await asyncio.wait_for(_drain_and_wait(attempt, proc), timeout=HOLD_LIMIT_SEC)
  except TimeoutError:
    _kill(proc)
    attempt.login_exit = await proc.wait()
    _end(attempt, Outcome.HOLD_LIMIT)
    return
  if attempt.ended:
    return
  if attempt.login_exit != 0:
    _end(attempt, Outcome.LOGIN_FAILED)
    return
  await _read_back(attempt)


async def _drain_and_wait(attempt: Attempt, proc: asyncio.subprocess.Process) -> int:
  while await _read_line(attempt, proc) is not None:
    pass
  return await proc.wait()


async def _read_back(attempt: Attempt) -> None:
  login = attempt.login
  attempt.phase = Phase.READING_BACK
  try:
    proc = await _spawn(login.readback_command)
  except OSError as exc:
    attempt.readback_output = f"could not start the readback command: {exc}"
    _end(attempt, Outcome.READBACK_FAILED)
    return
  attempt.process = proc
  attempt.readback_exit, attempt.readback_output = await _finish_capture(proc, READBACK_TIMEOUT_SEC)
  if attempt.ended:
    return
  _end(attempt, Outcome.APPROVED if attempt.readback_exit == 0 else Outcome.READBACK_FAILED)


async def _read_line(attempt: Attempt, proc: asyncio.subprocess.Process) -> str | None:
  """The next output line, appended to the attempt's tail; None once the output has ended."""
  raw = await proc.stdout.readline()
  if not raw:
    return None
  line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
  attempt.output.append(line)
  return line


async def _spawn(argv: list[str]) -> asyncio.subprocess.Process:
  """Start *argv* without a shell, in the server's environment, with stdin on /dev/null and output merged."""
  return await asyncio.create_subprocess_exec(
      *argv, stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT)


async def _finish_capture(proc: asyncio.subprocess.Process, timeout_sec: float) -> tuple[int, str]:
  """Wait for a captured child: ``(exit code, output)``. Past *timeout_sec* it is killed, and its output drained."""
  try:
    raw, _ = await asyncio.wait_for(proc.communicate(), timeout=timeout_sec)
  except TimeoutError:
    _kill(proc)
    raw, _ = await proc.communicate()
  return proc.returncode, raw.decode("utf-8", errors="replace")


def _kill(proc: asyncio.subprocess.Process) -> None:
  """Kill *proc* unless it has already exited."""
  if proc.returncode is None:
    proc.kill()


def _failure_text(exit_code: int, output: collections.deque[str]) -> str:
  """The 502 body: the exit code, then the last output lines."""
  return "\n".join([f"exit {exit_code}", *list(output)[-START_FAILURE_TAIL_LINES:]])


def _fail_start(attempt: Attempt, body: str) -> None:
  """Record a start failure and fail the click waiting on the link with *body*."""
  log.warning("browser_login_start_failed", login=attempt.login.name, login_exit=attempt.login_exit)
  _end(attempt, Outcome.START_FAILED)
  attempt.link.set_exception(StartFailedError(body))


def _abandon(attempt: Attempt) -> None:
  """End an attempt whose driver raised or was cancelled, so that no live slot or waiting click is left behind."""
  match attempt.phase:
    case Phase.STARTING:
      _fail_start(attempt, "the browser login ended before it gave an approval link")
    case Phase.WAITING_FOR_APPROVAL:
      _end(attempt, Outcome.LOGIN_FAILED)
    case Phase.READING_BACK:
      _end(attempt, Outcome.READBACK_FAILED)
    case _:
      raise ValueError(f"{attempt.phase!r} has no abandon path")


def _end(attempt: Attempt, outcome: Outcome) -> None:
  """Record *attempt*'s outcome, kill its live child if one runs, and release its login's live slot.

  An attempt that already ended keeps its record: a newer click ends a live attempt as REPLACED, and the
  replaced process's own exit then has nothing left to record.
  """
  if outcome is Outcome.UNKNOWN:
    raise ValueError("an attempt ends with a recorded outcome; UNKNOWN marks one still in progress")
  if attempt.ended:
    return
  if attempt.process is not None:
    _kill(attempt.process)
  name = attempt.login.name
  attempt.outcome = outcome
  attempt.finished_at = models.utc_now()
  del _live[name]
  _last[name] = attempt
  log.info(
      "browser_login_finished",
      login=name,
      login_exit=attempt.login_exit,
      readback_exit=attempt.readback_exit,
      outcome=outcome.label)
