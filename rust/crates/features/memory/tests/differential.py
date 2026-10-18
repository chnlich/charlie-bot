"""Compare the memory store's server half with Python's, answer by answer.

Each synthetic store of a seeded corpus is built, read by Python's `memory.select_master_memory` and
`memory.select_worker_memory` for the master audience and each repo basename, rebuilt, and read by the
`memory-select` helper (`src/bin/memory-select.rs`). Each selection must match byte for byte (every segment, its
sources and texts), and each format error must carry the same text. For empty directories, `ensure_store` must leave
the same file tree in both, `.git` excluded.

Usage: uv run python rust/crates/features/memory/tests/differential.py [path/to/memory-select]
"""

from __future__ import annotations

import dataclasses
import json
import pathlib
import random
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT))

from src.features.memory import memory  # noqa: E402

SEED = 20261018
RANDOM_STORES = 80
ENSURE_DIRS = 4
DEFAULT_BINARY = ROOT / "rust/target/release/memory-select"
REPO_BASENAMES = ("", "profile", "alpha", "gamma-service", "unknown-repo")
TOPICS = "profile resident\nalpha\nbeta\ngamma-service\n"


@dataclasses.dataclass(frozen=True)
class Store:
  """One synthetic store: relative path to text, or to None for a directory."""

  name: str
  files: dict[str, str | None]


def entry(*header: str, body: str = "body\n") -> str:
  """A closed entry: the header lines between two `---` lines, then the body."""
  return "---\n" + "".join(line + "\n" for line in header) + "---\n" + body


VALID_HEADER = ("scope: user", "topic: profile", "audience: master, worker", "title: Note")


def valid_stores() -> list[Store]:
  """Valid stores that cover the selection's shapes, plus the grammar edge cases a real store can hold."""
  dark_mode = entry(
      "scope: user", "topic: profile", "audience: master, worker", "title: Dark Mode", body="Prefers dark themes.\n")
  return [
      Store(
          "mixed-audiences",
          {
              "topics":
                  TOPICS,
              "entries/profile/dark-mode.md":
                  dark_mode,
              "entries/alpha/master-host.md":
                  entry("scope: host", "topic: alpha", "audience: master", "title: 记忆 Host ✓", body="中文正文 🙂\n"),
              "entries/alpha/both-spaced.md":
                  entry("scope: user", "topic: alpha", "audience: master , worker", "title: Spaced", body="both\n"),
              "entries/beta/worker-only.md":
                  entry("scope: user", "topic: beta", "audience: worker", "title: Worker Only", body="worker body\n"),
              "entries/gamma-service/build.md":
                  entry(
                      "scope: user", "topic: gamma-service", "audience: worker,master", "title: Build", body="steps\n"),
              "staging/capture.md":
                  "# Capture\n\nstaged fact, never selected\n",
          },
      ),
      Store(
          "crlf-and-lone-cr",
          {
              "topics":
                  "profile resident\r\nalpha\r\n",
              "entries/profile/crlf.md":
                  dark_mode.replace("\n", "\r\n"),
              "entries/alpha/lone-cr.md":
                  entry("scope: user", "topic: alpha", "audience: worker", "title: Lone CR", body="one\rtwo\n"),
          },
      ),
      Store(
          "bodies",
          {
              "topics":
                  TOPICS,
              "entries/profile/rules.md":
                  entry(*VALID_HEADER, body="Rule one.\n\n---\nRule two.\n--- tail\n"),
              "entries/profile/heading.md":
                  entry(*VALID_HEADER, body="# Own Heading\n\nText.\n"),
              "entries/profile/trailing.md":
                  entry(*VALID_HEADER, body="Trailing blank lines.\n\n\n"),
              "entries/profile/newline-only.md":
                  entry(*VALID_HEADER, body="\n"),
              "entries/alpha/unicode.md":
                  entry(
                      "scope: user",
                      "topic: alpha",
                      "audience: master, worker",
                      "title: 　Café 记忆 ",
                      body="Tab\there, back\\slash, 日本語\n"),
          },
      ),
      Store(
          "dotted-and-dot-md",
          {
              "topics": TOPICS,
              "entries/profile/a.b.md": entry(*VALID_HEADER, body="dotted slug\n"),
              "entries/profile/.md": entry(*VALID_HEADER, body="hidden-looking slug\n"),
              "entries/profile/notes.txt": "not an entry, never read\n",
              "entries/profile/sub/deep.md": "---\nnot even a header\n",
              "entries/loose.md": "---\nloose file at the entries root\n",
          },
      ),
      Store("no-entries-dir", {"topics": TOPICS}),
      Store("topics-missing", {"entries/profile/only.md": entry(*VALID_HEADER)}),
      Store(
          "topics-blank-lines",
          {
              "topics": "profile resident\n\n\nbeta\n",
              "entries/profile/only.md": entry(*VALID_HEADER),
          },
      ),
  ]


