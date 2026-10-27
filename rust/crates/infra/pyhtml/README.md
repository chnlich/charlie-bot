# charliebot-pyhtml

`charliebot-pyhtml` ports the CPython 3.14.7 HTML helpers used by artifact checks and plan diffs. Its library exposes the HTML tokenizer, named and numeric character-reference decoding, HTML escaping, and `SequenceMatcher` opcodes. The `pyhtml-dump` binary prints tokenizer events for a document read from standard input.

The tokenizer records handler arguments, Unicode line and column, UTF-8 byte offset, and raw start-tag text. Its `convert_charrefs` argument selects the same reference event behavior as `html.parser.HTMLParser`. The entity table is generated from `html.entities.html5` by `tests/fixtures/generate_fixtures.py` and stored in `src/html5_entities.json`.

The committed gzip-compressed JSON fixture corpus and expected results come from CPython 3.14.7. Regenerate them from the repository root with:

```sh
PYTHONPATH=$PWD uv run python rust/crates/infra/pyhtml/tests/fixtures/generate_fixtures.py
```

Run the extended seeded parser comparison after building `pyhtml-dump` with:

```sh
uv run python rust/crates/infra/pyhtml/tests/fixtures/differential.py
```
