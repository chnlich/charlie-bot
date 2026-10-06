# Commits and PRs

Style for commit messages and pull request descriptions.

## Order

- Write a commit message body in three parts, in this order: why, evidence, how.
- Write a PR description in four sections, each under its own heading, in this order:
  1. Why
  2. How it works
  3. Evidence
  4. How the diff reads
- In a commit message, the how part follows the rules of the How the diff reads section.

## Why

- Open with the problem that the change removes, stated as its cost to the reviewer.
- Keep the summary line on the effect. Put the mechanism in the body.
- Say what the reviewer loses by skipping the change.

## How it works

- Describe how the changed system runs, in run order, before and after the change.
- Keep the section to one short paragraph.

## Evidence

- Quantify the problem and the improvement.
- For each number, name its source, so that the reviewer can rerun it.
- When a failing case exists, give it in full:
  - the input that triggers it
  - the observed cost
  - how often it occurs
- Name the past pattern that the change closes, so that the reviewer can recognize the next instance.
- Mark each unverified claim, and name the gate that settles it.

## How the diff reads

- Describe the approach in the order that the diff reads.
- Give each choice that is not obvious its reason in one clause.
- Say which parts change behavior and which parts only move code.

## Length and vocabulary

- The reviewer reads the description once, before the diff. Fit the description on one screen.
- Put a derivation, a glossary, or an operating guide in the code or the docs. Point to it from the description.
- Use the reviewer's terms.
- When the PR coins a term, define it in the docstring of the module that owns the term.
- Name that docstring before the description uses the term.
- Name a run by its measurement and its rerun command. Run identifiers, codenames, and hostnames resolve only
  for their author, and they date the text.