def format_error_stores() -> list[Store]:
  """One store per format error: each holds a single broken file, or a broken vocabulary."""
  profile = {"topics": TOPICS}

  def with_entry(name: str, relative: str, text: str) -> Store:
    return Store(name, {**profile, relative: text})

  return [
      with_entry("no-opener", "entries/profile/x.md", "scope: user\ntopic: profile\n---\nbody\n"),
      with_entry("malformed-uppercase-key", "entries/profile/x.md", entry("Title: Note", *VALID_HEADER[:3])),
      with_entry("malformed-no-space", "entries/profile/x.md", entry("title:Note", *VALID_HEADER[:3])),
      with_entry("malformed-slug-space", "entries/profile/x.md", entry("scope: user x", *VALID_HEADER[1:])),
      with_entry(
          "malformed-audience-shape", "entries/profile/x.md",
          entry(*VALID_HEADER[:2], "audience: master worker", "title: Note")),
      with_entry(
          "audience-leading-space", "entries/profile/x.md",
          entry(*VALID_HEADER[:2], "audience:  master, worker", "title: Note")),
      with_entry("lone-cr-splits-a-header", "entries/profile/x.md", entry("scope: us\rer", *VALID_HEADER[1:])),
      with_entry("malformed-tab", "entries/profile/x.md", entry("scope:\tuser", *VALID_HEADER[1:])),
      with_entry("malformed-control", "entries/profile/x.md", entry("scope: us\x01er", *VALID_HEADER[1:])),
      with_entry("malformed-quote", "entries/profile/x.md", entry("scope: u'ser", *VALID_HEADER[1:])),
      with_entry("unknown-field", "entries/profile/x.md", entry("revises: old-entry", *VALID_HEADER)),
      with_entry("duplicate-field", "entries/profile/x.md", entry(*VALID_HEADER, "title: Again")),
      with_entry(
          "empty-title", "entries/profile/x.md",
          entry("scope: user", "topic: profile", "audience: worker", "title:    ")),
      with_entry("missing-title", "entries/profile/x.md", entry(*VALID_HEADER[:3])),
      with_entry("missing-scope", "entries/profile/x.md", entry("topic: profile", "audience: worker", "title: N")),
      with_entry("missing-topic", "entries/profile/x.md", entry("scope: user", "audience: worker", "title: N")),
      with_entry("missing-audience", "entries/profile/x.md", entry("scope: user", "topic: profile", "title: N")),
      with_entry("missing-closer", "entries/profile/x.md", "---\nscope: user\ntopic: profile\ntitle: N"),
      with_entry("empty-body", "entries/profile/x.md", entry(*VALID_HEADER, body="")),
      with_entry("bad-filename", "entries/profile/bad slug.md", entry(*VALID_HEADER)),
      with_entry("non-ascii-filename", "entries/profile/记忆.md", entry(*VALID_HEADER)),
      with_entry(
          "unknown-topic", "entries/nonexistent/x.md",
          entry("scope: user", "topic: nonexistent", "audience: worker", "title: N")),
      with_entry("directory-mismatch", "entries/wrongdir/x.md", entry(*VALID_HEADER)),
      with_entry(
          "bad-topic-name", "entries/Bad_Name/x.md",
          entry("scope: user", "topic: Bad_Name", "audience: worker", "title: N")),
      with_entry("bad-scope", "entries/profile/x.md", entry("scope: team", *VALID_HEADER[1:])),
      with_entry(
          "bad-audience-element", "entries/profile/x.md",
          entry("scope: user", "topic: profile", "audience: master,all", "title: N")),
      with_entry(
          "two-violations", "entries/nonexistent/bad slug.md",
          entry("scope: user", "topic: nonexistent", "audience: worker", "title: N")),
      Store("topics-bad-line", {"topics": "profile\nBad Topic\n"}),
      Store("topics-duplicate", {"topics": "profile\nprofile resident\n"}),
      Store("topics-double-space", {"topics": "profile  resident\n"}),
      Store("topics-is-a-directory", {"topics": None}),
      Store(
          "topic-missing-from-vocabulary", {
              "topics": "profile resident\n",
              "entries/alpha/x.md": entry("scope: user", "topic: alpha", "audience: master", "title: N")
          }),
  ]


