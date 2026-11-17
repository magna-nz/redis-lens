#!/usr/bin/env python3
"""Builds docs/index.html for redis-lens from a hand-written template.

- <!--TERM:name--> markers become terminal mocks drawn from the insta snapshot files
  under src/tui/**/snapshots/ (the text is the app's own rendering; only colour is added).
- <!--KEYTABLES--> becomes the key tables of spec.md's "### Keys" section.

Usage:
  python3 docs/site-src/build.py                       # this repository: docs/site-src/index.src.html -> docs/index.html
  python3 docs/site-src/build.py <repo root> <template> <output>
"""
import glob
import html
import os
import re
import sys

if len(sys.argv) == 4:
    ROOT, TEMPLATE, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
elif len(sys.argv) == 1:
    ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    TEMPLATE = os.path.join(ROOT, "docs", "site-src", "index.src.html")
    OUT = os.path.join(ROOT, "docs", "index.html")
else:
    raise SystemExit("usage: build.py [<repo root> <template> <output>]")

BOX = set("┌┐└┘─│├┤┬┴┼")
BARS = set("█▉▊▋▌▍▎▏▁▂▃▄▅▆▇")

AREAS = {0: "0 Connections", 1: "1 Overview", 2: "2 Browse", 3: "3 Analyze", 4: "4 Server", 5: "5 Pub/Sub"}

# name: (snapshot stem, area, active panel or None, selected (line, pane) list, footer kind, edits)
# Line numbers are those of the snapshot file; empty rows of tall screens are trimmed after marking.
SCREENS = {
    "connections": ("app__dashboard_tests__connections_with_the_dashboard_120x36", 0, None, [(5, 0)], "keys", []),
    "overview-before": ("ui__overview__tests__before_120x36", 1, None, [], "keys", []),
    "overview-dialog": ("ui__overview__tests__dialog_deep_120x36", 1, None, [(16, 1)], "notice", []),
    "overview-analyzing": ("ui__overview__tests__analyzing_120x36", 1, None, [(9, 0)], "keys", []),
    "overview-after": ("ui__overview__tests__analyzed_120x36", 1, None, [(9, 0)], "ok", []),
    "overview-cluster": ("ui__overview__tests__cluster_120x36", 1, None, [(9, 0)], "keys", []),
    "browse-names": ("snapshot_tests_browse__browse_three_names_only_120x36", 2, "Keys", [(4, 1)], "keys", []),
    "browse-patterns": ("snapshot_tests_browse__browse_three_focus_patterns_opened_120x36", 2, "Keys", [(9, 0)], "keys", []),
    "browse-keylist": ("snapshot_tests_browse__browse_three_focus_keys_120x36", 2, "Keys", [(4, 1)], "keys", []),
    "browse-value": ("snapshot_tests_browse__browse_three_focus_value_120x36", 2, "Keys", [(7, 2)], "keys", []),
    "browse-actions": ("snapshot_tests_browse__browse_three_actions_menu_120x36", 2, "Keys", [(16, 2)], "notice", []),
    "browse-find-prompt": ("app__find__tests__find_prompt_120x36", 2, "Keys", [(4, 0)], "notice", []),
    "browse-find": ("app__find__tests__find_results_120x36", 2, "Keys", [(4, 1)], "keys", []),
    "browse-binary": ("app__views__value__tests__binary_120x36", 2, "Keys", [(7, 2)], "keys", []),
    "browse-streams": ("app__views__streams__tests__list_120x36", 2, "Streams", [(4, 0)], "keys", []),
    "browse-search": ("app__views__search__tests__results_120x36", 2, "Search", [(5, 1)], "keys", []),
    "goto-slow": ("app__goto__tests__goto_slow_120x36", 1, None, [(10, 1)], "keys", []),
    "goto-cart": ("app__goto__tests__goto_cart_120x36", 1, None, [(7, 1)], "keys", []),
    "menu-value": ("app__views__value__tests__menu_value_hash_writes_on_120x36", 2, "Keys", [(18, 2)], "notice", []),
    "analyze-bigkeys": ("app__views__analysis__tests__big_keys_120x36", 3, "Big keys", [(5, 0)], "keys", []),
    "analyze-expiry": ("ui__ttl__tests__expiry_merged_120x36", 3, "Expiry", [(4, 0)], "keys", []),
    "analyze-changes": ("snapshot_tests__diff_view_with_trend", 3, "Changes", [(5, 0)], "keys", []),
    "analyze-hot": ("snapshot_tests__hot_keys_view", 3, "Hot keys", [(5, 0)], "keys", []),
    "analyze-slots": ("app__views__slots__tests__snapshots__populated_on_redis_80x24", 3, "Slots", [(9, 0)], "keys", []),
    "server-clients": ("app__views__server__tests__clients_120x36_second_group_selected", 4, "Clients", [(5, 0)], "keys", []),
    "server-slowlog": ("app__views__server__tests__slow_log_120x36_with_a_note", 4, "Slow log", [(7, 0)], "keys", []),
    "server-monitor": ("app__views__server__monitor_tests__monitor_running_120x36", 4, "Monitor", [(13, 0)], "notice", ["monitor"]),
    "pubsub-tail": ("app__views__pubsub__tests__tail_120x36", 5, None, [(4, 0)], "keys", []),
    "cmdline-panel": ("app__views__cmdline__tests__panel_120x36", 2, "Keys", [], "notice", []),
    "cmdline-confirm": ("app__views__cmdline__tests__confirm_prod_80x24", 2, "Keys", [], "notice", []),
    "help-overview": ("ui__help__tests__overview_120x36", None, None, [], "none", []),
    "help-filtered": ("ui__help__tests__filtered_120x36", None, None, [(14, 0)], "none", []),
    "sheet-cmdline": ("app__views__cmdline__tests__sheet_cmdline_write_120x36", 2, "Keys", [], "keys", []),
    "gate-writes": ("app__tests__sheet_writes_on_120x36", 2, "Keys", [], "keys", []),
    "gate-delete": ("app__views__value__tests__sheet_delete_prod_120x36", 2, "Keys", [], "keys", []),
    "gate-bulk": ("app__bulk__tests__bulk_expire_question_prod_80x24", 2, "Keys", [], "keys", []),
    "dbs": ("app__dbs__tests__the_list_open_80x24", 2, "Keys", [(11, 1)], "keys", []),
}


