# charliebot-pyhtml

`charliebot-pyhtml` ports the CPython 3.14.7 HTML helpers used by artifact checks and plan diffs. Its library exposes the HTML tokenizer, named and numeric character-reference decoding, HTML escaping, and `SequenceMatcher` opcodes. The `pyhtml-dump` binary prints tokenizer events for a document read from standard input.

The tokenizer records handler arguments, Unicode line and column, UTF-8 byte offset, and raw start-tag text. Its `convert_charrefs` argument selects the same reference event behavior as `html.parser.HTMLParser`. The entity table in `src/html5_entities.json` was generated from `html.entities.html5`; `tests/fixtures/source.json` names the generator and its base commit.

The committed gzip fixtures record CPython 3.14.7 output, and Rust tests read them. `tests/fixtures/source.json` names the
helper and base commit for the entity and tokenizer fixtures. `tests/fixtures/differential.json.gz` records the seeded
documents from the randomized comparison in both `convert_charrefs` modes; `fixtures.rs` checks those outputs.
