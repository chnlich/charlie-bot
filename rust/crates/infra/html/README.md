# charliebot-html

`charliebot-html` ports the CPython 3.14.7 HTML helpers used by artifact checks and plan diffs. Its library exposes the HTML tokenizer, named and numeric character-reference decoding, HTML escaping, `SequenceMatcher` opcodes, and the `folds_at_open` page-open fold predicate that the artifact check and the cold-read visible-text extract share.

The tokenizer records handler arguments, Unicode line and column, UTF-8 byte offset, and raw start-tag text. Its `convert_charrefs` argument selects the same reference event behavior as `html.parser.HTMLParser`. The entity table in `src/html5_entities.json` was generated from `html.entities.html5`; `tests/fixtures/provenance.json` names the generator and its base commit.

The committed gzip fixtures record CPython 3.14.7 output, and Rust tests read them. `tests/fixtures/provenance.json` names the
helper and base commit for the entity and tokenizer fixtures. `tests/fixtures/differential_golden.json.gz` records the seeded
documents from the randomized comparison in both `convert_charrefs` modes; `fixtures.rs` checks those outputs.