def find_snapshot(stem):
    hits = glob.glob(os.path.join(ROOT, "src", "**", "snapshots", f"redis_lens__tui__{stem}.snap"), recursive=True)
    if len(hits) != 1:
        raise SystemExit(f"snapshot {stem}: found {hits}")
    return hits[0]


def read_lines(path):
    lines = []
    for raw in open(path, encoding="utf-8").read().splitlines():
        if raw.startswith('"') and raw.endswith('"') and len(raw) >= 2:
            lines.append(raw[1:-1])
    if not lines:
        raise SystemExit(f"{path}: no screen lines")
    return lines


def blank_body(line):
    inner = line.strip()
    return bool(inner) and all(c in BOX or c == " " for c in inner) and not any(c in "┌└├┬┴" for c in inner)


def crop(lines):
    """The indexes of the rows to keep: long runs of empty body rows (tall screens) collapse to two."""
    keep, run = [], 0
    for i, line in enumerate(lines):
        if 3 <= i < len(lines) - 2 and blank_body(line):
            run += 1
            if run > 2:
                continue
        else:
            run = 0
        keep.append(i)
    return keep


def classes_for(lines, area, panel, sels, footer):
    n = len(lines)
    cls = [[set() for _ in line] for line in lines]

    def mark(i, a, b, c):
        for j in range(max(0, a), min(len(lines[i]), b)):
            cls[i][j].add(c)

    for i, line in enumerate(lines):
        for j, ch in enumerate(line):
            if ch in BOX:
                cls[i][j].add("b")
            elif ch in BARS:
                cls[i][j].add("bar")
            elif ch == "░":
                cls[i][j].add("d")
            elif ch == "▲":
                cls[i][j].add("w")
            elif ch == "✗":
                cls[i][j].add("e")
            elif ch == "✓":
                cls[i][j].add("o")
            elif ch in "○·…":
                cls[i][j].add("d")
            elif ch in "›▾▸‹":
                cls[i][j].add("a")
        # border titles
        if line[:1] in "┌├" or (line.lstrip()[:1] in "┌├" and area is None) or "┌─" in line or "├─" in line:
            for m in re.finditer(r"─ ([^─]+?) ─", line):
                if any(c not in BOX and c != " " for c in m.group(1)):
                    mark(i, m.start(1), m.end(1), "t")
        for m in re.finditer(r"\bread-only\b", line):
            mark(i, m.start(), m.end(), "w")
        for m in re.finditer(r"\bwrites on\b", line):
            mark(i, m.start(), m.end(), "e")
        for m in re.finditer(r"\bPROD\b", line):
            mark(i, m.start(), m.end(), "e")
        for m in re.finditer(r"\bSTAGE\b", line):
            mark(i, m.start(), m.end(), "w")
        for m in re.finditer(r"\bLOCAL\b", line):
            mark(i, m.start(), m.end(), "o")
        for m in re.finditer(r"\$ redis-cli[^│]*", line):
            mark(i, m.start(), m.end(), "a")
        # Colour roles as the terminal draws them (spec.md, Theme and NO_COLOR).
        # Warning: a ▲ and the sentence it starts (not a flagged row of a table, whose ▲ alone is marked).
        for m in re.finditer(r"▲ ([^│]*)", line):
            rest = re.split(r" {3,}", m.group(1))[0]  # stop before a column such as WHERE
            if rest.strip() and not re.match(r"\d\d:\d\d", rest) and re.match(r"\S+ \S+", rest):
                end = m.start(1) + len(rest.rstrip())
                mark(i, m.start(1), end, "w")
        # Warning: `never` in an expiry column, and `asks first`.
        for m in re.finditer(r"(?<=  )never(?=\s*(│|$| (hash|list|set|zset|string|stream)\b))", line):
            mark(i, m.start(), m.end(), "w")
        for m in re.finditer(r"\basks first\b", line):
            mark(i, m.start(), m.end(), "w")
        # Danger: what can lose data, in the Actions menu, Go to and the confirm sheet.
        for m in re.finditer(r"(?:│|›) {1,3}(?:› )?\S{1,6} +(Delete key|Delete all|Delete|End|Reset|Drop|Trim)\b", line):
            mark(i, m.start(1), m.end(1), "e")
        for m in re.finditer(r"This can't be undone\.", line):
            mark(i, m.start(), m.end(), "e")
        # Placeholder: the {parts} of an inferred pattern.
        for m in re.finditer(r"\{(id|uuid|ulid|hash|ip|var|num|date)\}", line):
            mark(i, m.start(), m.end(), "ph")

    if area is not None:
        # Line 0 is the context bar: profile, environment, access, database, address, state.
        top = lines[0]
        mark(0, 0, len(top), "top")
        m = re.match(r" (\S+)", top)
        if m:
            mark(0, m.start(1), m.end(1), "strong")
        for word, badge in (("PROD", "badge-e"), ("STAGE", "badge-w"), ("WRITES ON", "badge-e")):
            k = top.find(f" {word} ")
            if k >= 0:
                mark(0, k, k + len(word) + 2, badge)
        for m in re.finditer(r"(?<=  )[wb](?=  )", top):
            mark(0, m.start(), m.end(), "k")
        for m in re.finditer(r"● (connected|analyzed)[^│]*$", top):
            mark(0, m.start(), m.end(), "o")
        for m in re.finditer(r"[⠇⠋] analyzing \d+%", top):
            mark(0, m.start(), m.end(), "a")
        # Line 1 is the area row; the open area shows its panels inline ("2  Browse › Keys · Streams · Search").
        row = lines[1]
        m = re.search(rf"(?:^| ){area}  \S.*?(?=   |$)", row)
        if m:
            a = m.start() + (1 if row[m.start()] == " " else 0)
            mark(1, a - 1, m.end() + 1, "tab")
            if panel:
                k = row.find("›", a, m.end())
                for p in re.finditer(r"[^·›]+", row[k + 1 : m.end()] if k >= 0 else ""):
                    name = p.group(0).strip()
                    if name and name != panel and not name.startswith(panel + " "):
                        s = k + 1 + p.start() + p.group(0).index(name)
                        mark(1, s, s + len(name), "d")
        if area != 0:
            for j in range(len(row)):
                if "tab" not in cls[1][j]:
                    cls[1][j].add("d")

    for (i, pane) in sels:
        line = lines[i]
        bars = [j for j, c in enumerate(line) if c == "│"]
        if len(bars) >= pane + 2:
            a, b = bars[pane] + 1, bars[pane + 1]
        elif len(bars) == pane + 1:
            a, b = bars[pane] + 1, len(line)
        else:
            a, b = 0, len(line)
        mark(i, a, b, "sel")

    last = n - 1
    foot = lines[last]
    if footer == "keys":
        for m in re.finditer(r"\S+(?: \S+)*", foot):
            seg = m.group(0)
            first = seg.split(" ", 1)[0]
            mark(last, m.start(), m.start() + len(first), "k")
            mark(last, m.start() + len(first), m.end(), "d")
    elif footer == "ok":
        mark(last, 0, len(foot), "o")
    elif footer == "notice":
        mark(last, 0, len(foot), "d")
    return cls


