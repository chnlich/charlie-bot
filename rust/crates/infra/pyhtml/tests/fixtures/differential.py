"""Compare CPython's tokenizer with the built pyhtml-dump executable."""

from __future__ import annotations

import json
import html.parser
import random
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[6]

COUNT = 20_000
SEED = 20261019


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


def random_document(rng: random.Random) -> str:
  fragments = [
      "plain text",
      "CJK 中文",
      "😀é",
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


def main() -> int:
  binary = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "rust/target/release/pyhtml-dump"
  documents = random_documents(SEED, COUNT)
  input_jsonl = "".join(json.dumps(document["source"], ensure_ascii=True) + "\n" for document in documents)
  differences = 0

  for convert_charrefs in (False, True):
    mode = "convert_charrefs_true" if convert_charrefs else "convert_charrefs_false"
    argument = "--convert-charrefs" if convert_charrefs else "--no-convert-charrefs"
    try:
      result = subprocess.run(
          [str(binary), "--batch-jsonl", argument],
          input=input_jsonl,
          text=True,
          capture_output=True,
          check=True,
      )
    except subprocess.CalledProcessError as error:
      sys.stderr.write(error.stderr)
      raise
    actual_documents = [json.loads(line) for line in result.stdout.splitlines()]
    assert len(actual_documents) == COUNT, f"pyhtml-dump returned {len(actual_documents)} documents for {COUNT} inputs"

    for document, actual in zip(documents, actual_documents, strict=True):
      expected = tokenize(document["source"], convert_charrefs)
      if actual != expected:
        differences += 1
        print(
            json.dumps(
                {
                    "id": document["id"],
                    "mode": mode,
                    "source": document["source"],
                    "expected": expected,
                    "actual": actual,
                },
                ensure_ascii=False))

  print(f"documents={COUNT} modes=2 differences={differences}")
  return int(differences != 0)


if __name__ == "__main__":
  raise SystemExit(main())