TOPIC_POOL = ("profile", "alpha", "beta", "gamma-service", "charliebot", "host-notes")
SLUG_WORDS = ("dark-mode", "build", "naming", "review", "a.b", "x_y")
AUDIENCES = ("master", "worker", "master, worker", "worker,master", "master , worker")
SCOPES = ("user", "host")
TITLES = ("Dark Mode", "记忆 测试", "Café ✓", "A: B", "  padded  ", "　ideographic")
BODIES = (
    "Plain body.\n",
    "中文正文 with emoji 🙂\n",
    "# Own Heading\n\nText.\n",
    "Rule one.\n\n---\nRule two.\n",
    "Trailing blank lines.\n\n\n",
    "\n",
    "Tab\there and back\\slash\n",
    "Lone\rcarriage\n",
)
INJECTED = (
    entry("scope: user", "topic: alpha", "audience: master", "title: N", "bogus: 1"),
    entry("scope: user", "topic: alpha", "audience: master", "title: N", "audience: worker"),
    entry("scope: team", "topic: alpha", "audience: master", "title: N"),
    entry("scope: user", "topic: alpha", "audience: master,nobody", "title: N"),
    entry("scope: user", "topic: alpha", "audience: worker", "title:"),
    "scope: user\ntopic: alpha\n---\nno opener\n",
)


def random_stores(rng: random.Random) -> list[Store]:
  """Seeded stores: resident and non-resident topics, every audience and scope, repo-matched topics, and some with
  one malformed file."""
  stores = []
  for index in range(RANDOM_STORES):
    names = rng.sample(TOPIC_POOL, rng.randint(1, len(TOPIC_POOL)))
    lines = [f"{name} resident" if rng.random() < 0.4 else name for name in names]
    files: dict[str, str | None] = {"topics": "\n".join(lines) + "\n"}
    for number in range(rng.randint(0, 10)):
      topic = rng.choice(names)
      slug = f"{rng.choice(SLUG_WORDS)}-{number}"
      files[f"entries/{topic}/{slug}.md"] = entry(
          f"scope: {rng.choice(SCOPES)}",
          f"topic: {topic}",
          f"audience: {rng.choice(AUDIENCES)}",
          f"title: {rng.choice(TITLES)}",
          body=rng.choice(BODIES),
      )
    for number in range(rng.randint(0, 2)):
      files[f"staging/capture-{number}.md"] = f"# Capture {number}\n\nstaged\n"
    if rng.random() < 0.15:
      files[f"entries/{rng.choice(names)}/injected.md"] = rng.choice(INJECTED)
    stores.append(Store(f"random-{index:03d}", files))
  return stores


def materialize(root: pathlib.Path, files: dict[str, str | None]) -> None:
  """Writes a store's files under `root`; a None entry is a directory."""
  root.mkdir(parents=True)
  for relative, text in files.items():
    path = root / relative
    if text is None:
      path.mkdir(parents=True, exist_ok=True)
    else:
      path.parent.mkdir(parents=True, exist_ok=True)
      path.write_bytes(text.encode("utf-8"))


def selection_json(selection: memory.MemorySelection) -> dict:
  """The selection as the helper prints it: every field, each segment's sources with their own text."""
  return {
      "audience": selection.audience,
      "repo_basename": selection.repo_basename,
      "segments":
          [
              {
                  "delivery":
                      delivery,
                  "text":
                      text,
                  "sources":
                      [
                          {
                              "source_ref": source.source_ref,
                              "delivery": source.delivery,
                              "text": source.text,
                          } for source in sources
                      ],
              } for delivery, text, sources in selection.segments
          ],
      "usage_line": selection.usage_line,
      "text": selection.text,
  }


def python_answer(store_dir: pathlib.Path, request: dict) -> dict:
  """Python's answer to one request, in the helper's shape."""
  try:
    if request["audience"] == "master":
      selection = memory.select_master_memory(store_dir)
    else:
      selection = memory.select_worker_memory(store_dir, request["repo_basename"])
  except memory.MemoryFormatError as error:
    return {"error": str(error)}
  return {"selection": None if selection is None else selection_json(selection)}


