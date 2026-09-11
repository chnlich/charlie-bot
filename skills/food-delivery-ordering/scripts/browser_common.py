"""Shared browser setup for delivery-platform automation.

Delivery platforms in this skill sit behind bot defense (Cloudflare on
DoorDash). Headless Chromium renders their pages, but an invisible challenge
overlay intercepts every pointer event, so clicking is impossible. The
working configuration is real Chrome, headed, on a virtual display: callers
run the process under `xvfb-run -a` and use launch_chrome().

wait_for_text() polls a page until a marker appears in the body text. A
False return means the defense is challenging the session; the caller
discards the session and retries in a fresh one.
"""

LAUNCH_ARGS = ["--disable-blink-features=AutomationControlled"]
VIEWPORT = {"width": 1440, "height": 1900}
POLL_MS = 3000
POLL_TRIES = 30


def launch_chrome(playwright):
    """Start real Chrome headed with webdriver patched out; return (browser, page).

    Real Chrome is required (Chromium or a forced user_agent string breaks the
    fingerprint the defense scores). The UA the browser sends for itself stays
    consistent with the binary; do not override it.
    """
    browser = playwright.chromium.launch(
        channel="chrome", headless=False, args=LAUNCH_ARGS
    )
    context = browser.new_context(viewport=VIEWPORT, locale="en-US")
    # Cloudflare scores navigator.webdriver; patch it before any page script runs.
    context.add_init_script(
        "Object.defineProperty(navigator,'webdriver',{get:()=>undefined});"
    )
    return browser, context.new_page()


def wait_for_text(page, marker):
    """Poll until marker appears in the page body; True means the page is usable."""
    for _ in range(POLL_TRIES):
        if marker in page.inner_text("body"):
            return True
        page.wait_for_timeout(POLL_MS)
    return False
