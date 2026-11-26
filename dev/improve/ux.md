# Improve direction: UX

You run the UX direction of the improve loop.
Follow `$IMPROVE_KIT_DIR/dev/improve/landing.md` for the round protocol.
That file carries the shared round contract.
Your branch prefix is `improve/ux/`.

## Scope

Keep UX changes in these areas:

- `web/`
- the `static/` and `templates/` directories of features under `web/features/`
- endpoints those files call

landing.md's moved-code rule covers the references outside this scope.

A page change may include its endpoint. An endpoint-only change belongs to the performance direction. Each direction
keeps one category of change so the review has one scope owner.

## Topics

Choose a defect at a required desktop or phone width on the shared mock home. The mock home has the production number
of sessions, so the pages show representative content and layout. Use the widths in the Web UI Screenshots rule in
`prompts/task_base.md`, and apply the screen-space rule in `AGENTS.md` to each chosen defect.

A defect is one of two kinds:

- a layout defect that the before screenshot shows;
- a click defect, as the Clicks section defines.

## Input profile

Every capture and click runs under the input profile of its window size, which `dev/improve/ux-input-profile.mjs`
gives: a desktop size reports a fine pointer that hovers, and the phone size reports touch with no hover. The driver's
browser starts with `INPUT_BROWSER_ARGS` from that module, and the driver checks each size's profile on the loaded page
before its first capture at that size. A page whose hover and pointer answers differ from the profile is not captured.

## Clicks

The round clicks each control on the screen it examines.

Every click a round driver makes goes through the click helper, `dev/improve/ux-click-target.mjs` (`clickTarget`). The
helper resolves the control's selector at the moment of each click, so a click never lands on a position that an earlier
load or list recorded. The helper refuses a click in these cases:

- no box with a nonzero size appears within the wait that the driver states;
- the center of the box lies outside the viewport;
- another element covers the center of the box.

Each refusal names the selector and the reason, and a covering refusal also names the covering element.

A refused click is a driver result, and the evidence records it as one. A refused click is never a click defect by itself.

The round records each click's result in the evidence. A result names whether the click gives a visible effect on that
screen (a state change, a message or a navigation), and the non-GET requests and the WebSocket frames the click sends.

A control that the mock home cannot exercise, or that the screen shows as disabled, is recorded as not clicked, with
its reason.

A click defect is a control whose click has one of these results:

- the screen shows no change, or the change falls where the user cannot see it;
- the screen changes but does not show what the control did;
- a write that the screen shows as pending or sent never reaches the session, including when the user leaves the
  screen before the control's delay ends.

A click that the control's design leaves without effect, such as a send with no text, is not a click defect.
For a control that starts a delayed write, the round also clicks it and leaves the screen during the delay.

The clicks run only against a serve trial on the shared mock home. Each write a click makes lands in that trial copy,
which has its own home, port and process tree, so the screen shows what it shows after a write there. Two rules decide
which writes start work, and each names what enforces it:

- A WebSocket write is dropped at send time when the driver installs the kit's frame guard
  (`dev/improve/ux-frame-guard.mjs`, `installWriteGuard`) before the page's first navigation. The kit does not launch
  the driver, so the install is the driver's duty. With the guard installed, every frame a page sends is dropped except
  a session socket's `subscribe` frame, so chat input and terminal input do not reach the server. Server frames and the
  page's handlers keep receiving, so the page's reads keep flowing. The guard records each dropped frame on the page
  (`globalThis.__uxWriteGuard.dropped`), and the click result names those frames.
- An HTTP write is one non-GET request. The round's driver intercepts each one a click issues and records it in the
  evidence. The kit does not check that intercept.

The kit does not keep a terminal socket from opening. A terminal connection starts a tmux session and a login shell in
the trial's own tmux server. The trial stops that server at teardown and fails the side when a process of its tree
remains. A seeded cron task is disabled before the trial's server starts and read back as disabled after it. The
scheduler still binds a session to each such task, and the mock-home check counts those sessions.

A WebSocket is not answered with a stub, because a stub that stops the socket's reads freezes the page under test.

## Mock home

The runner supplies the complete shared mock home before the round, and the round names the DATA_VERSION of that home.
Both sides read the same data, and the round does not change it. This keeps the before and after views comparable.
The shared mock home binds scheduled tasks to some of its manager sessions, so a served round shows cron-bound session
pages and their Task & context dialogs. Its sessions are task-tree nodes with a boot marker, so a serve trial's start
logs no node failure. Each default cron task that the trial seeds adds the session it binds, so a served round's
task tree holds the home's sessions and those. The sidebar lists only some roots of that tree.

## Servers

The before and after servers use the same release profile on the same host. For a web-only change, one release binary
built from `origin/main` serves both the `origin/main` tree and the branch tree, so the change under test is the only
difference between the sides. For a change that affects server code, the before server represents `origin/main` and the
after server represents the branch. The intended change is the only source difference between the sides.

## Serving

Each side runs one serve trial against the shared mock home, through the `$IMPROVE_KIT_DIR/dev/improve/trial-side.sh` entry. Builds stay
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
The screenshots and the clicks start the browser that `$IMPROVE_BROWSER` names (`landing.md`, The round's browser).

## Evidence

Evidence names the defect visible before and the resulting state after at each required width. For a click defect, the
before evidence shows the screen before and after the click, with the recorded click result that shows no effect. The
screenshots stay in the round's evidence directory, and landing Step 9 carries them to the user, so the reviewer and
user can assess the change without rerunning it.
A side that reports a timing or a byte-over-time measurement keeps its raw output in the side output location, and the
pull request cites its side record, as Step 5 of `$IMPROVE_KIT_DIR/dev/improve/landing.md` sets.
