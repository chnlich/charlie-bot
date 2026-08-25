# Commits and PRs

Style for commit messages and pull request descriptions.

## Order

Why, then evidence, then how. A reviewer decides how much attention to spend
before opening the diff, so the reason and its size arrive first; the
mechanism reads faster once the motive is known.

## Why

- Open with the problem the change removes, stated as its cost to the reader.
- Keep the summary line on the effect, and leave the mechanism to the body.
- Say what a reader loses by skipping the change.

## Evidence

- Quantify both the problem and the improvement, and name where each number
  came from so a reviewer can rerun it.
- Give the failing case in full when one exists: the input that triggers it,
  the observed cost, and how often it lands.
- Name the past pattern the change closes, so a reviewer can recognize the
  next instance.
- Keep unverified claims marked, and name the gate that settles each one.

## How

- Describe the approach after the evidence, in the order the diff reads.
- Give each non-obvious choice its reason in one clause.
- Say which parts change behavior and which only move code.

## Length and vocabulary

- The description is read before the diff and once; it fits on one screen. A derivation, a glossary, or an
  operating guide the reader will need again lives in the code or the docs, and the description points there.
- Use the reader's terms. A term the PR coins is defined in the module docstring it belongs to, and the
  description uses it only after naming that home.
- Evidence names the measurement and where to rerun it; run identifiers, codenames, and hostnames stay out of
  the prose, since a reviewer cannot resolve them and they date the text.
