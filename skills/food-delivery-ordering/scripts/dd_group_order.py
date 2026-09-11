#!/usr/bin/env python3
"""Join a DoorDash group order as a guest and add one approved item.

The guest flow is: name form (first and last required, phone optional),
the store menu inside the group cart, the item's option modal, then
"Add to cart". The guest never pays; the organizer submits the shared cart
before their deadline. Participant identity lives in the browser session:
entering the same name from a fresh session creates a second, empty
participant, so one participant is written exactly once.

Caller contract: group_url is a drd.sh or doordash.com group-cart link,
item is the exact menu name, and every option pattern was approved by the
user beforehand. Success is verified by reading the participant line
"<First> <Last-initial> (You)" beside the item and its price in the page.

Option patterns are case-insensitive regexes matched against leaf-row text
inside the modal, for example "30%-Minimal" or "Tea Base Removed". A pattern
that matches nothing aborts the run before Add, so a wrong drink is never
submitted silently.

Usage (headed Chrome on a virtual display, see SKILL.md):
    xvfb-run -a python dd_group_order.py "https://drd.sh/cart/<id>/" \
        --first Alex --last Kim --item "Jasmine Milk Tea" \
        --option "70%- Less" \
        --shots /tmp/dd_order [--dry-run]
"""
import argparse
import json
import re
import sys
import time
from pathlib import Path

import browser_common

NAME_FORM_MARKER = "First Name"
CART_MARKERS = ("Featured Items", "Most Ordered")
MODAL_WAIT_MS = 5000
SETTLE_WAIT_MS = 6000


def log(*parts):
    print(*parts, flush=True)


def bbox_by_text(page, text, tag="*"):
    """Bounding-box center of the first element whose trimmed text equals text.

    React handlers ignore synthetic JS clicks, so callers pass the box to
    page.mouse.click for a real input event.
    """
    return page.evaluate(
        """([text, tag]) => {
            const el = [...document.querySelectorAll(tag)]
                .find(e => e.textContent.trim() === text);
            if (!el) return null;
            el.scrollIntoView({block: 'center'});
            const r = el.getBoundingClientRect();
            return {x: r.x + r.width / 2, y: r.y + r.height / 2};
        }""",
        [text, tag],
    )


def real_click(page, box):
    """Click at the box center with a real mouse event; a null box is a no-op."""
    if box:
        page.mouse.click(box["x"], box["y"])


def fill_guest_form(page, first, last, shots):
    """Fill the guest name form, verify the values, click Continue."""
    if not browser_common.wait_for_text(page, NAME_FORM_MARKER):
        return False
    page.wait_for_timeout(2000)
    filled = page.evaluate(
        """([first, last]) => {
            const visible = [...document.querySelectorAll('input')]
                .filter(e => e.offsetParent && e.type === 'text');
            const firstBox = visible.find(
                e => (e.placeholder || '').toLowerCase().includes('first'));
            const lastBox = visible.find(
                e => (e.placeholder || '').toLowerCase().includes('last'));
            if (!firstBox || !lastBox) return 'missing';
            const set = (el, value) => {
                const setter = Object.getOwnPropertyDescriptor(
                    window.HTMLInputElement.prototype, 'value').set;
                setter.call(el, value);
                el.dispatchEvent(new Event('input', {bubbles: true}));
            };
            set(firstBox, first);
            set(lastBox, last);
            return 'ok';
        }""",
        [first, last],
    )
    values = page.evaluate(
        """() => [...document.querySelectorAll('input')]
            .filter(e => e.offsetParent && e.type === 'text')
            .map(e => e.value)"""
    )
    if filled != "ok" or values != [first, last]:
        log("name form failed read-back:", filled, values)
        page.screenshot(path=str(shots / "form_rejected.png"))
        return False
    page.screenshot(path=str(shots / "1_form_filled.png"))
    real_click(page, bbox_by_text(page, "Continue", "button"))
    return True


def open_item_modal(page, item, shots):
    """Scroll the virtualized menu to the item, open its modal, return its data.

    Returns a dict with the modal text ("text") and every radio/checkbox with
    its label and checked state ("controls"), or None when the modal never
    opened.
    """
    for _ in range(70):
        box = bbox_by_text(page, item, "h3")
        if box:
            break
        page.mouse.wheel(0, 700)
        page.wait_for_timeout(500)
    else:
        log("item never appeared in the menu DOM:", item)
        page.screenshot(path=str(shots / "item_not_found.png"))
        return None
    page.wait_for_timeout(1500)
    real_click(page, bbox_by_text(page, item, "h3"))
    page.wait_for_timeout(MODAL_WAIT_MS)
    modal = page.evaluate(
        """() => {
            const dlg = document.querySelector("[role='dialog']");
            if (!dlg) return null;
            const controls = [...dlg.querySelectorAll(
                'input[type=radio],input[type=checkbox]')].map(i => ({
                    checked: i.checked,
                    label: (i.closest('label')?.textContent || i.value || '')
                        .trim().slice(0, 60),
                }));
            return {text: dlg.innerText, controls: controls};
        }"""
    )
    if modal is None:
        log("modal did not open for:", item)
        page.screenshot(path=str(shots / "modal_missing.png"))
        return None
    return modal


