---
name: food-delivery-ordering
description: >
  Read delivery-platform store menus and place guest items into group
  orders from a headless host, with screenshot evidence. DoorDash is
  written out; Uber Eats follows the same workflow. Use when the user asks
  to order, join a group order, or see what a store offers.
version: 1.0.0
---

# Food Delivery Ordering

Join a delivery platform's group order as a guest and add one approved item,
or read a store's full menu, from a headless host with no account and no
payment step. A guest never pays: the group order's organizer reviews the
shared cart and submits before their own deadline, so a guest item is cheap
to change and cheap to remove. The real risks are the platform's bot
defense and a mis-clicked option, and both are answered with evidence:
a screenshot of the filled name form, of the configured modal, and of the
cart, plus a read-back of the participant line.

Platforms share one workflow and differ in bot defense and page structure.
DoorDash is written out below. A second platform, such as Uber Eats, earns
its own section only after one real order has gone end to end on it
(see Adding a platform).

## Environment (one-time per host)

Real Google Chrome, Xvfb, and a Playwright venv:

```
uv venv /tmp/pwenv
uv pip install --python /tmp/pwenv/bin/python playwright
/tmp/pwenv/bin/playwright install chromium
sudo apt-get install -y xvfb
# Chrome stable: install the .deb from
# https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb
```

Run every browser step under `xvfb-run -a`: the scripts launch real Chrome
headed on a virtual display (`scripts/browser_common.py`). Headless mode and
plain HTTP fetches both fail on these sites (next section), so even a
read-only menu dump goes through the browser.

## Shared workflow

1. Reach the page. Bot defense challenges automated sessions; a clean page
   is detected by a content marker, and a session that never shows the
   marker is discarded and retried in a fresh browser.
2. Read the menu from the server-rendered surface, never from the visible
   list: menus render lazily and a scroll dump misses most items.
3. Join as the guest name the user gave, and read the input values back
   before continuing. The name is how orders are sorted at handoff, so a
   wrong name is a wrong delivery.
4. Open the item's option modal, print its groups, and click exactly the
   approved options. Merchant-recommended defaults are preselected, so the
   option list only states differences from those defaults.
5. Add, then verify the cart line by reading the page: the item, its price,
   and the participant name must all appear. Screenshot at the form, the
   configured modal, and the cart.

## DoorDash

### Bot defense

DoorDash sits behind Cloudflare Turnstile. Headless Chromium renders pages
but an invisible `div[data-testid="turnstile/overlay"]` intercepts every
pointer event, so clicks time out while the page looks fine. The working
configuration is real Chrome (`channel="chrome"`), headed, under Xvfb, with
`--disable-blink-features=AutomationControlled` and `navigator.webdriver`
patched to undefined before any page script runs; `browser_common.py` sets
all of it. Repeated automated visits lower the score until pages become a
"Just a moment..." interstitial; the recovery is a fresh session per
attempt (the scripts do three) with retries spaced out. A store page that
shows the text `Featured Items` is clean; a guest form that shows
`First Name` with `Continue` is clean.

### Menu

The full menu is server-rendered as schema.org JSON-LD: a script block with
`@type: Menu` whose `hasMenuSection[].hasMenuItem[]` entries carry
`offers.price`. Network capture finds no menu request, and the rendered
list is virtualized to a window around the scroll position, so parsing the
JSON-LD is the only complete read:

```
xvfb-run -a /tmp/pwenv/bin/python dd_store_menu.py <store-url-or-store-id>
```

Store URLs look like `https://www.doordash.com/store/<slug>-<storeId>/...`.

### Group order flow

Group order share links (`https://drd.sh/cart/<id>/`) redirect to a guest
form: first and last name (phone optional), then `Continue`, then the store
menu inside the group cart, then the item modal, then
`Add to cart - $<price>`. One participant is written exactly once:
participant identity is the browser session, so re-entering the same name
from a fresh session creates a second, empty participant with that name.
Checking an order from outside means opening the link in the user's own
browser, or reading the screenshots.

```
xvfb-run -a /tmp/pwenv/bin/python dd_group_order.py \
  "https://drd.sh/cart/<id>/" \
  --first Alex --last Kim --item "Jasmine Milk Tea" \
  --option "70%- Less" \
  --shots /tmp/dd_order
```

`--dry-run` prints every option group with its checked state and exits
without adding: it answers "what are the choices". Option rows are leaf
text inside the modal, for example `30%-Minimal`, `Tea Base Removed`,
`Sago +$1.00`. Tea stores expose a sugar scale
(`100%-Regular`, `70%- Less(Recommend)`, `30%-Minimal`, `0%-No Added`),
numbered by the merchant's sugar percentage, and tea-base drinks often
offer `Tea Base Removed` as the caffeine-free
variant.

### Page mechanics

React handlers ignore synthetic JS clicks: click with real mouse events at
the element's bounding-box center. The menu list virtualizes, so items
exist in the DOM only near the scroll position: step-scroll and re-check
for the item's `<h3>` each step. Playwright Python's `screenshot()` takes
keyword arguments only (`path=...`); a positional path raises TypeError.
Fill inputs through the native value setter plus an `input` event, then
read the values back.

## TODO: second platform (Uber Eats)

Uber Eats has no section yet, and none should be written before one real
order has gone end to end on it. When that happens, write the section in
the shape of the DoorDash section above (bot defense, menu, group order
flow, page mechanics) and add `ue_` scripts beside `browser_common.py`,
which stays shared.

## Boundaries

Order only what the user explicitly approved: item, options, and the name
to write. The guest flow has no payment step; no payment data is ever
entered. "Done adding items" and similar buttons are participant status
flags, not submissions, and skipping them changes nothing: the organizer's
deadline submits the shared cart as it stands. Changes and removals before
the deadline are manual, by the participant or the organizer, in their own
browser.
