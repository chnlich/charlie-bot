"""The single-trace merged-trace build's child process.

The build runs here, in a raw subprocess, because a spawn-context pool worker
re-imports the parent's ``__main__`` — the full server module, ~0.6 s of the M99
import floor on every build — and a forkserver child re-imports it too (the
forked child runs spawn's preparation data, ``_serve_one`` → ``spawn._main``).
A fresh process per build is still required: a build's freed arenas stay mapped
in a reused worker's address space and the next build's allocations only
sometimes reuse them, the retention the merge pool's ``max_tasks_per_child=1``
exists for. The child is that same fresh address space without the server
re-import. Module level stays stdlib-only: the parent imports the argv builder
at server-import time, so the M99 server import floor carries no trace stack.
"""

import pathlib
import sys

EXIT_OK = 0
EXIT_FAILED = 1
EXIT_NOT_A_TRACE = 2


def parent_argv(paths: list[pathlib.Path], out_path: pathlib.Path, slim: bool, checkout_root: str) -> list[str]:
  """The spawn argv the parent runs; *checkout_root* puts ``src`` on the child's path."""
  return [
      sys.executable,
      str(pathlib.Path(__file__).resolve()),
      checkout_root,
      *[str(path) for path in paths],
      str(out_path),
      "1" if slim else "0",
  ]


def main(argv: list[str]) -> int:
  """One build: merge the argv's traces into the output path, exit by error class.

  The exit classes mirror the in-process build's error types so the route
  re-raises what it answered before the build left the process: a trace that
  parses but is not Chrome-JSON is EXIT_NOT_A_TRACE, every other build failure
  (a decode error, a missing file's traceback exit) is EXIT_FAILED.
  """
  sys.path.insert(0, argv[1])
  paths = [pathlib.Path(value) for value in argv[2:-2]]
  out_path = pathlib.Path(argv[-2])
  slim = argv[-1] == "1"
  from src.core import trace_merge

  try:
    trace_merge.merge_traces(paths, out_path, slim)
  except trace_merge.NotATraceError as error:
    print(str(error), file=sys.stderr)
    return EXIT_NOT_A_TRACE
  except ValueError as error:  # orjson decode errors subclass ValueError
    print(str(error), file=sys.stderr)
    return EXIT_FAILED
  return EXIT_OK


if __name__ == "__main__":
  sys.exit(main(sys.argv))
