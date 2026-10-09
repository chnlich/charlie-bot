"""CLI script for the ``charliebot artifact`` subcommand.

  charliebot artifact check <file> --genre plan|understanding|sitrep|debug|explain
      [--trigger "<message>"] [--assertions-only] [--background]
  charliebot artifact wrap <fragment> --genre <genre> --output <page.html> [--math/--no-math]

``check`` runs the genre's mechanical DOM assertions, prints one line per assertion
(``ok <name>[ <measurement>]`` or ``FAIL <name>: <location>``), and — for every genre,
once every assertion passed — runs the cold-read probe unless ``--assertions-only`` was
given; ``--trigger`` is required for every genre unless ``--assertions-only``. With
``--background`` the probe leaves the foreground: after the assertions pass, the command
writes the cold-read log's first line (page path and sha256), starts the same check without
``--background`` as a new-session child appending to that log, registers a session wake on
the child through /api/internal/schedule-trigger, prints ``cold read started: pid <PID>
log <path> trigger <id>``, and exits 0; a rejected or failed registration kills the
child's process group and prints ``wake registration rejected: <server reason>`` before
exit 1. ``--background`` needs ``--trigger`` and refuses ``--assertions-only`` (both usage
errors); it is the one verb that resolves a session and calls the server. ``wrap``
assembles a genre page from a content fragment (the fragment fills <body>; head and style
come from the genre template), pre-rendering math to KaTeX markup for explain by default.
Exit codes: 0 = success, 1 = any assertion failed, the probe could not run, the wake
registration failed, or the assembled page failed the byte self-check, 2 = usage error.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import io
import json
import os
import pathlib
import signal
import subprocess
import sys
from collections.abc import Sequence
from datetime import UTC, datetime
from typing import TYPE_CHECKING

from src.features.artifacts import artifact_check, artifact_wrap, constants
from src.infra import constants as infra_constants
from src.infra import help_formatter, home
from src.runtime.cli import common as cli_common
from src.runtime.cli import schedule_trigger

if TYPE_CHECKING:
  from src.infra import config


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(
      prog="charliebot artifact", description="Artifact checks", formatter_class=help_formatter.CliHelpFormatter)
  sub = parser.add_subparsers(dest="verb", required=True)
  check = sub.add_parser(
      "check",
      help="Run a genre's assertions (and cold-read probe) on a local file",
      formatter_class=help_formatter.CliHelpFormatter)
  check.add_argument("file", help="Artifact path as an ordinary filesystem path (absolute or cwd-relative)")
  check.add_argument(
      "--genre", required=True, choices=constants.ARTIFACT_GENRES, help="Genre the page claims to follow")
  check.add_argument(
      "--trigger",
      default=None,
      help="Chat message that triggered the page (question 6 verbatim); required for every genre "
      "unless --assertions-only is given")
  check.add_argument(
      "--assertions-only", action="store_true", help="Run the assertions alone, skipping the cold-read probe")
  check.add_argument(
      "--background",
      action="store_true",
      help="Run the cold read detached once the assertions pass: append it to a log under "
      "/tmp/charliebot-coldread, register a session wake on the detached process, print "
      "'cold read started: pid <PID> log <path> trigger <id>' and exit")
  wrap = sub.add_parser(
      "wrap", help="Assemble a genre page from a content fragment", formatter_class=help_formatter.CliHelpFormatter)
  wrap.add_argument("fragment", help="Content fragment path: the page's <body> content")
  wrap.add_argument(
      "--genre", required=True, choices=constants.ARTIFACT_GENRES, help="Genre whose template shells the page")
  wrap.add_argument("--output", required=True, help="Assembled page path (the artifacts path to write)")
  wrap.add_argument(
      "--math",
      action=argparse.BooleanOptionalAction,
      default=None,
      help="Pre-render math to KaTeX markup at assembly time (default: on for explain, off for other genres)")
  return parser


def _spawn_cold_read(argv: list[str], log_path: pathlib.Path, cwd: pathlib.Path) -> subprocess.Popen:
  """Start the detached cold-read child: a new-session process with stdin from /dev/null and
  stdout and stderr appended to *log_path*."""
  with log_path.open("a", encoding="utf-8") as log:
    return subprocess.Popen(
        argv, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, cwd=cwd, start_new_session=True)


def _start_cold_read_log(log_path: pathlib.Path, artifact: pathlib.Path) -> None:
  """Create the cold-read log (parents included) with its first line: page path and the page
  bytes' sha256, so the wake can tell whether the page changed after the cold read started."""
  log_path.parent.mkdir(parents=True, exist_ok=True)
  digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
  log_path.write_text(f"page {artifact} sha256 {digest}\n", encoding="utf-8")


def _terminate_process_group(proc: subprocess.Popen) -> None:
  """SIGKILL the detached cold read's process group; the child leads it (start_new_session), so
  its own renderer subprocess dies with it."""
  # An already-exited cold read meets the termination goal; SIGKILL otherwise.
  with contextlib.suppress(ProcessLookupError):
    os.killpg(proc.pid, signal.SIGKILL)
  proc.wait()


def _rejection_reason(captured_stderr: str) -> str:
  """The server reason out of the JSON error line post_internal_api printed before exiting."""
  return str(json.loads(captured_stderr.strip().splitlines()[-1])["error"])


