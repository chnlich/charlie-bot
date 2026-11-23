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

Choose a defect at a required desktop or phone width on the shared mock home. The mock home has the production number
of sessions, so the pages show representative content and layout. Use the widths in the Web UI Screenshots rule in
`prompts/task_base.md`, and apply the screen-space rule in `AGENTS.md` to each chosen defect.

A defect is one of two kinds:

- a layout defect that the before screenshot shows;
- a click defect, as the Clicks section defines.

## Clicks

The round clicks each control on the screen it examines.

The round records each click's result in the evidence. A result names whether the click gives a visible effect on that
screen (a state change, a message or a navigation), and the non-GET requests the click issues.

A control that the mock home cannot exercise, or that the screen shows as disabled, is recorded as not clicked, with
its reason.

A click defect is a control whose click has one of these results:

- the screen shows no change, or the change falls where the user cannot see it;
- the screen changes but does not show what the control did;
- a write that the screen shows as pending or sent never reaches the session, including when the user leaves the
  screen before the control's delay ends.

A click that the control's design leaves without effect, such as a send with no text, is not a click defect.
For a control that starts a delayed write, the round also clicks it and leaves the screen during the delay.

The clicks run only against a serve trial on the shared mock home.
Every non-GET request a click issues is intercepted and recorded, so no click writes to a real session.
The intercept may answer a recorded write locally, so the screen shows what it shows after a successful write.

## Mock home

The runner supplies the complete shared mock home before the round, and the round names the DATA_VERSION of that home.
Both sides read the same data, and the round does not change it. This keeps the before and after views comparable.
The shared mock home binds scheduled tasks to some of its manager sessions, so a served round shows cron-bound session
pages and their Task & context dialogs.

## Servers

The before and after servers use the same release profile on the same host. For a web-only change, one release binary
built from `origin/main` serves both the `origin/main` tree and the branch tree, so the change under test is the only
difference between the sides. For a change that affects server code, the before server represents `origin/main` and the
after server represents the branch. The intended change is the only source difference between the sides.

## Serving

Each side runs one serve trial against the shared mock home, through the `dev/improve/trial-side.sh` entry. Builds stay
outside the bench lock. The trial exports `CHARLIEBOT_SERVE_URL`, `CHARLIEBOT_SERVE_AUTH_HEADER` and
`CHARLIEBOT_SERVE_PID` to its measurement command. All requests and process reads use that trial's server.

The side's report decides whether the side holds the bench lock:

- A side whose evidence is screenshots and clicks runs with the bench lock off. The entry runs with
  `IMPROVE_TRIAL_USE_LOCK=0`.
  - Screenshots and clicks do not depend on a quiet host.
  - The bench's scopes keep CPU priority over the improve scopes, so the side does not take the bench's CPU.
  - The side's evidence records the bench-lock holder (the process that holds the lock, or `none`) at the side's start
    and at its end.
- A side that reports any timing or byte-over-time measurement holds the bench lock from trial startup through its
  measurements and trial shutdown. It keeps the default of `IMPROVE_TRIAL_USE_LOCK`.
  - A timing taken beside a bench measures that bench's CPU contention.

A side that holds the bench lock waits at most 15 minutes for it. If the wait expires, that side takes no screenshot or
measurement. The round reports no result from an incomplete pair. It stops without opening a pull request and names the
held bench lock in its stop reason. The next scheduled fire tries again.

Each side's server passes `mock-home check` before that side takes any screenshot or measurement. Exit 1 means the
server reads data outside the shared mock home, and it stops the round before any measurement.

## Screenshots

For each width that the Web UI Screenshots rule in `prompts/task_base.md` names, the round takes a before and an after
screenshot of each state the change alters, and an after screenshot of each state the change adds. Each pair shows the
same view before and after the change, so the reader can see the user-facing difference.

## Evidence

Evidence names the defect visible before and the resulting state after at each required width. For a click defect, the
before evidence shows the screen before and after the click, with the recorded click result that shows no effect. The
screenshots stay in the round's evidence directory, and landing Step 9 carries them to the user, so the reviewer and
user can assess the change without rerunning it.
A side that reports a timing or a byte-over-time measurement keeps its raw output in the side output location, and the
pull request cites its side record, as Step 5 of `dev/improve/landing.md` sets.