def apply_options(page, patterns):
    """Click one option row per pattern; return the click results.

    DoorDash renders option rows as leaf text like "30%-Minimal" or
    "Tea Base Removed"; the modal preselects the merchant's recommended
    defaults, so a pattern list only needs to state the differences.
    """
    results = []
    for pattern in patterns:
        outcome = page.evaluate(
            """(reStr) => {
                const dlg = document.querySelector("[role='dialog']");
                if (!dlg) return 'no-dialog';
                const re = new RegExp(reStr, 'i');
                const rows = [...dlg.querySelectorAll('*')].filter(e => {
                    if (e.childElementCount > 1) return false;
                    const t = e.textContent.trim();
                    return t.length < 60 && re.test(t);
                });
                if (!rows.length) return 'not-found';
                const el = rows[rows.length - 1];
                el.scrollIntoView({block: 'center'});
                const r = el.getBoundingClientRect();
                return JSON.stringify({
                    x: r.x + r.width / 2, y: r.y + r.height / 2,
                    label: el.textContent.trim(),
                });
            }""",
            pattern,
        )
        if not isinstance(outcome, str) or not outcome.startswith("{"):
            log("option pattern matched nothing:", pattern, "->", outcome)
            return None
        box = json.loads(outcome)
        real_click(page, box)
        page.wait_for_timeout(1000)
        results.append(box["label"])
        log("option", pattern, "->", box["label"])
    return results


def add_to_cart(page, shots):
    """Click the modal's add button and return its price label, or None."""
    button = page.evaluate(
        """() => {
            const dlg = document.querySelector("[role='dialog']");
            const el = [...(dlg ? dlg.querySelectorAll('button')
                                : document.querySelectorAll('button'))]
                .find(b => /add to cart/i.test(b.textContent));
            if (!el) return null;
            const r = el.getBoundingClientRect();
            return {x: r.x + r.width / 2, y: r.y + r.height / 2,
                    price: el.textContent.trim(),
                    disabled: el.disabled || el.getAttribute('aria-disabled')};
        }"""
    )
    if not button or button.get("disabled"):
        log("add button missing or disabled:", button)
        page.screenshot(path=str(shots / "add_unavailable.png"))
        return None
    real_click(page, button)
    page.wait_for_timeout(SETTLE_WAIT_MS)
    return button["price"]


def verify_cart(page, first, item, price, shots):
    """Confirm the cart shows the item and price beside the participant line."""
    body = page.inner_text("body")
    participant = re.compile(re.escape(first) + r"\s+\S?\s*\(You\)")
    verified = item.lower() in body.lower() and price in body and participant.search(body)
    page.screenshot(path=str(shots / "4_after_add.png"))
    log("cart verification (item, price, participant):", verified)
    return bool(verified)


def run_session(playwright, args):
    """One browser session: join, configure, add, verify. True on success."""
    browser, page = browser_common.launch_chrome(playwright)
    try:
        page.goto(args.group_url, wait_until="domcontentloaded", timeout=60000)
        if not fill_guest_form(page, args.first, args.last, args.shots):
            log("guest form never became usable")
            return False
        if not browser_common.wait_for_text(page, CART_MARKERS[0]) and not (
            browser_common.wait_for_text(page, CART_MARKERS[1])
        ):
            log("group-cart menu never loaded")
            page.screenshot(path=str(args.shots / "menu_missing.png"))
            return False
        page.wait_for_timeout(3000)
        modal = open_item_modal(page, args.item, args.shots)
        if modal is None:
            return False
        log("=== MODAL ===")
        log(modal["text"][:2500])
        log("=== CONTROLS ===")
        log(json.dumps(modal["controls"], indent=1))
        page.screenshot(path=str(args.shots / "2_item_modal.png"))
        if args.dry_run:
            log("dry-run: printing options only, nothing added")
            return True
        if apply_options(page, args.option) is None:
            page.screenshot(path=str(args.shots / "options_rejected.png"))
            return False
        page.wait_for_timeout(1000)
        page.screenshot(path=str(args.shots / "3_configured.png"))
        price = add_to_cart(page, args.shots)
        if price is None:
            return False
        return verify_cart(page, args.first, args.item, price, args.shots)
    finally:
        browser.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "group_url",
        help="group order share link (drd.sh/cart/... or doordash.com/cart/...)",
    )
    parser.add_argument("--first", required=True)
    parser.add_argument("--last", required=True)
    parser.add_argument("--item", required=True, help="exact name shown on the menu")
    parser.add_argument(
        "--option",
        action="append",
        default=[],
        help="option-row regex to click inside the modal; repeatable",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the modal options and exit without adding",
    )
    parser.add_argument("--shots", default="/tmp/dd_order", help="screenshot directory")
    parser.add_argument("--attempts", type=int, default=3)
    args = parser.parse_args()
    args.shots = Path(args.shots)
    args.shots.mkdir(parents=True, exist_ok=True)

    from playwright.sync_api import sync_playwright

    with sync_playwright() as playwright:
        for attempt in range(1, args.attempts + 1):
            log("=== attempt", attempt, "===")
            try:
                if run_session(playwright, args):
                    log("ORDER ADDED ✔")
                    return
            except Exception as error:
                log("session error:", str(error)[:300])
            time.sleep(5)
    sys.exit("all attempts failed: inspect the screenshots in " + str(args.shots))


if __name__ == "__main__":
    main()