ORDER = ["top", "sel", "tab", "badge-e", "badge-w", "b", "bar", "t", "strong", "k", "e", "w", "o", "a", "ph", "d"]


def render(lines, cls):
    out = []
    for line, row in zip(lines, cls):
        parts, cur, buf = [], None, ""
        for ch, c in zip(line, row):
            key = " ".join(x for x in ORDER if x in c)
            if key != cur:
                if buf:
                    parts.append((cur, buf))
                cur, buf = key, ""
            buf += ch
        if buf:
            parts.append((cur, buf))
        html_line = "".join(
            (f'<span class="{k}">{html.escape(t, quote=False)}</span>' if k else html.escape(t, quote=False))
            for k, t in parts
        )
        out.append(html_line)
    return "\n".join(out)


def term(name):
    stem, area, panel, sels, footer, edits = SCREENS[name]
    lines = read_lines(find_snapshot(stem))
    width = max(len(l) for l in lines)
    if "monitor" in edits:
        # The fixture's SET value reads like a secret; show an ordinary value instead.
        old, new = "SET rate:1.2.3.4 hunter2-monitor-secret EX 60", "SET rate:1.2.3.4 41 EX 60"
        lines = [l.replace(old, new + " " * (len(old) - len(new))) for l in lines]
    for l in lines:
        if "hunter2" in l or "secret" in l.lower():
            raise SystemExit(f"{name}: credential-looking text left in: {l}")
    cls = classes_for(lines, area, panel, sels, footer)
    keep = crop(lines)
    lines, cls = [lines[i] for i in keep], [cls[i] for i in keep]
    wide = " term-wide" if width > 100 else ""
    src = os.path.relpath(find_snapshot(stem), ROOT)
    return (
        f'<div class="term-window{wide}" data-cols="{width}">'
        f'<div class="term-bar"><span class="dots"><i></i><i></i><i></i></span>'
        f'<span class="term-size">{width}×{len(read_lines(find_snapshot(stem)))}</span></div>'
        f'<pre class="term" aria-label="Terminal screen drawn from {html.escape(os.path.basename(src))}">'
        f"{render(lines, cls)}</pre></div>"
    )


