"""Compare CPython's tokenizer with the built pyhtml-dump executable."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

from generate_fixtures import ROOT, assert_reference_is_from_worktree, random_documents, tokenize

COUNT = 20_000
SEED = 20261019


def main() -> int:
  assert_reference_is_from_worktree()
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
