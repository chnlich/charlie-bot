# charliebot-html

`charliebot-html` provides the HTML helpers used by artifact checks and plan diffs. Its library exposes the HTML tokenizer, named and numeric character-reference decoding, HTML escaping, sequence-diff opcodes, and the `folds_at_open` page-open fold predicate that the artifact check and the cold-read visible-text extract share.

The tokenizer records handler arguments, Unicode line and column, UTF-8 byte offset, and raw start-tag text. Its `convert_charrefs` argument selects whether character references resolve into text or arrive as separate events. The entity table in `src/html5_entities.json` holds the HTML5 named references.
