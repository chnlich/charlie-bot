# Improve direction: decoupling

You run the decoupling direction of the improve loop.
Follow `$IMPROVE_KIT_DIR/dev/improve/landing.md` for the round protocol.
That file carries the steps that every direction shares.
Your branch prefix is `improve/decoupling/`.

## Scope

- the dependencies between two crates under `rust/`
- the responsibilities inside one crate under `rust/`
- a user-setting finding, where the fix moves the setting to the single record that owns it

landing.md's moved-code rule covers the references outside this scope.
A change to a web page takes its screenshots and serve trial from landing.md's Web page evidence section.

Change a dependency edge or move code to the crate that owns it.
A change to one function's behavior belongs to another direction.
A configuration-in-code finding may sit in any tracked file that runs: `rust/`, `web/`, `dev/` and `scripts/`.
When the finding sits under `dev/improve/`, name it in the final message and open no pull request.
Those files launch, confine and review this round, so a change to them needs the user's decision.

## Topics

Take evidence from these tools:

- `cargo metadata` for the dependency graph
- the compiler and clippy output for an unused or tangled dependency
- `git grep` for the same name spelled in two places

Take one finding as the topic.
A finding is one of these:

- a dependency that no code needs
- a name that two crates spell for one concept
- a crate with two responsibilities
- a user setting that two stores write, or that two edit entries write to different stores
- a value in code that the user's configuration owns
- a responsibility that two or more modules carry when one module already owns it, for example a write to one file or
  record; the fix routes every site through the owner.

An edit entry is a form or an API endpoint that writes a user setting.
Two edit entries that write the same record are one store and raise no finding.
For example, the session header title and the menu Rename both write the session record.

The user's configuration owns each value that chooses what runs: a backend, pool or option id, a model id, an API base
or gateway URL, a credential name, and the harness that a call runs on.
CharlieBot's general keys for these choices sit under `backends` in `config.yaml`, and
`configs/config.example.yaml` shows their form.
Code reads each such value through its key, and a script reads it through `charliebot backend resolve`.
These raise no finding:
- a test
- a mock or fixture
- the example configuration under `configs/`
- a code comment, which describes a choice and makes none
- a backend crate's own harness binary, argv and usage kind, in that crate; another backend's name still raises one

In a configuration-in-code finding, name the value and each code site that writes it, and the configuration key
that owns it.

In a user-setting finding, name:

- the setting
- each store
- each edit entry with its handler

In a scattered-ownership finding, name the responsibility, each module that carries it, and the module that owns it.

One finding per round keeps the review focused.

A finding that gives a shared name one owner — a file or directory name, a field or key name, a process or executable
name, or any other literal that the owner rule of `rust/crates/app/structure/tests/structure.rs` can match — also
registers the name with that rule in the same pull request. The structure test then fails a later change that spells
the name outside its owner. The rule covers the event names, the `CHARLIEBOT_` environment variable names and the
`/api/` route paths without a table, so those need no row; rust/README.md states which kinds the rule reads from code
and which it reads from its tables.

## Evidence

Count the dependency edges or the duplicate spellings before and after the change.
For a user-setting finding, count its stores and edit entries (write sites) before and after the change.
For a configuration-in-code finding, count the code sites that write the value before and after the change, and name
the key that supplies it after.
For a scattered-ownership finding, count the sites that carry the responsibility before and after the change.
For a finding that gives a name one owner, the count is the spellings of the name before and after the change, and the
Evidence also states that the owner rule passes with the name's new row and that the row's name appears in the pull
request's diff.
Write the counts in the pull request's `## Evidence` section.
Let CI run the structure tests and the full suite, because a green run proves that the layout still holds.
The count shows the reviewer that the coupling fell.
