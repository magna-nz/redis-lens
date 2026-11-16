# UI redesign (2026-11)

The design brief for the redesign of the terminal UI and the web dashboard. The owner approved the
mockup at https://claude.ai/artifact/WzCEbkbaSv65p7jayi6d2x. `spec.md` stays the source of truth
for behaviour: each change that lands updates it, and this file records the design the changes
aim at.

Nothing here changes what is read from or written to a server. The project rules hold as they
are: no credential on screen, every write through the one write gate, a feature that is off says
why, and no new server reads (the redesign never adds a command that runs as the cursor moves).

## 1. One context bar

Connection name, environment badge, access, database, node and status sit on one line, in both
UIs, and each is switched from there.

- Terminal, row 1: ` name  ENV  w read-only  b db 0  primary · 15G / 24G · 7.92M keys` on the
  left, `● analyzed 14:02 UTC · live 2 s` on the right. `w` and `b` are drawn as key caps: they
  are the keys that change the thing beside them. On a cluster the picked node is shown here too
  (`[ ] node 10.0.1.11:6379 · 1 of 3`), not only in panel titles. The parts drop by rank when
  the row is narrow, as the header does today.
- Dashboard, top bar: a connection button (status dot, name, env badge) that opens a list of the
  saved connections to switch to; an access button (Read-only / Writes on); a database select;
  a node select on a cluster; the Go to box; the Live button with the interval; the `?` button.
  The database and node controls leave the individual screens.

## 2. Two rows of chrome in the terminal

The header, the tab row and the sub-tab row become two rows. Row 2 is the areas with the current
area's panels inline:

    1 Overview   2 Browse › Keys · Streams · Search   3 Analyze   4 Server   5 Pub/Sub      0 Connections

The current area is bold with its digit as a key cap, the current panel is in the focus colour,
and `0 Connections` sits at the right. The body gets the row back.

## 3. Focus you can see

- The focused pane has its border in the focus colour and its title bold. Other panes keep the
  normal border and a dim title.
- Every list draws `›` in a 2-cell gutter on its selected row, as well as the selection
  background. No list relies on the background alone.

## 4. Colour roles

| Role | Terminal (dark) | Dashboard token (dark) | Used for |
|---|---|---|---|
| Focus | `#6cb6ff` | `--accent` | Focused border, `›`, current panel, links, key names in JSON |
| Placeholder | `#b79cf5` | `--ph` | `{id}` parts of a pattern |
| Warning | `#e8b45c` | `--warn` | `▲` findings and notes, `read-only`, `never` in an expiry column |
| Danger | `#f07662` | `--danger` | PROD badge, `writes on`, errors, destructive actions |
| Ok | `#5fc6b4` | `--ok` | `●` connected / analyzed, live |
| Data | `#4f8fd6` | `--bar` | Bars and meters |

