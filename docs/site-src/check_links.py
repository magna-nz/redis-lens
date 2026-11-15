"""Checks the built docs page.

- Every internal #anchor resolves, ids are unique and local files exist.
- Outside resources come only from Google Fonts and jsdelivr's mermaid; images are always local.
- Every image has alt text; the screenshots and GIFs also have width, height and loading="lazy".
- The Terminal / Dashboard switch: both buttons are there with aria-pressed, data-only holds
  "terminal" or "dashboard", and each sidebar link shows in the same mode as its target.
- No dashboard sign-in token is anywhere in the page.

Usage: check_links.py [docs/index.html ...]   (default: docs/index.html of this repository)
"""
import os
import re
import sys
from html.parser import HTMLParser

ALLOWED = ("https://fonts.googleapis.com", "https://fonts.gstatic.com", "https://cdn.jsdelivr.net/npm/mermaid")
VOID = {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr"}
MODES = {"terminal", "dashboard"}


class P(HTMLParser):
    def __init__(self):
        super().__init__()
        self.ids, self.hrefs, self.srcs = [], [], []
        self.mode_of_id = {}
        self.nav_links = []  # (href, mode)
        self.imgs = []  # (attrs)
        self.choices = []  # (value, aria-pressed)
        self.bad_modes = []
        self.stack = []  # (tag, mode, in_nav)

    def mode(self):
        for _, m, _ in reversed(self.stack):
            if m:
                return m
        return None

    def in_nav(self):
        return any(n for _, _, n in self.stack)

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        only = a.get("data-only")
        if only is not None and only not in MODES:
            self.bad_modes.append(only)
        mode = only or self.mode()
        nav = tag == "nav" and "sidebar" in (a.get("class") or "")
        if "id" in a:
            self.ids.append(a["id"])
            self.mode_of_id[a["id"]] = mode
        if tag == "a" and "href" in a:
            self.hrefs.append(a["href"])
            if self.in_nav() and a["href"].startswith("#"):
                self.nav_links.append((a["href"][1:], mode))
        if tag in ("img", "script") and "src" in a:
            self.srcs.append((tag, a["src"]))
        if tag == "img":
            self.imgs.append(a)
        if tag == "link" and "href" in a and a.get("rel") not in ("preconnect",):
            self.srcs.append((tag, a["href"]))
        if tag == "button" and "data-ui-choice" in a:
            self.choices.append((a["data-ui-choice"], a.get("aria-pressed")))
        if tag not in VOID:
            self.stack.append((tag, only, nav))

    def handle_endtag(self, tag):
        for i in range(len(self.stack) - 1, -1, -1):
            if self.stack[i][0] == tag:
                del self.stack[i:]
                break


def check(path):
    ok = True

    def fail(*msg):
        nonlocal ok
        print(path, *msg)
        ok = False

    text = open(path, encoding="utf-8").read()
    p = P()
    p.feed(text)
    here = os.path.dirname(path)
    ids = set(p.ids)
    dups = sorted({i for i in p.ids if p.ids.count(i) > 1})
    if dups:
        fail("duplicate ids", dups)
    for h in p.hrefs:
        if h.startswith("#"):
            if h[1:] not in ids:
                fail("broken anchor", h)
        elif not h.startswith(("http://", "https://", "mailto:")):
            f = h.split("#")[0].split("?")[0]
            if f and not os.path.exists(os.path.join(here, f)):
                fail("missing file", h)
    for tag, s in p.srcs:
        if s.startswith(("http:", "https:", "//")):
            if tag == "img" or not s.startswith(ALLOWED):
                fail("external resource", s)
        elif not os.path.exists(os.path.join(here, s)):
            fail("missing resource", s)
    for a in p.imgs:
        src = a.get("src", "")
        if "alt" not in a:
            fail("image without alt", src)
        if src.startswith("img/") or src.endswith(".gif"):
            for need in ("width", "height"):
                if not (a.get(need) or "").isdigit():
                    fail(f"image without {need}", src)
            if a.get("loading") != "lazy":
                fail("image not lazy-loaded", src)
    if p.bad_modes:
        fail("data-only must be terminal or dashboard:", sorted(set(p.bad_modes)))
    if sorted(v for v, _ in p.choices) != ["dashboard", "terminal"]:
        fail("the switch needs one Terminal and one Dashboard button, found", p.choices)
    for v, pressed in p.choices:
        if pressed not in ("true", "false"):
            fail("switch button without aria-pressed:", v)
    for target, mode in p.nav_links:
        want = p.mode_of_id.get(target)
        if want != mode:
            fail(f"nav link #{target} shows in {mode or 'both'} but its target in {want or 'both'}")
    if re.search(r"#token=[0-9a-f]{16,}", text):
        fail("a dashboard sign-in token is in the page")
    internal = sum(h.startswith("#") for h in p.hrefs)
    print(f"{path}: {len(p.hrefs)} links, {internal} internal anchors, {len(ids)} ids, {len(p.imgs)} images, "
          f"{sum(1 for m in p.mode_of_id.values() if m == 'terminal')} terminal-only and "
          f"{sum(1 for m in p.mode_of_id.values() if m == 'dashboard')} dashboard-only ids")
    return ok


paths = sys.argv[1:] or [os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "index.html")]
ok = all([check(p) for p in paths])
print("OK" if ok else "FAIL")
sys.exit(0 if ok else 1)