def _start_background_cold_read(args: argparse.Namespace, artifact: pathlib.Path, cfg: config.CharlieBotConfig) -> int:
  """Detach the cold read and register its wake; the module docstring holds the contract."""
  # The autonamer import drags fastapi and the sessions stack (~250 ms); it serves only this
  # branch's backend count, so the wrap and foreground paths must not pay it.
  from src.runtime import autonamer  # deferred: charliebot artifact --help

  session_id = cli_common.resolve_session_id(None)
  timestamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
  log_path = pathlib.Path("/tmp/charliebot-coldread") / session_id[:8] / f"{artifact.name}.{timestamp}.log"
  label = f"cold read: {log_path}"
  try:
    schedule_trigger._validate_message(label)
  except argparse.ArgumentTypeError as e:
    cli_common.exit_error(str(e))
  _start_cold_read_log(log_path, artifact)
  argv = [
      sys.executable, "-m",
      cli_common.cli_entry_module(), "artifact", "check",
      str(artifact), "--genre", args.genre, "--trigger", args.trigger
  ]
  # The same interpreter and the same code tree as this command: the tree root is the directory
  # holding the src package of this very file.
  proc = _spawn_cold_read(argv, log_path, pathlib.Path(__file__).resolve().parents[3])
  watch_targets = [{"kind": infra_constants.WatchKind.LOCAL_PID.value, "pid": proc.pid}]
  backend_count = sum(1 for _ in autonamer.iter_light_backends(cfg))
  payload = {
      "session_id": session_id,
      "delay_seconds": int(backend_count * artifact_check.ARTIFACT_PROBE_TIMEOUT) + 60,
      "message": label,
      "watch_targets": watch_targets,
  }
  stderr_capture = io.StringIO()
  try:
    with contextlib.redirect_stderr(stderr_capture):
      result = cli_common.post_internal_api(
          "/api/internal/schedule-trigger",
          payload,
          readback=lambda: schedule_trigger._readback_trigger(session_id, label, watch_targets))
  except SystemExit:
    # post_internal_api exits on a server rejection or a failed call, after printing the
    # reason as JSON on stderr; the cold read must not outlive its wake.
    _terminate_process_group(proc)
    print(f"wake registration rejected: {_rejection_reason(stderr_capture.getvalue())}")
    return 1
  if schedule_trigger._readback_trigger(session_id, label, watch_targets) is None:
    _terminate_process_group(proc)
    print("wake registration rejected: no pending trigger watching the cold-read pid on readback")
    return 1
  print(f"cold read started: pid {proc.pid} log {log_path} trigger {result['trigger_id']}")
  return 0


def _run_check(args: argparse.Namespace) -> int:
  if args.background and args.assertions_only:
    cli_common.exit_usage_error(
        "--background cannot combine with --assertions-only: "
        "the background child runs the full check, probe included")
  if args.background and args.trigger is None:
    cli_common.exit_usage_error("--background requires --trigger: the cold read answers the trigger")
  if args.trigger is None and not args.assertions_only:
    cli_common.exit_usage_error(f"--genre {args.genre} requires --trigger unless --assertions-only is given")
  artifact = pathlib.Path(args.file).resolve()
  if not artifact.is_file():
    cli_common.exit_error(f"artifact not found: {args.file}")
  cfg = cli_common.get_config()
  failed = 0
  for outcome in artifact_check.run_assertions(args.genre, artifact, cfg):
    if outcome.passed:
      print(f"ok {outcome.name}" + (f" {outcome.detail}" if outcome.detail else ""))
    else:
      failed += 1
      print(f"FAIL {outcome.name}: {outcome.detail}")
  if failed:
    return 1
  if args.assertions_only:
    return 0
  if args.background:
    return _start_background_cold_read(args, artifact, cfg)
  print("--- cold read ---")
  try:
    result = artifact_check.run_probe(cfg, artifact, args.trigger)
  except ValueError as e:
    cli_common.exit_error(str(e))
  for backend_id, error in result.attempts:
    print(f"attempt {backend_id} failed: {error}")
  if result.backend_id is None:
    print(f"probe could not run: every backend failed ({len(result.attempts)} tried)")
    return 1
  print(f"backend {result.backend_id}")
  print(result.answer)
  return 0


def _run_wrap(args: argparse.Namespace) -> int:
  fragment = pathlib.Path(args.fragment).resolve()
  if not fragment.is_file():
    cli_common.exit_usage_error(f"fragment not found: {args.fragment}")
  math = args.math if args.math is not None else args.genre == "explain"
  try:
    written = artifact_wrap.wrap_fragment(
        genre=args.genre,
        fragment=fragment,
        output=pathlib.Path(args.output).resolve(),
        math=math,
        vendor_path=artifact_wrap.vendor_katex_path(home.charliebot_home_dir()),
    )
  except (RuntimeError, ValueError) as e:
    cli_common.exit_error(str(e))
  print(f"wrote {written}")
  return 0


def main(argv: Sequence[str] | None = None) -> None:
  parser = _build_parser()
  args = parser.parse_args(argv if argv is not None else None)
  if args.verb == "check":
    sys.exit(_run_check(args))
  if args.verb == "wrap":
    sys.exit(_run_wrap(args))


if __name__ == "__main__":
  main()
