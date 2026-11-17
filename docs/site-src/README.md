# The docs site's source

`docs/index.html` is generated. Edit `index.src.html` here, never the output, then rebuild:

```sh
make site
# or
python3 docs/site-src/build.py                 # docs/site-src/index.src.html -> docs/index.html
python3 docs/site-src/check_links.py           # checks docs/index.html
```

Run both from the repository root. They need Python 3 and nothing else.

- `build.py` replaces `<!--TERM:name-->` with a terminal screen drawn from the app's insta
  snapshot files (`src/**/snapshots/`), and `<!--KEYTABLES-->` with the key tables of
  `spec.md`'s "### Keys" section. A snapshot or key change shows up on the next build.
  Runs of more than two empty body rows (tall 120x36 screens) are trimmed to two; the size
  label keeps the snapshot's size. Line 0 of a snapshot is the context bar and line 1 the
  area row, with the open area's panels inline.
  `build.py <repo root> <template> <output>` builds from other paths.
- `check_links.py` checks that every `#anchor` resolves, ids are unique, local files exist,
  nothing is loaded from another host but Google Fonts and mermaid (images always local),
  images have alt text and a size, the Terminal / Dashboard switch is complete, every
  sidebar link shows in the same mode as its target, and no dashboard sign-in token is in
  the page.

## The Terminal / Dashboard switch

- Content that applies to one of them carries `data-only="terminal"` or
  `data-only="dashboard"`. Everything else shows in both.
- Give a sidebar `<li>` the same `data-only` as the section it links to; the checker fails
  otherwise.
- The script in `<head>` sets `html[data-ui]` before the page is drawn, from `?ui=terminal`
  or `?ui=dashboard`, then `localStorage` (`redis-lens.docs.ui`), then `terminal`. Without
  JavaScript the switch is hidden and everything shows.

## Images

The dashboard screenshots in `docs/img/` are PNGs, 1280 px wide, from the demo servers
(`make demo-up`) only. Give each `<img>` `alt`, `width`, `height` and `loading="lazy"`.
`docs/dashboard.gif` is recorded with `make dashboard-gif`.
