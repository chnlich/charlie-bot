"""The direct-pass trace build's child process: validate, then compress, off the server's GIL.

The validating parse holds the GIL for its whole run (a measured event-loop stall
of 2010-2014 ms on the 334.3 MB corpus while the parse ran on a server thread), so both passes run here,
in a process whose GIL the server never waits on. The module level stays stdlib-only: the
parent imports the exit classes and the argv builder at server-import time. The parse is the
build wall, so `_validate` fans it out; the fan-out's contract is `_split_chunks`'s.
"""

import subprocess
import sys
from pathlib import Path

EXIT_OK = 0
EXIT_NOT_A_TRACE = 3
EXIT_PARSE_FAILED = 4
EXIT_IGZIP_FAILED = 5

# Below this a split's helper spawn+import overhead outweighs the parse saving.
_MIN_CHUNK_BYTES = 64 << 20
_MAX_CHUNKS = 8
# The element-line pattern recurs every few hundred bytes on a pretty-printed
# trace, so a miss just drops that split.
_ANCHOR_WINDOW_BYTES = 8 << 20
_WS = b" \t\r\n"


def parent_argv(trace_path: Path, out_path: Path, checkout_root: str) -> list[str]:
  """The spawn argv the parent runs; *checkout_root* puts ``src`` on the child's path."""
  return [sys.executable, str(Path(__file__).resolve()), str(trace_path), str(out_path), checkout_root]


