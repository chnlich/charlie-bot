# Improve direction: dead code

You run the dead code direction of the improve loop.
Follow `tools/improve/landing.md` for the round protocol.
That file carries the steps that every direction shares.
Your branch prefix is `improve/dead_code/`.

## Scope

- `rust/`
- `web/`

Delete a public item, code or feature whose callers do not exist.
The tree then carries only code that something uses.
A compatibility layer in `rust/` that mirrors Python behavior is dead when no caller uses it.

## Topics

Take evidence from these tools:

- `cargo metadata` for the list of crates that a grep must cover
- the compiler and clippy output for a private item that nothing calls
- `git grep` for a public item, with every string reference checked by hand
- `git grep` for the feature name, including prompts, skills, configs and docs

Check each string reference yourself, because the compiler cannot see it.
A string reference reaches the symbol, so one live match keeps the item.

Take one item or one feature as the topic.
One topic per round keeps the review focused.
A topic whose deletion needs a new consumer is a design change.
Name such a topic in the final message and open no pull request.
The round then changes a design, and a design needs the user's decision.

## Search scope

Search the sources named under Scope.
When a removed name may appear in the repository's prompts, skills, configs and docs, search those files too.
Never search a path outside this repository, including the home directory.
A search of the whole home directory read about 36 GB and slowed a round's startup.

## Evidence

Write the command and its output that show no caller remains for the deleted name.
Add the grep output for each name that the change removes.
Let CI run the full suite, because a green run proves that the deletion broke nothing.
The evidence shows the reviewer that the deletion is safe.