def helper_answers(binary: pathlib.Path, requests: list[dict]) -> list[dict]:
  """The helper's answers to `requests`, one per request, in order."""
  payload = "".join(json.dumps(request, ensure_ascii=False) + "\n" for request in requests)
  result = subprocess.run([str(binary)], input=payload, capture_output=True, encoding="utf-8", check=False)
  if result.returncode != 0:
    sys.stderr.write(result.stderr)
    raise SystemExit(f"{binary} exited {result.returncode}")
  # str.splitlines would also split on U+2028 inside a text, so split on the newline alone.
  lines = [line for line in result.stdout.split("\n") if line]
  assert len(lines) == len(requests), f"{binary} answered {len(lines)} of {len(requests)} requests"
  return [json.loads(line) for line in lines]


def tree(root: pathlib.Path) -> dict[str, bytes | None]:
  """Every path under `root` with its bytes (None for a directory), `.git` excluded."""
  out: dict[str, bytes | None] = {}
  for path in sorted(root.rglob("*")):
    relative = path.relative_to(root).as_posix()
    if relative == ".git" or relative.startswith(".git/"):
      continue
    out[relative] = None if path.is_dir() else path.read_bytes()
  return out


def check_store(binary: pathlib.Path, work: pathlib.Path, index: int, store: Store, counts: dict[str,
                                                                                                 int]) -> list[str]:
  """Compares every audience's selection on one store, and returns the differences found."""
  store_dir = work / "stores" / f"{index:03d}-{store.name}" / "memory"
  materialize(store_dir, store.files)
  requests = [{"op": "select", "store": str(store_dir), "audience": "master"}]
  requests += [
      {
          "op": "select",
          "store": str(store_dir),
          "audience": "worker",
          "repo_basename": repo
      } for repo in REPO_BASENAMES
  ]
  expected = [python_answer(store_dir, request) for request in requests]
  shutil.rmtree(store_dir)
  materialize(store_dir, store.files)
  actual = helper_answers(binary, requests)
  differences = []
  for request, want, got in zip(requests, expected, actual, strict=True):
    if "error" in want:
      counts["format_errors"] += 1
    elif want["selection"] is None:
      counts["none"] += 1
    else:
      counts["selections"] += 1
      counts["segments"] += len(want["selection"]["segments"])
    if want != got:
      differences.append(
          f"{store.name} {request['audience']} {request.get('repo_basename', '')!r}:\n"
          f"  python: {json.dumps(want, ensure_ascii=False)}\n"
          f"  rust:   {json.dumps(got, ensure_ascii=False)}")
  return differences


def check_ensure(binary: pathlib.Path, work: pathlib.Path, index: int) -> list[str]:
  """Runs `ensure_store` twice on an empty directory on each side, and compares the trees after each call."""
  py_dir = work / "ensure" / f"{index}-python"
  rust_dir = work / "ensure" / f"{index}-rust"
  py_dir.mkdir(parents=True)
  rust_dir.mkdir(parents=True)
  differences = []
  for call in (1, 2):
    memory.ensure_store(py_dir)
    answer, = helper_answers(binary, [{"op": "ensure", "dir": str(rust_dir)}])
    if "error" in answer:
      raise SystemExit(f"ensure on call {call} failed in the helper: {answer['error']}")
    if not (py_dir / ".git").exists() or not (rust_dir / ".git").exists():
      differences.append(f"ensure call {call}: .git missing on one side")
    if tree(py_dir) != tree(rust_dir):
      differences.append(f"ensure call {call}: python {sorted(tree(py_dir))} != rust {sorted(tree(rust_dir))}")
  return differences


def main() -> int:
  binary = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_BINARY
  if not binary.is_file():
    raise SystemExit(f"{binary} is missing: build it with cargo-slot.sh build --release")
  rng = random.Random(SEED)
  stores = valid_stores() + format_error_stores() + random_stores(rng)
  counts = {"selections": 0, "segments": 0, "none": 0, "format_errors": 0}
  differences: list[str] = []
  work = pathlib.Path(tempfile.mkdtemp(prefix="memory-differential-"))
  try:
    for index, store in enumerate(stores):
      differences += check_store(binary, work, index, store, counts)
    for index in range(ENSURE_DIRS):
      differences += check_ensure(binary, work, index)
  finally:
    shutil.rmtree(work)
  for difference in differences:
    print(difference)
  print(f"command: uv run python rust/crates/features/memory/tests/differential.py {binary}")
  print(
      f"stores={len(stores)} requests={len(stores) * (1 + len(REPO_BASENAMES))} "
      f"selections={counts['selections']} segments={counts['segments']} none={counts['none']} "
      f"format_errors={counts['format_errors']} ensure_dirs={ENSURE_DIRS} differences={len(differences)}")
  return int(bool(differences))


if __name__ == "__main__":
  raise SystemExit(main())
