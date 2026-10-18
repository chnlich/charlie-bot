"""Generate Rust fixtures from the CPython implementation in this worktree."""

from __future__ import annotations

import difflib
import gzip
import html
import html.entities
import html.parser
import itertools
import json
import random
import re
import sys
from pathlib import Path

from src.features.artifacts import plan_diff

ROOT = Path(__file__).resolve().parents[6]
FIXTURES = Path(__file__).resolve().parent
ENTITY_OUTPUT = Path(__file__).resolve().parents[2] / "src" / "html5_entities.json"
VERSION = sys.version.split()[0]
SEED = 20261018


def assert_reference_is_from_worktree() -> None:
  module_path = Path(plan_diff.__file__).resolve()
  assert module_path.is_relative_to(ROOT), f"plan_diff imported outside worktree: {module_path}"


class EventParser(html.parser.HTMLParser):

  def __init__(self, source: str, convert_charrefs: bool) -> None:
    super().__init__(convert_charrefs=convert_charrefs)
    self.source = source
    self.line_starts = [0]
    self.line_starts.extend(match.end() for match in re.finditer("\\n", source))
    self.events: list[dict] = []

  def record(self, kind: str, *args: object, starttag_text: str | None = None) -> None:
    line, column = self.getpos()
    char_offset = self.line_starts[line - 1] + column
    byte_offset = len(self.source[:char_offset].encode("utf-8"))
    values = list(args)
    if kind in {"start_tag", "start_end_tag"}:
      values[1] = [list(attribute) for attribute in values[1]]
    event = {
        "kind": kind,
        "args": values,
        "position": {
            "line": line,
            "column": column
        },
        "byte_offset": byte_offset,
    }
    if starttag_text is not None:
      event["starttag_text"] = starttag_text
    self.events.append(event)

  def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
    self.record("start_tag", tag, attrs, starttag_text=self.get_starttag_text())

  def handle_startendtag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
    self.record("start_end_tag", tag, attrs, starttag_text=self.get_starttag_text())

  def handle_endtag(self, tag: str) -> None:
    self.record("end_tag", tag)

  def handle_data(self, data: str) -> None:
    self.record("data", data)

  def handle_entityref(self, name: str) -> None:
    self.record("entity_reference", name)

  def handle_charref(self, name: str) -> None:
    self.record("character_reference", name)

  def handle_comment(self, data: str) -> None:
    self.record("comment", data)

  def handle_decl(self, decl: str) -> None:
    self.record("declaration", decl)

  def handle_pi(self, data: str) -> None:
    self.record("processing_instruction", data)

  def unknown_decl(self, data: str) -> None:
    self.record("unknown_declaration", data)


def tokenize(source: str, convert_charrefs: bool) -> list[dict]:
  parser = EventParser(source, convert_charrefs)
  parser.feed(source)
  parser.close()
  return parser.events


def html_documents() -> list[dict[str, str]]:
  paths = set(Path("prompts").rglob("*.html"))
  paths.update(Path("web/templates").rglob("*.html"))
  paths.update(path for path in Path("src").rglob("*.html") if "templates" in path.parts)
  return [
      {
          "id": f"html:{path.as_posix()}",
          "part": "html_file",
          "source": path.read_text(encoding="utf-8")
      } for path in sorted(paths)
  ]


def edge_documents() -> list[dict[str, str]]:
  cases = {
      "unclosed-tags": "<div><p>open",
      "stray-less-than": "before < after",
      "end-tag-without-name": "before </ after",
      "attribute-values": "<x missing empty= quoted='single' double=\"double\" bare=value equals=><!>",
      "bare-and-references": "& &amp; &amp &notit; &#65; &#65 &#x41; &#x41 &unknown;",
      "numeric-reference-limits": "&#0; &#11; &#127; &#128; &#129; &#xD800; &#1114112; &#999999999999999999999999;",
      "comment-abrupt-close": "<!-->tail<!--->tail<!--body--!>tail",
      "doctype-and-cdata": "<!DOCTYPE><!DOCTYPE html [ <!ENTITY x 'y'> ]><![CDATA[<>&]]>",
      "processing-instructions": "<?target data?><p><? spaced >",
      "raw-text-bodies": "<script>one </ two <!-- x &amp; </script><style>two </x <!--y</style>",
      "rcdata-bodies": "<textarea>A &amp; &#65; </x </textarea><title>&notit;</title>",
      "line-endings-and-unicode": "<p>一\r\n\t😀e\u0301 &amp; end</p>\r\n",
      "malformed-start-and-end": "<a x=one/y z='unterminated>tail</a/ > </> </x y>",
      "markup-edge-cases": "<![CDATA[unfinished <!- <!-- unfinished <?unfinished <!doctype unfinished",
      "raw-text-close-boundaries": "<script> </scriptx> </script / > end</script>",
  }
  return [{"id": f"edge:{name}", "part": "edge_case", "source": source} for name, source in cases.items()]


