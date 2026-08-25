---
name: writing-style
description: Genre style packs for code, READMEs, commits and PRs,
  bug reports, article shares, and coordination messages. Load the matching
  genre file at its writing moment. General prose rules live in the master
  prompt's Writing Style section.
version: 2.2.0
---

# Writing Style

Genre packs, one file per genre.

## One fact, one surface

Each fact about a piece of code has one home; every other surface refers to it.

- Comment: what the next editor must keep true at this line.
- Docstring: what the thing is and what a caller relies on.
- Module docstring: the notation, equations, and glossary of the module's private vocabulary, once.
- Configuration comment: what the operator changes and what they will observe.
- README or design doc: the shape of the system and how to operate it.
- Commit message and PR description: why the change exists, the evidence, and how the diff reads.

## Code

See [genres/code.md](genres/code.md).

## READMEs

See [genres/readme.md](genres/readme.md).

## Commits and PRs

See [genres/commits-and-prs.md](genres/commits-and-prs.md).

## Bug reports

See [genres/bug-report.md](genres/bug-report.md).

## Sharing articles

See [genres/sharing-articles.md](genres/sharing-articles.md).

## Coordination messages

See [genres/coordination-messages.md](genres/coordination-messages.md).