def inline_md(s):
    s = html.escape(s, quote=False)
    s = re.sub(r"`([^`]+)`", r"<code>\1</code>", s)
    return s


def keys_cell(s):
    keys = re.findall(r"`([^`]+)`", s)
    rest = re.sub(r"`[^`]+`", "", s).strip()
    out = " ".join(f"<kbd>{html.escape(k, quote=False)}</kbd>" for k in keys)
    if rest:
        out += " " + html.escape(rest, quote=False)
    return out


def key_tables():
    spec = open(os.path.join(ROOT, "spec.md"), encoding="utf-8").read().splitlines()
    start = spec.index("### Keys")
    end = spec.index("### Keys overlay")
    out, title, rows = [], None, []

    def flush():
        if title and rows:
            slug = "keys-" + re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")
            body = "".join(
                f"<tr><td class=\"keys\">{keys_cell(k)}</td><td>{inline_md(a)}</td></tr>" for k, a in rows
            )
            out.append(
                f'<div class="sub" id="{slug}"><h3>{html.escape(title, quote=False)} '
                f'<a class="anchor" href="#{slug}" aria-label="Link to this table">#</a></h3>'
                f'<div class="table-wrap"><table class="keytable"><thead><tr><th>Keys</th><th>Action</th></tr></thead>'
                f"<tbody>{body}</tbody></table></div></div>"
            )

    for line in spec[start:end]:
        if line.startswith("#### "):
            flush()
            title, rows = line[5:].strip(), []
        elif line.startswith("| ") and not line.startswith("| Keys") and not line.startswith("|---"):
            cells = [c.strip() for c in line.strip().strip("|").split(" | ")]
            if len(cells) == 2:
                rows.append(cells)
    flush()
    return "\n".join(out), len(out)


page = open(TEMPLATE, encoding="utf-8").read()
used = set()


def sub_term(m):
    used.add(m.group(1))
    return term(m.group(1))


page = re.sub(r"<!--TERM:([a-z-]+)-->", sub_term, page)
tables, count = key_tables()
page = page.replace("<!--KEYTABLES-->", tables)
unused = set(SCREENS) - used
if unused:
    print("unused screens:", sorted(unused), file=sys.stderr)
open(OUT, "w", encoding="utf-8").write(page)
print(f"wrote {OUT}: {len(used)} screens, {count} key tables")
