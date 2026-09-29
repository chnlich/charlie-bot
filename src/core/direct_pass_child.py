"""The direct-pass trace build's child process: validate, then compress, off the server's GIL.

The validating parse holds the GIL for its whole run (measured 2.05-2.11 s event-loop stall
on the 334.3 MB corpus while the parse ran on a server thread), so both passes run here,
in a process whose GIL the server never waits on. The module level stays stdlib-only: the
parent imports the exit classes and the argv builder at server-import time.
"""

import subprocess
import sys
from pathlib import Path

EXIT_OK = 0
EXIT_NOT_A_TRACE = 3
EXIT_PARSE_FAILED = 4
EXIT_IGZIP_FAILED = 5


def parent_argv(trace_path: Path, out_path: Path, checkout_root: str) -> list[str]:
  """The spawn argv the parent runs; *checkout_root* puts ``src`` on the child's path."""
  return [sys.executable, str(Path(__file__).resolve()), str(trace_path), str(out_path), checkout_root]


def main(argv: list[str]) -> int:
  sys.path.insert(0, argv[3])
  import orjson

  from src.core.gc_control import gc_off
  from src.core.trace_merge import NotATraceError, _trace_events_or_raise, igzip_command

  trace_path, out_path = Path(argv[1]), Path(argv[2])
  # The compress starts before the parse so the two passes overlap, the shape the
  # in-process build ran: the wall is max(parse, compress), not their sum.
  with out_path.open("wb") as compressed:
    gzip_proc = subprocess.Popen(igzip_command("-c", str(trace_path)), stdout=compressed, stderr=subprocess.PIPE)
    try:
      # The parse allocates ~1M dicts per 1M input events; the generational passes
      # over that churn measured 0.27-0.35 s per 307 MB parse.
      with gc_off(collect=True), trace_path.open("rb") as validate_file:
        # Parseable JSON is not enough: a JSON object with no traceEvents array
        # (an analysis manifest) would otherwise compress into the cache and
        # reach the viewer as a trace that renders nothing.
        _trace_events_or_raise(orjson.loads(validate_file.read()), trace_path)
    except NotATraceError as error:
      _kill(gzip_proc)
      print(str(error), file=sys.stderr)
      return EXIT_NOT_A_TRACE
    except ValueError as error:  # orjson's decode errors are ValueError subclasses
      _kill(gzip_proc)
      print(f"trace failed to parse: {error}", file=sys.stderr)
      return EXIT_PARSE_FAILED
    detail = gzip_proc.stderr.read().decode(errors="replace").strip()
    if gzip_proc.wait() != 0:
      print(detail, file=sys.stderr)
      return EXIT_IGZIP_FAILED
  return EXIT_OK


def _kill(gzip_proc: subprocess.Popen) -> None:
  gzip_proc.kill()
  gzip_proc.wait()


if __name__ == "__main__":
  sys.exit(main(sys.argv))
