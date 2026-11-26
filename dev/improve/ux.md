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
A change to a web page takes its screenshots and serve trial from landing.md's Web page evidence section.

A page change may include its endpoint. An endpoint-only change belongs to the performance direction. Each direction
keeps one category of change so the review has one scope owner.

## Topics

Choose a defect at a required desktop or phone width on the shared mock home. Use the widths in the Web UI Screenshots
rule in `prompts/task_base.md`, and apply the screen-space rule in `AGENTS.md` to each chosen defect.

A defect is one of two kinds:

- a layout defect that the before screenshot shows;
- a click defect, as the Clicks section defines.

## Clicks

The round clicks each control on the screen it examines. The click helper, the refusals it makes and the write rules
that every click follows are in landing.md's Web page evidence section.

A refused click is never a click defect by itself.

A click defect is a control whose click has one of these results:

- the screen shows no change, or the change falls where the user cannot see it;
- the screen changes but does not show what the control did;
- a write that the screen shows as pending or sent never reaches the session, including when the user leaves the screen
  before the control's delay ends.

A click that the control's design leaves without effect, such as a send with no text, is not a click defect.
For a control that starts a delayed write, the round also clicks it and leaves the screen during the delay.

## Evidence

Evidence names the defect visible before and the resulting state after at each required width. For a click defect, the
before evidence shows the screen before and after the click, with the recorded click result that shows no effect.
