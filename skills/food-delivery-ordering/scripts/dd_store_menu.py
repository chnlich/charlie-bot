#!/usr/bin/env python3
"""Dump a DoorDash store menu from its server-rendered JSON-LD.

The store page embeds the full menu, every section and item with its price,
as a schema.org JSON-LD script block whose @type is "Menu". The visible DOM
is a virtualized list that renders only the items near the scroll position,
so the JSON-LD is the reliable surface. Plain HTTP fetches receive
Cloudflare 403: run through a real headed Chrome (browser_common) under
xvfb-run.

Usage:
    xvfb-run -a python dd_store_menu.py <store-url-or-store-id>

Output: one line per item, "<section> / <item name> / <price>".
"""
import argparse
import json
import re
import sys

import browser_common

PAGE_MARKER = "Featured Items"
STORE_URL_TEMPLATE = "https://www.doordash.com/store/{store_id}/"


def extract_menu(html):
    """Return the JSON-LD block with @type Menu, or None when absent."""
    for match in re.finditer(
        r'<script type="application/ld\+json"[^>]*>(.*?)</script>', html, re.S
    ):
        try:
            block = json.loads(match.group(1))
        except json.JSONDecodeError:
            continue
        if isinstance(block, dict) and block.get("@type") == "Menu":
            return block
    return None


def print_menu(menu):
    """Print each item as "<section> / <item name> / <price>"."""
    for section in menu.get("hasMenuSection", []):
        section_name = section.get("name", "?")
        for item in section.get("hasMenuItem", []):
            price = (item.get("offers") or {}).get("price", "?")
            print(f"{section_name} / {item.get('name')} / {price}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "store",
        help="store URL, or the numeric store id from a doordash.com store URL",
    )
    args = parser.parse_args()
    url = (
        args.store
        if args.store.startswith("http")
        else STORE_URL_TEMPLATE.format(store_id=args.store)
    )

    from playwright.sync_api import sync_playwright

    with sync_playwright() as playwright:
        browser, page = browser_common.launch_chrome(playwright)
        try:
            page.goto(url, wait_until="domcontentloaded", timeout=60000)
            if not browser_common.wait_for_text(page, PAGE_MARKER):
                sys.exit(
                    "page never showed its menu: the defense is challenging this "
                    "session. Retry in a fresh session (new xvfb-run)."
                )
            page.wait_for_timeout(5000)
            html = page.content()
        finally:
            browser.close()

    menu = extract_menu(html)
    if menu is None:
        sys.exit("no Menu JSON-LD in the page: retry in a fresh session")
    print_menu(menu)


if __name__ == "__main__":
    main()
