# Improve direction: UX

You run the UX direction of the improve loop.
Follow `dev/improve/landing.md` for the round protocol.
That file carries the shared round contract.
Your branch prefix is `improve/ux/`.

## Scope

Keep UX changes in these areas:

- `web/`
- the `static/` and `templates/` directories of features under `web/features/`
- endpoints those files call

A page change may include its endpoint. An endpoint-only change belongs to the performance direction. Each direction
keeps one category of change so the review has one scope owner.

## Topics

Choose a defect visible in the before screenshot on the shared mock home at a required desktop or phone width. The
mock home has the production number of sessions, so the pages show a representative layout. Use the widths in the
Web UI Screenshots rule in `prompts/task_base.md`, and apply the screen-space rule in `AGENTS.md` to each chosen defect.

## Mock home

The runner supplies the complete shared mock home before the round, and the round names the DATA_VERSION of that home.
Both sides read the same data, and the round does not change it. This keeps the before and after views comparable.

## Servers

The before and after servers use the same release profile on the same host. For a web-only change, one release binary
built from `origin/main` serves both the `origin/main` tree and the branch tree, so the change under test is the only
difference between the sides. For a change that affects server code, the before server represents `origin/main` and the
after server represents the branch. The intended change is the only source difference between the sides.

## Serving

Each side runs one serve trial against the shared mock home. The `dev/improve/trial-side.sh` entry holds the host's
bench lock from trial startup through screenshots or measurements and trial shutdown. Builds stay outside the lock.
The trial exports `CHARLIEBOT_SERVE_URL`, `CHARLIEBOT_SERVE_AUTH_HEADER` and `CHARLIEBOT_SERVE_PID` to its measurement
command; all requests and process reads use that trial's server.

Each side waits at most 15 minutes for the bench lock. If the wait expires, that side takes no screenshot or
measurement. The round reports no result from an incomplete pair, stops without opening a pull request and names the
held bench lock in its stop reason. The next scheduled fire tries again.

Each side's server passes `mock-home check` before that side takes any screenshot or measurement. Exit 1 means the
server reads data outside the shared mock home, and it stops the round before any measurement.

## Screenshots

For each width that the Web UI Screenshots rule in `prompts/task_base.md` names, the round takes a before and an after
screenshot of each state the change alters, and an after screenshot of each state the change adds. Each pair shows the
same view before and after the change, so the reader can see the user-facing difference.

## Evidence

Evidence names the defect visible before and the resulting state after at each required width. The screenshots stay in
the round's evidence directory, and landing Step 9 carries them to the user, so the reviewer and user can assess the
change without rerunning it.