Amber no longer means focus. The light themes get the same roles; every text pair still meets
WCAG AA (the dashboard's `theme.test.js` checks it). The dashboard keeps system fonts: its
content policy allows nothing else.

## 5. One key grammar

These letters mean one thing on every screen, in both UIs. A screen may add its own verbs on
other keys, but never gives one of these another meaning.

| Key | Meaning |
|---|---|
| `↑↓` `j` `k`, `PgUp` `PgDn`, `Home` `End` `g` `G` | Move |
| `→` `l` `enter` | Open, or move focus to the pane on the right |
| `←` `h` `esc` | Back, or move focus to the pane on the left |
| `1`–`5`, `0` | Area. The current area's digit again: its next panel |
| `tab` `shift-tab` | Next / previous panel of the area |
| `/` | Filter the focused list (on Search: the query) |
| `s` | Sort (was `o`) |
| `v` | More columns |
| `m` | Load more rows (was `n`) |
| `a` | Analyze the keys (was `r` on Overview, the analysis panels and Streams) |
| `r` | Refresh: read again now. Never anything else |
| `space` | Pause or resume whatever is live on the screen (was `p`) |
| `y` / `Y` | Copy the selected row's thing / copy its parent's (a field's value / its key name; a pattern / its scan command) |
| `e` | Edit the selected row |
| `+` | Add: a connection, a key, a field or element, a subscription, an index |
| `t` / `T` | Set a TTL on the selected row / on its parent |
| `d` / `D` | Delete the selected row / its parent |
| `R` | Rename the key |
| `f` | Show the value as another format |
| `w` | Writes on or off, from any screen (the same question as today) |
| `b` | Database list |
| `[` `]` | Previous / next node |
| `.` | Actions menu |
| `ctrl-k` | Go to |
| `ctrl-f` (terminal), `/` in the Go to box (dashboard) | Find key |
| `:` | Command line |
| `?` | Keys |
| `ctrl-s` | Save a snapshot (was `s`) |
| `q`, `ctrl-c` | Quit (terminal) |

**Lower case acts on the selected row, upper case on the thing it belongs to.** With a field
selected, `d` deletes the field and `D` the key. With a key selected, `d` deletes the key and `D`
every key of the pattern (the bulk action). With a pattern selected, `d` is the bulk delete and
`t` the bulk TTL (they were `D` and `x`). Every one of these still goes through the write gate
and, for a bulk action, the dry run.

Screen verbs, on keys the grammar leaves free:

| Screen | Keys |
|---|---|
| Overview | `<` `>` slower / faster interval (was `-` `+`) |
| Keys of a pattern | `ctrl-t` type filter (was `t`) |
| Clients | `d` ends the selected connection (was `K`) |
| Slow log | `e` edits the threshold (was `c`), `D` resets the log (was `R`) |
| Streams | `c` claim, `A` ack / pending, `-` trim (was `X`) |
| Pub/Sub | `+` subscribe (was `s`), `S` shard subscribe, `P` publish, `ctrl-l` clear the tail (was `c`) |
| Search | `x` explain (was `e`), `X` profile (was `P`), `+` create an index (was `C`), `d` drop the selected index (was `D`) |
| Connections | `+` add (was `a`) |

Questions keep their keys: `y` / `n` for a yes-or-no, the typed name and `enter` on production.

## 6. Footer

- Left: at most five keys for the focused pane, and only keys that do something in the current
  state. A view that is off shows no key that needs it; a read-only connection shows no write
  key; a drill-down shows its way back.
- Right, always: `. Actions   ^K Go to   ? Keys`. `ctrl-f` and `:` leave the footer (the Go to
  box and the Keys sheet list them).

## 7. Actions menu (`.`)

A menu of every action for the selected row and for its parent, in two groups, each line with
its key, its name and, for a write, the command it sends. Built from the key table, so it cannot
list a key that does not exist. An action that is off stays listed, dim, with the reason at the
right (`off: read-only`, `off: needs Redis 7.4`). A write that asks first says `asks first`.
`↑↓` and `enter` run a line; the line's own key runs it too; `esc` closes.

Dashboard: the same list drives the buttons of a pane. Each button shows its key, and a button
that is off shows its reason as text under the bar, not only in a tooltip.

## 8. Go to (`ctrl-k`)

One box. Results in groups, most specific first:

1. **Patterns** of the analysis (or the names read so far) whose name contains the text.
2. **Keys** already loaded whose name contains the text, then one line `Find every key matching
   *text*` that runs Find key. Typing reads nothing from the server.
3. **Screens**: every area and panel, with the keys that reach it.
4. **Actions** of the current screen, with their keys; off ones with their reason.

`↑↓` pick, `enter` goes, `tab` jumps to the next group, `esc` closes. Text that starts with `:`
opens the command line with the rest.

## 9. Browse in three panes

From 120 columns (terminal) and from a wide window (dashboard), Browse › Keys shows Patterns,
the keys of the open pattern and the open key side by side. `←` `→` move the focus. Nothing is
read as the cursor moves: a pane fills when its parent row is opened, exactly as the screens do
today, and until then the pane to the right of the focus shows the summary the right-hand pane
shows today. Below 120 columns the terminal keeps two panes.

Dashboard: the three panes fill the window height and scroll inside themselves; the open
pattern, key and Find text are in the URL, so Back undoes a drill-down and a link reopens it.
Rows are 28px.

## 10. Screens that use their height

- Overview: six live figures in a 3 × 2 grid with a memory bar; below, the findings on the left
  with the selected finding explained under the list, and on the right the memory by pattern,
  what expiry frees, and the slowest commands the slow log already holds.
- Slow log, Clients, Monitor, Nodes: from 120 columns the detail of the selected row sits to the
  right of the list, not under it.

## 11. One confirm

Every question that guards a write or a costly read is the same sheet, docked to the bottom of
the body: what it acts on, `Sends <command>`, whether it can be undone, then `y` / `n`, or the
typed name on production. Monitor's question and the command line's question use it too. The
Analyze and database dialogs are choices, not confirms, and stay centred.

## 12. Dashboard chrome

- A left rail with the areas and their digits as key hints; Connections and the command line
  under a rule.
- The command line is a drawer docked to the bottom of the window on every screen, opened with
  `:` and closed with `esc`, not a card under the page.
- Compact density: 28px rows, 30px buttons and fields.
- `?` opens a sheet of the keys for the current screen, built from the same table as the
  buttons' key hints.