def _split_chunks(path: Path, size: int, min_chunk_bytes: int) -> tuple[bool, bytes, list[int]] | None:
  """Byte offsets tiling *path*'s traceEvents array into element-aligned chunks.

  Returns the document form (object with a traceEvents array, or bare array),
  the element-line indent, and the anchor offsets — each the newline starting an element line inside the
  events array; chunk 0 starts at 0 and the last chunk ends at *size*. None
  when the file cannot be split: not JSON starting with ``{``/``[``, no
  traceEvents array (the whole-file parse then also answers the shape check),
  fewer bytes than two chunks, a first element sharing the array's line
  (compact JSON), or no usable anchor at a boundary. The events array may be
  followed by sibling keys: the tail chunk validates them beside the elements.
  """
  if size < 2 * min_chunk_bytes:
    return None
  with path.open("rb") as f:
    raw_head = f.read(64 << 10)
  lead = len(raw_head) - len(raw_head.lstrip(_WS))
  head = raw_head[lead:]
  if head[:1] == b"[":
    object_form = False
    array_open = lead
  elif head[:1] == b"{":
    key_at = head.find(b'"traceEvents"')
    if key_at < 0:
      return None
    array_at = head.find(b"[", key_at)
    if array_at < 0:
      return None
    object_form = True
    array_open = lead + array_at
  else:
    return None
  with path.open("rb") as f:
    f.seek(array_open + 1)
    seg = f.read(4096)
  brace = seg.find(b"{")
  if brace < 0:
    return None
  line_at = seg.rfind(b"\n", 0, brace)
  if line_at < 0:
    return None
  indent = seg[line_at + 1:brace]
  if not set(indent) <= set(b" \t"):
    return None
  pattern = b"\n" + indent + b"{"
  k = max(2, min(_MAX_CHUNKS, size // min_chunk_bytes))
  starts: list[int] = []
  prev = array_open + 1

  def anchor_near(lo: int, hi: int, from_end: bool) -> int | None:
    with path.open("rb") as f:
      f.seek(lo)
      window = f.read(hi - lo)
    at = (window.rfind if from_end else window.find)(pattern)
    return None if at < 0 else lo + at

  for i in range(1, k):
    nominal = size * i // k
    if nominal <= prev:
      continue
    # The split wants an element line at or before the boundary; element lines
    # recur every few hundred bytes on a pretty-printed trace, so a window
    # ending at the boundary answers without reading the span since the last
    # boundary. The full-span scan stays for files whose element lines sit
    # wider apart than the window; a file whose elements all sit later takes
    # the first one after, so a split still makes progress on a small file,
    # where one window covers the whole file.
    anchor = anchor_near(max(prev, nominal - _ANCHOR_WINDOW_BYTES), min(nominal, size), from_end=True)
    if anchor is None:
      anchor = anchor_near(prev, min(nominal, size), from_end=True)
    if anchor is None:
      anchor = anchor_near(nominal, min(nominal + _ANCHOR_WINDOW_BYTES, size), from_end=False)
    if anchor is None or anchor <= prev:
      continue
    starts.append(anchor)
    prev = anchor
  # A tail chunk under a quarter of the target is one spawn for nearly nothing.
  if starts and size - starts[-1] < min_chunk_bytes // 4:
    starts.pop()
  if not starts:
    return None
  return object_form, indent, starts


def _chunk_parse_input(
    path: Path, start: int, end: int, index: int, count: int, object_form: bool, indent: bytes) -> bytes:
  """The chunk's bytes as one standalone JSON document for orjson.

  Every chunk ends right before the next element's newline, so its bytes end
  with a trailing ``,``, which every shape drops. The head re-closes what the
  split opened (the events array, then the object). The tail rebuilds a
  document from the elements it carries plus the original bytes after the
  array's close, so the sibling keys that follow the array are validated too.
  A rule finding bytes other than the expected ones means the anchor rules
  mis-fired: the ValueError this raises (or the parse error it produces) sends
  the build to the whole-file fallback, never to a wrong verdict.
  """
  with path.open("rb") as f:
    f.seek(start)
    core = f.read(end - start).rstrip(_WS)
  # A non-tail chunk's last byte is the next element's separator comma;
  # requiring it sends a missing comma at a split point to the whole-file
  # rejection instead of erasing it into a silent acceptance.
  if index < count - 1 and not core.endswith(b","):
    raise ValueError("chunk does not end with the element separator")
  if core.endswith(b","):
    core = core[:-1]
  if index == 0:
    return core + (b"]}" if object_form else b"]")
  if index == count - 1 and object_form:
    close_at = core.rfind(b"\n" + indent + b"}")
    if close_at < 0:
      raise ValueError("tail chunk carries no element close")
    elements_end = close_at + len(indent) + 2
    suffix = core[elements_end:].lstrip(_WS)
    if not suffix.startswith(b"]"):
      raise ValueError("tail chunk does not close the events array")
    return b'{"traceEvents":[' + core[:elements_end] + suffix
  if index == count - 1:
    if not core.rstrip(_WS).endswith(b"]"):
      raise ValueError("tail chunk does not close the events array")
    core = core.rstrip(_WS)[:-1].rstrip(_WS)
    if core.endswith(b","):
      core = core[:-1]
  return b"[" + core + b"]"


def _validate_chunk_main(argv: list[str]) -> int:
  """One helper's work: parse its byte range wrapped, exit EXIT_OK or EXIT_PARSE_FAILED."""
  import gc

  import orjson

  gc.disable()
  path, start, end = Path(argv[0]), int(argv[1]), int(argv[2])
  index, count, object_form = int(argv[3]), int(argv[4]), argv[5] == "1"
  indent = argv[6].encode()
  try:
    orjson.loads(_chunk_parse_input(path, start, end, index, count, object_form, indent))
  except ValueError as error:  # orjson's decode errors are ValueError subclasses
    print(f"chunk {index} failed to parse: {error}", file=sys.stderr)
    return EXIT_PARSE_FAILED
  return EXIT_OK


def _validate_chunk_argv(
    trace_path: Path, start: int, end: int, index: int, count: int, object_form: bool, indent: bytes) -> list[str]:
  return [
      sys.executable,
      str(Path(__file__).resolve()),
      "--validate-chunk",
      str(trace_path),
      str(start),
      str(end),
      str(index),
      str(count),
      "1" if object_form else "0",
      indent.decode(),
  ]


def _validate(trace_path: Path, orjson: object, shape_check: object, gc_off: object, min_chunk_bytes: int) -> None:
  """The fan-out validation: head chunk in this process, the rest in helpers.

  The whole-file parse is the authority whenever anything in the fan-out fails —
  a chunk failure can be the anchors mis-firing rather than a corrupt file — so
  the final verdict is always a whole-file parse's.
  """
  split = _split_chunks(trace_path, trace_path.stat().st_size, min_chunk_bytes)
  if split is None:
    _validate_whole(trace_path, orjson, shape_check, gc_off)
    return
  object_form, indent, starts = split
  count = len(starts) + 1
  size = trace_path.stat().st_size
  helpers: list[subprocess.Popen] = []
  spawned_all = True
  for index in range(1, count):
    end = size if index == count - 1 else starts[index]
    try:
      helpers.append(
          subprocess.Popen(
              _validate_chunk_argv(trace_path, starts[index - 1], end, index, count, object_form, indent),
              stderr=subprocess.DEVNULL))
    except OSError:
      spawned_all = False
      break
  head_failed = False
  try:
    with gc_off(collect=True):
      parsed = orjson.loads(_chunk_parse_input(trace_path, 0, starts[0], 0, count, object_form, indent))
    shape_check(parsed, trace_path)
  except ValueError:
    head_failed = True
  if not spawned_all or head_failed or any(helper.wait() != EXIT_OK for helper in helpers):
    _validate_whole(trace_path, orjson, shape_check, gc_off)


def _validate_whole(trace_path: Path, orjson: object, shape_check: object, gc_off: object) -> None:
  with gc_off(collect=True), trace_path.open("rb") as validate_file:
    # Parseable JSON is not enough: a JSON object with no traceEvents array
    # (an analysis manifest) would otherwise compress into the cache and
    # reach the viewer as a trace that renders nothing.
    shape_check(orjson.loads(validate_file.read()), trace_path)


def main(argv: list[str]) -> int:
  if argv[1] == "--validate-chunk":
    return _validate_chunk_main(argv[2:])
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
      _validate(trace_path, orjson, _trace_events_or_raise, gc_off, _MIN_CHUNK_BYTES)
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
