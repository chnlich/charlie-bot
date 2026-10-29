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
Each direction keeps its own kind of change, so the review stays with one owner.

## Topics

Take a defect that the before screenshot shows at either width, on the mock home described below.
Use 1440 px as the desktop width and 390 px as the phone width.
The two widths stand for the desktop window and the phone screen.
The mock home has the production number of sessions, so the pages show the layout that a user sees.

## Mock home

Generate the mock home only when `~/improve/mock-home/<blob>/` is missing.
The `<blob>` is `git rev-parse <commit>:tools/improve/mock-home.py`, for the branch commit.
Run `python3 -I tools/improve/mock-home.py generate <blob dir>` on an empty directory to create it.
The fixed data keeps the before and after servers on the same bytes, so the two sides compare like with like.

## Servers

For a change to web files only, build one server from `origin/main` on the round's host with `cargo build --release --locked -p charliebot-server -p charliebot-cli` in `rust/`.
Serve the `origin/main` tree and the branch tree with that one server.
The server binary then stays equal, so the two trees differ only in the change under test.

For any other change, build the before server from an export of `origin/main`, and the after server from the branch.
Both use the same release profile on one machine, so the change under test is the only difference between them.

Start each side with `scripts/release/trial.sh --serve --tree-root /tmp/improve-ux/ --seed-home <blob dir> --in-caller-scope <commit> <server binary> -- <command>`.
The tree root keeps the two trees apart, and the seed gives both servers the same data.
Create the tree root first: the preflight refuses a missing one, so a round never writes outside its own directory.

Run `python3 -I tools/improve/mock-home.py check <blob dir> "$CHARLIEBOT_SERVE_URL"` first in `<command>`.
Stop the round when it exits 1, because that server reads data other than the mock home.
The trial exports `CHARLIEBOT_SERVE_URL`, `CHARLIEBOT_SERVE_AUTH_HEADER` and `CHARLIEBOT_SERVE_PID` to `<command>`, so the screenshots and the requests name this trial's server and no other.

## Screenshots

Take the screenshots with the playwright chromium under `~/.cache/ms-playwright`.
Send `CHARLIEBOT_SERVE_AUTH_HEADER` with each page request, so that the pages load with the access key.
Take one screenshot per width on each side: 1440 px and 390 px before the change, and the same two widths after it.
The pair at each width shows the change to the user.

## Evidence

Write in the pull request's `## Evidence` section what the screenshots show, in words.
For each width, name the defect that the before screenshot shows and the state that the after screenshot shows.
Both sides come from one round on one server and one mock home, so the pair at each width shows only the change.
The four screenshots stay in the round's evidence directory from the landing protocol; their links go only into the final message.
The reviewer judges the change from the words, so the words must say what each screenshot shows.
