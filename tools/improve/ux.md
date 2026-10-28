# Improve direction: UX

You run the UX direction of the improve loop.
Follow `tools/improve/landing.md` for the round protocol.
That file carries the steps that every direction shares.
Your branch prefix is `improve/ux/`.

## Scope

- `web/`
- the `static/` and `templates/` directories of the features under `src/`
- the endpoints that those files call

A change inside this scope may touch the endpoint that the page calls.
A change to the endpoint alone belongs to the performance direction.

## Topics

Take a UI problem that the user named in a session record.
Search the records with `charliebot session dialog | rg -i -C3 '<term>'`.
Take a defect that the before screenshot shows at either width.
Both sources show a problem that the user feels.

Use 1440 px as the desktop width and 390 px as the phone width.
The two widths stand for the desktop window and the phone screen.

## Measurement

Serve one tree with `scripts/release/trial.sh --serve <commit> <server binary> -- <command>`.
The command reads `CHARLIEBOT_SERVE_URL`, `CHARLIEBOT_SERVE_AUTH_HEADER` and `CHARLIEBOT_SERVE_PID`.
The environment names the isolated server, so the command reaches no other server.

For a change to web files only, use the live binary `~/charliebot-releases/current/charliebot-server` for both sides:
serve `origin/main` for the main tree, then the branch head for the branch tree.
The two trees under one binary differ only in the change under test.

For any other change, use main's CI artifact `charliebot-server-<main sha>` for the main tree
and the pull request's artifact `charliebot-server-pr-<head sha>` for the branch tree.
Serve each artifact at the commit in its name, so the binary and the tree match.
The pull request's own binary carries the change under test.

Take the screenshots with the playwright chromium under `~/.cache/ms-playwright`.
Take one screenshot per width on each side: 1440 px and 390 px before the change, and the same two widths after it.
The pair at each width shows the change to the user.

## Evidence

Write in the pull request's `## Evidence` section what the screenshots show, in words: for each width, the defect that the before screenshot shows and the state that the after screenshot shows.
The four screenshots stay in the round's evidence directory from the landing protocol; their links go only into the final message.
The reviewer reads the words and reaches the screenshots through the final message.