def random_document(rng: random.Random) -> str:
  fragments = [
      "plain text",
      "CJK 中文",
      "😀e\u0301",
      "&",
      "&amp;",
      "&amp",
      "&notit;",
      "&#65;",
      "&#x41",
      "&#1114112;",
      "<",
      "</",
      "<p>",
      "</p>",
      "<x a b='v' c=\"w\" d=z>",
      "<x a=one/y>",
      "<x/>",
      "<!--comment--!>",
      "<!DOCTYPE html>",
      "<![CDATA[x<y]]>",
      "<?p data?>",
      "<script>raw </ <!-- &amp; </script>",
      "<style>raw </ <!-- &amp; </style>",
      "<textarea>raw &amp; </textarea>",
      "\r\n\t",
      "</>tail",
      "<!bad>tail",
  ]
  return "".join(rng.choice(fragments) for _ in range(rng.randint(1, 18)))


def random_documents(seed: int, count: int) -> list[dict[str, str]]:
  rng = random.Random(seed)
  return [
      {
          "id": f"random-{index:04d}",
          "part": "random_document",
          "source": random_document(rng)
      } for index in range(count)
  ]


def opcode_inputs() -> list[dict[str, object]]:
  rng = random.Random(SEED + 1)
  vocabulary = ["alpha", "beta", "gamma", "delta", "一", "二", "😀", "token_1"]
  inputs: list[dict[str, object]] = []
  for index in range(300):
    left = [rng.choice(vocabulary) for _ in range(rng.randint(0, 48))]
    right = [rng.choice(vocabulary) for _ in range(rng.randint(0, 48))]
    inputs.append({"id": f"random-opcodes-{index:03d}", "a": left, "b": right})

  templates = sorted(Path("prompts").glob("*_template.html"))
  assert len(templates) == 5, f"expected five prompt templates, found {len(templates)}"
  token_lists = {
      path: [value for value, _, _ in plan_diff._tokenise(path.read_text(encoding="utf-8"))] for path in templates
  }
  for left, right in itertools.combinations(templates, 2):
    inputs.append({
        "id": f"template-pair:{left.stem}:{right.stem}",
        "a": token_lists[left],
        "b": token_lists[right],
    })
  return inputs


def opcode_result(a: list[str], b: list[str]) -> list[dict[str, object]]:
  result = difflib.SequenceMatcher(None, a, b, autojunk=False).get_opcodes()
  return [
      {
          "tag": tag,
          "a_start": a_start,
          "a_end": a_end,
          "b_start": b_start,
          "b_end": b_end
      } for tag, a_start, a_end, b_start, b_end in result
  ]


def reference_fixtures(documents: list[dict[str, str]], opcodes: list[dict[str, object]]) -> dict[str, object]:
  parser_results = []
  for document in documents:
    source = document["source"]
    parser_results.append(
        {
            "id": document["id"],
            "convert_charrefs_false": tokenize(source, convert_charrefs=False),
            "convert_charrefs_true": tokenize(source, convert_charrefs=True),
        })

  entity_inputs = [f"&{name}" for name in html.entities.html5]
  entity_inputs.extend(
      [
          "&unknown;",
          "&unknown",
          "&notit;",
          "&ampx",
          "&#0;",
          "&#11;",
          "&#127;",
          "&#128;",
          "&#129;",
          "&#xD800;",
          "&#1114112;",
          "&#x110000;",
          "&#999999999999999999999999999999999999;",
          "&#xFFFFFFFFFFFFFFFF;",
          "&#12g",
          "&#xAg",
          "&#;",
          "&#x;",
          "x &amp; y &notit; z",
      ])
  entity_inputs = list(dict.fromkeys(entity_inputs))
  escape_inputs = ["", "plain", "&<>", "'\"", "<😀&e\u0301>", "\u0000\u007f\u00a0"]
  return {
      "cpython_version": VERSION,
      "documents": parser_results,
      "unescape": [{
          "input": value,
          "expected": html.unescape(value)
      } for value in entity_inputs],
      "escape":
          [
              {
                  "input": value,
                  "quote": quote,
                  "expected": html.escape(value, quote=quote)
              } for value in escape_inputs for quote in (False, True)
          ],
      "opcodes": [{
          "id": item["id"],
          "expected": opcode_result(item["a"], item["b"])
      } for item in opcodes],
  }


def write_json(path: Path, value: object) -> None:
  path.write_text(json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n", encoding="utf-8")


def write_json_gzip(path: Path, value: object) -> None:
  encoded = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8") + b"\n"
  path.write_bytes(gzip.compress(encoded, mtime=0))


def main() -> None:
  assert_reference_is_from_worktree()
  documents = html_documents() + edge_documents() + random_documents(SEED, 500)
  opcodes = opcode_inputs()
  corpus = {
      "cpython_version": VERSION,
      "seed": SEED,
      "documents": documents,
      "opcodes": opcodes,
  }
  expected = reference_fixtures(documents, opcodes)
  write_json(ENTITY_OUTPUT, dict(sorted(html.entities.html5.items())))
  write_json_gzip(FIXTURES / "corpus.json.gz", corpus)
  write_json_gzip(FIXTURES / "expected.json.gz", expected)
  total = sum(
      path.stat().st_size for path in (ENTITY_OUTPUT, FIXTURES / "corpus.json.gz", FIXTURES / "expected.json.gz"))
  print(f"CPython {VERSION}: {len(documents)} documents, {len(opcodes)} opcode pairs, {total} fixture bytes")


if __name__ == "__main__":
  main()
