# Dashboard API

The JSON API and event stream the web dashboard's page talks to (`src/web/`). The page in
`src/web/assets/` is the only client; this file is the contract between the two.

Every example reply below was captured from a real run: a test that started the dashboard
with `redis_lens::web::start_with` against a Redis 7 container, a temporary config file and
an in-memory credential store (`MemoryStore`). Ports and times are from that run. Long lists
are cut, and each place that is cut says so. The examples of Browse, Analyze and snapshots
were captured by `tests/web_p2_it.rs` (see [Browse, Analyze and snapshots](#browse-analyze-and-snapshots)).

## Listening and signing in

- The dashboard binds `127.0.0.1:<port>` only (default 7379; port 0 picks a free one).
  A port in use is `StartError::PortInUse`.
- Each run makes a session token: 32 random bytes from the OS, written as 64 hex digits.
  `Debug`, logs and every reply leave it out.
- The sign-in URL is `http://127.0.0.1:<port>/#token=<64 hex digits>`
  (`Dashboard::open_url`). The token is in the fragment, which a browser never sends to a
  server. `redis-lens --dashboard` prints it once, on its `Dashboard:` line. Nothing else
  shows it.
- To open a browser (`ctrl-o` in the TUI, `--dashboard` without `--no-open`), the dashboard
  writes a small HTML file that sends the browser on to the sign-in URL (a meta refresh,
  and a link if the browser does not follow it). The file goes in a new private directory:
  on unix the directory is mode `0700`, the file `0600`, and both have random names, under
  `$XDG_RUNTIME_DIR` or the temp directory; on Windows it goes under the user's temp
  directory. The opener (`open`, `xdg-open` or `rundll32 url.dll,FileProtocolHandler`) is
  given that file's path, so the token is never in a process argument. The file is deleted
  after 30 s, and when the dashboard stops.
- The page reads the token from `location.hash`, keeps it in memory, and removes it from
  the address bar. It then sends it on every API call as `Authorization: Bearer <token>`,
  the event stream included (the page reads `/api/events` with `fetch`, not `EventSource`).
  There is no cookie and no `/auth/...` route: `GET /auth/<anything>` is 404.
- `GET /` serves `index.html` and `GET /assets/<file>` serves the embedded files on a fixed
  list (404 for anything else). Both work without the token, because they are the page's
  static shell with no data in them. The `Host` check still applies. `.js` is
  `text/javascript; charset=utf-8`. `locked.html` is on the list and served at
  `/assets/locked.html`, but `/` no longer serves it. `mock.js` (the page's invented servers)
  is served by debug builds only; a release build answers 404 for it. Debug builds only:
  `REDIS_LENS_DASHBOARD_ASSETS=<dir>` serves the same names from that directory, so the page
  can be changed without a rebuild.

## Checks on every request

They run in this order, before any route:

1. `Host` must be `127.0.0.1:<port>` or `localhost:<port>`. Anything else gets 403
   `{"error":"forbidden"}`. This stops DNS rebinding. A request line in absolute form
   (`GET http://host:port/path`) must name this server the same way (`http`, and
   `127.0.0.1:<port>` or `localhost:<port>`), and the authority form (`CONNECT`) is refused.
   Anything else gets 403.
2. Any method other than `GET` and `HEAD` must have `Origin: http://127.0.0.1:<port>` (or
   `http://localhost:<port>`), the header `X-Redis-Lens: 1` and
   `Content-Type: application/json`. Anything else gets 403 `{"error":"forbidden"}`. That
   includes `OPTIONS`: there is no CORS preflight, and no response carries an
   `Access-Control-*` header.
3. Every `/api/...` route needs exactly one `Authorization: Bearer <token>` header (the
   scheme in any case), compared in constant time. Without it, with a wrong token, with two
   such headers or with the old `rl_session` cookie, the reply is 401
   `{"error":"not signed in"}`, with no detail.

Connections: hyper's header read timeout is 10 s (a connection that has not sent its
headers by then is closed). At most 64 connections are open at once; the next waits until
one closes.

Every response, errors and assets included, carries:

```
Cache-Control: no-store
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
X-Frame-Options: DENY
Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'
```

The server logs nothing.

Example, `GET /api/state (no token)`. Reply: HTTP 401

```json
{
  "error": "not signed in"
}
```

Example, `GET /api/state (wrong Host)`. Reply: HTTP 403

```json
{
  "error": "forbidden"
}
```

## Conventions

- Bodies are JSON. An empty body counts as `{}`. A body this route does not take
  (wrong shape, unknown field) gets 400 `{"error":"the request body is not the JSON this route takes"}`.
  The input is never echoed back.
- A query string this route does not take (a parameter missing, or not of its type, such
  as `GET /api/find` without `q`, `GET /api/value/item?view=abc` or
  `GET /api/analysis/slots?refresh=true`) gets 400
  `{"error":"the query parameters are not the ones this route takes"}`, as JSON like every
  other refusal. The input is never echoed back.
- Errors are `{"error": "<text>"}`.
- Status codes:
  - 200: done.
  - 201: a connection was added, or a snapshot saved.
  - 202: started, with the result arriving as events (connect, subscribe, Monitor, an
    analysis, a database switch).
  - 400: a body or a query string this route does not take (also a body over 2 MiB), or
    a change `conn add`/`conn set` refuses (the reason is in `error`).
  - 404: no such connection or route.
  - 409: there is no live connection (`{"error":"not connected: connect first, or the live connection is closed"}`),
    a confirm is needed (see below), or a command line is still running
    (`POST /api/cmdline`).
  - 413: a command line longer than 8,192 characters.
  - 410: a confirm id that was used, cancelled or expired.
  - 422: refused by the write gate or by a rule (read-only profile, no such node, an
    interval the TUI does not have, Monitor can't run here, a typed name that does not
    match), or a question that could not be put in the dashboard's words
    (`{"error":"That needs a question the dashboard cannot ask, so nothing was changed."}`).
  - 502: the server failed the request, or did not answer within 60 s.
- When the server or the user lacks a feature (`NOPERM`, a command Valkey or an old Redis
  does not have), the panel's route answers 200 `{"off": "<why>"}` instead of failing.
  This follows the rule "degrade instead of failing".
- Every text that came from the server (keys, client names, channel names, reply values,
  command lines) is escaped with `value::escape` before it goes in a reply.
- Connection URLs are always masked: the user name and password are never in a reply.
  `auth` says which kind of credential the profile uses. `endpoint` is masked too. A host that is not a plain host name (it holds `@`, `/`, `?`,
  `#`, a space and the like, as an old config file might) is shown as
  `(an address with a user name or password in it: add the connection again)`, in both
  `url` and `endpoint`.
- Texts that quote a profile name, a host or a tag keep them as they are: a name such as
  `cache--eu` or a punycode host such as `xn--bcher-kva.example` never turns a message into
  the fallback sentence. A confirm question is never replaced by the fallback: the request
  fails with 422 instead (above).
- Server error text never reaches a reply or an event: a leading all-capitals code (quoted
  or not, `MISCONF ...` or `"MISCONF": ...`), a quoted code anywhere, or a client-library
  error kind becomes one of three fixed sentences (needs a password, wrong credentials, or
  `the server refused the connection`).
- No reply carries the server's own error text (rule 3: it can name the ACL user). A
  permission check's reason, a connect that failed, a tail or Monitor connection that could
  not open: each is said in redis-lens's own words. The one exception is a command typed
  on the command line: its own reply, error replies included, is in `replies`.
- Texts are in the dashboard's terms: no TUI key (`w`, `esc`), screen number (`(3)`) or CLI
  flag (`--force`) is ever in one. The TUI and the CLI say the same things their way.
- Command lines from the server (slow log, Monitor) and the user's own lines are masked by
  the TUI's own masking code (`live::command::masked_line`): secret arguments, such as
  those of `AUTH` and `CONFIG SET masterauth`, are shown as `***`.

## Confirms (write questions)

A write that the gate (spec "Write permissions") would ask about in the TUI gets 409 instead,
and so does starting Monitor (it always asks) and turning writes on:

```json
{"confirm": {"id": "<32 hex>", "kind": "ask" | "type", "text": "<question>", "expect": "<name to type>" | null}}
```

- `kind: "ask"` is a yes/no question. `kind: "type"` needs a name typed, and `expect`
  holds it: the profile's name to turn writes on for a prod profile, the key's name for
  a destructive write on prod (deleting a key, a field, an element), the pattern for a
  bulk action on prod.
- The question of a destructive write (one that deletes data) ends with "This can't be
  undone.", the TUI's warning; this holds for a command line's destructive command too.
- `POST /api/confirm/<id>` with `{}` (ask) or `{"typed": "<name>"}` (type) answers yes. The
  gate checks again at that point, with the profile's `read_only` and `env` read again from
  the config file: a different connection (409 or 410), a new run of the same connection
  since the question (a database switch or an analysis started: 409
  `{"error":"Not sent: the connection changed since the question was asked."}`), another
  node picked since, for a write sent to the picked node (`FT.CREATE`, `FT.DROPINDEX`: the
  same 409), a bulk action that counts or runs (409, for every write but a typed command's),
  a profile that is now read-only (422), or a wrong typed name means it is not sent. The same holds
  for a `@dangerous` command line's question, and for a repeated write that a new run
  interrupts.
- `DELETE /api/confirm/<id>` cancels.
- Each id can be used once and expires after 2 minutes. After that both routes answer 410.
  Expired questions, and what they held, are dropped every 10 s even when no request comes.
  Connecting or disconnecting clears every open confirm.

Example, `PATCH /api/connections/prod-cache {"writes":true}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": "prod-cache",
    "id": "3c93343b42bfe866aac73ae898294baf",
    "kind": "type",
    "text": "prod-cache is a prod profile. Type its name to turn writes on."
  }
}
```

Example, `POST /api/confirm/<id> {"typed":"prod-cache"}`. Reply: HTTP 200

```json
{
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:61725",
    "env": "prod",
    "env_label": "PROD",
    "name": "prod-cache",
    "prefer_replica": false,
    "provider": null,
    "provider_notice": null,
    "read_only": false,
    "sentinel": null,
    "sentinel_tls": false,
    "ssh": null,
    "standalone": false,
    "tags": [],
    "tls": false,
    "token_command": false,
    "url": "redis://127.0.0.1:61725/0",
    "writes": true
  },
  "message": "Writes are on for prod-cache. Each write still asks as the table in the docs says."
}
```

Example, `DELETE /api/confirm/<id>`. Reply: HTTP 200

```json
{
  "cancelled": true,
  "text": "Cancelled: nothing was sent."
}
```

## Events: `GET /api/events`

A `text/event-stream` (SSE) stream with a keep-alive comment every 15 s. Each event's
`event:` field is its name and `data:` holds JSON. It ends when the dashboard shuts down.
The page opens one stream per tab. Like every `/api/...` route it needs
`Authorization: Bearer <token>`, so the page reads it with `fetch` and a stream reader
(`EventSource` cannot send that header). Without the token it is 401, as JSON, not a
stream.

| Event | When | Data |
|---|---|---|
| `state` | first on every stream, then on every change of connection, phase, node, tail or Monitor, and when the server refuses a write for this user (`refused` grows) | the same body as `GET /api/state` |
| `figures` | at the polling interval (2 s to start with, see `POST /api/polling`), only while at least one stream is open, polling is not paused and the live connection is ready | `{node, info, rates, shards}` for the picked node, where `shards` is every node's headline on a cluster (refreshed less often) |
| `tail` | pub/sub tail | `{tail, kind: "ready"}`, `{tail, kind: "messages", messages: [...], dropped}`, `{tail, kind: "closed", error}` |
| `monitor` | Monitor run | `{run, kind: "ready", at_unix_ms, max_lines, max_secs}`, `{run, kind: "lines", lines: [...]}`, `{run, kind: "closed", end: "stopped"\|"line_cap"\|"time_up"\|"lost"\|"failed", error}` |
| `overview` | when what the connect read for Overview changes: the connect's slow log and client list landing after the live connection is ready (they fill `findings`), another node picked (its findings go until it is read), the last analysis found, the connect's figures. Sent only when the data differs from the last `overview` event of this connection, never before the connect has read anything | the body of `GET /api/overview` without `live` and `shards` (those come with `figures`) |
| `progress` | an analysis: started, sampling, stopping, done, stopped, failed, and compared with the last snapshot (see [The `progress` event](#the-progress-event)) | `{run, kind, ...}` |
| `bulk` | a bulk action: after each step of the dry run's walk (one `SCAN` step and its type and TTL reads), counted, after each batch of the run, done, closed, and when a connect or a disconnect replaces the session it was of (see [Bulk actions](#bulk-actions)) | `{"bulk": <the body of GET /api/bulk>}`, `{"bulk": null}` once it is closed or its session is gone |
| `notice` | something the user should know that has no panel, such as the live connection closing or a stream that fell behind | `{"text": "..."}` |

A stream that falls more than 1024 events behind skips them and gets a `notice` instead.

Example, `event: state (first on the stream, not connected)`. Event (`$.polling.intervals` cut to the first 3 of 8)

```json
{
  "analysis": {
    "db": 0,
    "depth": null,
    "error": null,
    "note": null,
    "previous": false,
    "report": null,
    "run": 0,
    "sampled": null,
    "started_unix": null,
    "status": "none",
    "target": null
  },
  "connected_at_unix": null,
  "connection": null,
  "docker": false,
  "error": null,
  "features": {
    "entra_id": false,
    "version": "0.1.0"
  },
  "live": "waiting",
  "live_error": null,
  "monitor": null,
  "monitor_limits": {
    "max_lines": 10000,
    "max_secs": 30
  },
  "nodes": [],
  "picked": 0,
  "polling": {
    "interval_secs": 2,
    "intervals": [
      1,
      2,
      3
    ],
    "paused": false
  },
  "refused": [],
  "sentinel": null,
  "server": null,
  "status": "idle",
  "tail": null,
  "target": null,
  "writes": false,
  "writes_off": null
}
```

Example, `event: figures`. Event

```json
{
  "info": {
    "aof_ok": true,
    "aof_on": false,
    "blocked": 0,
    "clients": 2,
    "commands": 70,
    "evicted": 0,
    "expired": 0,
    "fragmentation": 6.26,
    "hits": 4,
    "keys": 1,
    "link_up": null,
    "maxmemory": 0,
    "misses": 0,
    "ops_now": 0,
    "policy": "noeviction",
    "rdb_ok": true,
    "replicas": 0,
    "role": "primary",
    "uptime_secs": 2,
    "used_memory": 1548008
  },
  "node": 0,
  "rates": {
    "blocked": 0,
    "clients": 2,
    "cpu_sys": 0.10089701482056067,
    "cpu_user": 0.2890455010476239,
    "evicted": 0.0,
    "expired": 0.0,
    "fragmentation": 6.26,
    "hit_rate": null,
    "net_in": 6.222723381003744,
    "net_out": 2481.533188295993,
    "ops": 0.44448024150026744,
    "used_memory": 1548008
  }
}
```

## Routes

| Method | Path | Does |
|---|---|---|
| GET | `/api/state` | the connection, phase, nodes, picked node, tail, Monitor, refused commands |
| GET | `/api/events` | the event stream |
| GET | `/api/connections` | saved profiles (masked) |
| POST | `/api/connections` | add a profile (what `conn add` does) |
| PATCH | `/api/connections/{name}` | change writes, tags, standalone (what `conn set` does) |
| DELETE | `/api/connections/{name}` | remove a profile and its keychain entries |
| POST | `/api/connections/{name}/test` | connect once and check permissions (what `conn test` does) |
| GET | `/api/docker` | Redis/Valkey containers found on the local Docker |
| POST | `/api/connect` | connect to a profile or a container |
| POST | `/api/disconnect` | close the connection |
| POST | `/api/node` | pick the cluster node the per-node panels read |
| POST | `/api/session/writes` | writes on or off for the session's connection (a container's for this session only) |
| GET | `/api/overview` | figures, permissions, shards, findings, the last analysis |
| POST | `/api/polling` | pause or resume the live figures, set their interval |
| GET | `/api/server/clients` | `CLIENT LIST`, grouped |
| GET | `/api/server/slowlog` | `SLOWLOG GET`, masked, plus the slow-log settings |
| POST, DELETE | `/api/server/monitor` | ask to start (409), stop a Monitor run |
| GET | `/api/pubsub/channels` | active channels and their subscriber counts |
| POST, DELETE | `/api/pubsub/subscribe` | start, stop the live tail |
| POST | `/api/pubsub/publish` | `PUBLISH`/`SPUBLISH` through the write gate |
| POST | `/api/cmdline` | run a typed command line under the TUI's rules |
| GET | `/api/cmdline/commands` | command names for completion |
| GET | `/api/cmdline/help?name=` | a command's help |
| GET | `/api/dbs` | the databases and their key counts |
| POST | `/api/db` | switch the session's database |
| GET | `/api/browse` | Browse › Keys: the pattern tree, from key names or an analysis |
| POST | `/api/browse/more` | one more page of key names |
| GET | `/api/keys?pattern=` | a pattern's keys |
| GET | `/api/find?q=` | Find key |
| GET | `/api/keys/more?list=` | a key list's next page |
| GET | `/api/value?key=` | open a key's value |
| GET | `/api/value/more?view=` | a value's next page |
| GET | `/api/value/item?view=&index=` | one item, whole, in a format |
| GET, POST, DELETE | `/api/analysis` | the analysis and its estimates; start one; stop it |
| GET | `/api/analysis/big_keys` | Big keys and "Things to look at" |
| GET | `/api/analysis/expiry` | Expiry |
| GET | `/api/analysis/changes` | Changes against the last snapshot |
| GET | `/api/analysis/hot_keys` | Hot keys |
| GET | `/api/analysis/nodes` | Nodes (a cluster) |
| GET | `/api/analysis/slots` | Slots (a cluster) |
| GET, POST | `/api/snapshots` | list, save snapshots |
| GET | `/api/snapshots/compare?a=&b=` | compare two snapshots |
| POST | `/api/value/edit` | edit a string, a hash field, a list element, a set member, a sorted set score |
| POST | `/api/value/add` | add a field, element, member or stream entry |
| POST | `/api/value/ttl` | set or remove the key's TTL, or a hash field's |
| POST | `/api/value/rename` | rename the key (`RENAMENX`) |
| POST | `/api/value/remove` | delete one field, element, member or entry |
| POST | `/api/value/delete` | delete the key (`UNLINK`) |
| POST | `/api/keys/add` | add a key with its first item |
| GET, POST, DELETE | `/api/bulk` | the bulk action; count one (the dry run); stop or close it |
| POST | `/api/bulk/run` | run the counted bulk action (asks first) |
| GET | `/api/streams` | Browse › Streams: the analysis's streams, groups and consumers, each read on its owner |
| GET | `/api/streams/pending?key=&group=` | a group's pending entries (`XPENDING`) |
| POST | `/api/streams/ack` | `XACK` an entry of the pending list shown |
| POST | `/api/streams/claim` | `XAUTOCLAIM` a group's pending entries for a consumer |
| POST | `/api/streams/trim` | `XTRIM` by `MAXLEN` or `MINID` (asks first) |
| GET | `/api/search` | Browse › Search: the indexes of the picked node and their `FT.INFO` |
| POST | `/api/search/query` | `FT.SEARCH`, `FT.EXPLAIN` or `FT.PROFILE` |
| GET | `/api/search/results?run=&from=` | a page of the last results |
| GET | `/api/search/field?run=&doc=&field=` | one field of a result, whole, in a format |
| POST | `/api/search/create` | `FT.CREATE` from the form |
| POST | `/api/search/drop` | `FT.DROPINDEX` without `DD` (asks first) |
| GET | `/api/analysis/hot_keys/monitor` | Hot keys ranked from the last Monitor run |
| POST, DELETE | `/api/confirm/{id}` | answer or cancel a question |

### `GET /api/state`

`status` is `idle`, `connecting`, `connected` or `failed` (`error` says why). `live` is
`waiting`, `ready` or `closed` (`live_error`). `writes` is true when this connection may
write, and `writes_off` says why not. For a saved profile both follow the config file:
this route, every write and every answer read its `read_only` and `env` again, and a
change sends a `state` event (a file that cannot be read, or that no longer has the
profile, turns writes off). A Docker container's session-only setting stays as set.
`refused` lists the commands the permission check
turned off, and the writes the server refused this session (each one sends a `state`
event). `nodes` and `picked` describe a cluster (the first primary is picked when the
nodes arrive). `polling` is how the live figures are polled (`paused`, `interval_secs`,
and the `intervals` that can be set). `monitor_limits` is when a Monitor run stops by itself
(`max_secs`, `max_lines`), known before one starts. `features.entra_id` says whether this
build has Entra ID. `analysis` is the analysis and its report's age (see
[`state.analysis`](#stateanalysis)).

A connect that opens but can't authenticate (the server wants a password and none was sent,
or it refused the one sent) is `failed`, with a fixed reason, never `connected`.

Example, `GET /api/state`. Reply: HTTP 200 (`$.polling.intervals` cut to the first 3 of 8)

```json
{
  "analysis": {
    "db": 0,
    "depth": null,
    "error": null,
    "note": null,
    "previous": false,
    "report": null,
    "run": 0,
    "sampled": null,
    "started_unix": null,
    "status": "none",
    "target": null
  },
  "connected_at_unix": null,
  "connection": null,
  "docker": false,
  "error": null,
  "features": {
    "entra_id": false,
    "version": "0.1.0"
  },
  "live": "waiting",
  "live_error": null,
  "monitor": null,
  "monitor_limits": {
    "max_lines": 10000,
    "max_secs": 30
  },
  "nodes": [],
  "picked": 0,
  "polling": {
    "interval_secs": 2,
    "intervals": [
      1,
      2,
      3
    ],
    "paused": false
  },
  "refused": [],
  "sentinel": null,
  "server": null,
  "status": "idle",
  "tail": null,
  "target": null,
  "writes": false,
  "writes_off": null
}
```

Example, `GET /api/state (after connecting)`. Event (`$.polling.intervals` cut to the first 3 of 8)

```json
{
  "connected_at_unix": 1794729495,
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:61725",
    "env": "local",
    "env_label": "LOCAL",
    "name": "cache",
    "prefer_replica": false,
    "provider": null,
    "provider_notice": null,
    "read_only": false,
    "sentinel": null,
    "sentinel_tls": false,
    "ssh": null,
    "standalone": false,
    "tags": [
      "team-a"
    ],
    "tls": false,
    "token_command": false,
    "url": "redis://127.0.0.1:61725/0",
    "writes": true
  },
  "docker": false,
  "error": null,
  "features": {
    "entra_id": false,
    "version": "0.1.0"
  },
  "live": "ready",
  "live_error": null,
  "monitor": null,
  "monitor_limits": {
    "max_lines": 10000,
    "max_secs": 30
  },
  "nodes": [],
  "picked": 0,
  "polling": {
    "interval_secs": 2,
    "intervals": [
      1,
      2,
      3
    ],
    "paused": false
  },
  "refused": [],
  "sentinel": null,
  "server": {
    "flavor": "redis",
    "keys": 1,
    "lfu": false,
    "maxmemory": 0,
    "mode": "standalone",
    "policy": "noeviction",
    "used_memory": 1454280,
    "version": "7.4.12"
  },
  "status": "connected",
  "tail": null,
  "target": "primary",
  "writes": true,
  "writes_off": null
}
```

Example, `GET /api/state (a connect to a server that wants a password, without one)`. Reply: HTTP 200 (`$.polling.intervals` cut to the first 3 of 8)

```json
{
  "analysis": {
    "db": 0,
    "depth": null,
    "error": null,
    "note": null,
    "previous": false,
    "report": null,
    "run": 1,
    "sampled": null,
    "started_unix": null,
    "status": "none",
    "target": null
  },
  "connected_at_unix": null,
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:61726",
    "env": "local",
    "env_label": "LOCAL",
    "name": "locked",
    "prefer_replica": false,
    "provider": null,
    "provider_notice": null,
    "read_only": false,
    "sentinel": null,
    "sentinel_tls": false,
    "ssh": null,
    "standalone": false,
    "tags": [],
    "tls": false,
    "token_command": false,
    "url": "redis://127.0.0.1:61726/0",
    "writes": true
  },
  "docker": false,
  "error": "the server asks for a password, and this connection has none: add the connection again with its password",
  "features": {
    "entra_id": false,
    "version": "0.1.0"
  },
  "live": "closed",
  "live_error": null,
  "monitor": null,
  "monitor_limits": {
    "max_lines": 10000,
    "max_secs": 30
  },
  "nodes": [],
  "picked": 0,
  "polling": {
    "interval_secs": 2,
    "intervals": [
      1,
      2,
      3
    ],
    "paused": false
  },
  "refused": [],
  "sentinel": null,
  "server": null,
  "status": "failed",
  "tail": null,
  "target": null,
  "writes": true,
  "writes_off": null
}
```

### `GET /api/connections`

`connections` holds the saved profiles, and `connected` is the name connected to (or null).
There are no credential fields: `auth` is `none`, `keychain` (a user name or password is saved there), `command`
(a token command) or `entra-id`, and `url` is masked.

Example, `GET /api/connections`. Reply: HTTP 200 (`$.connections` cut to the first 3 of 5)

```json
{
  "connected": null,
  "connections": [
    {
      "auth": "none",
      "db": 0,
      "endpoint": "127.0.0.1:61725",
      "env": "local",
      "env_label": "LOCAL",
      "name": "cache",
      "prefer_replica": false,
      "provider": null,
      "provider_notice": null,
      "read_only": false,
      "sentinel": null,
      "sentinel_tls": false,
      "ssh": null,
      "standalone": false,
      "tags": [
        "team-a"
      ],
      "tls": false,
      "token_command": false,
      "url": "redis://127.0.0.1:61725/0",
      "writes": true
    },
    {
      "auth": "none",
      "db": 0,
      "endpoint": "127.0.0.1:61725",
      "env": "prod",
      "env_label": "PROD",
      "name": "prod-cache",
      "prefer_replica": false,
      "provider": null,
      "provider_notice": null,
      "read_only": true,
      "sentinel": null,
      "sentinel_tls": false,
      "ssh": null,
      "standalone": false,
      "tags": [],
      "tls": false,
      "token_command": false,
      "url": "redis://127.0.0.1:61725/0",
      "writes": false
    },
    {
      "auth": "none",
      "db": 0,
      "endpoint": "127.0.0.1:61725",
      "env": "prod",
      "env_label": "PROD",
      "name": "prod-rw",
      "prefer_replica": false,
      "provider": null,
      "provider_notice": null,
      "read_only": false,
      "sentinel": null,
      "sentinel_tls": false,
      "ssh": null,
      "standalone": false,
      "tags": [],
      "tls": false,
      "token_command": false,
      "url": "redis://127.0.0.1:61725/0",
      "writes": true
    }
  ]
}
```

### `POST /api/connections`

This adds a profile, as `redis-lens conn add` does: the same checks, and credentials go only
to the OS keychain. `url` is a `redis://` or `rediss://` URL (`valkey://` and `valkeys://`
are the same two), or `host:port[,password=...]`. Any other scheme gets 400
`could not parse the connection string: only redis, rediss, valkey and valkeys URLs are supported`,
and a host that holds a user name, a password or other URL parts gets 400
`could not parse the connection string: the host holds a user name, a password or other URL parts; give the user name and password in a redis:// URL instead`.
Neither echoes the URL, and nothing is saved. While a question about the add waits, its
passwords and URL are held as secrets, and they are dropped when the question is answered,
cancelled or expires. Body (only `name` and `url` are required; unknown fields get 400):

```json
{"name": "staging", "url": "redis://app-user@10.0.0.5:6379/0", "password": "...",
 "env": "prod|stage|local|other", "tags": ["team-b"], "writes": true,
 "provider": "elasticache|elasticache-serverless|memorydb|azure|memorystore",
 "ssh": "user@bastion:22", "ssh_identity": "~/.ssh/id_ed25519", "ssh_known_hosts": "...",
 "sentinel": "10.0.0.1:26379,10.0.0.2:26379", "sentinel_master": "mymaster", "sentinel_tls": false,
 "sentinel_user": "...", "sentinel_password": "...",
 "token_command": ["prog", "arg"], "entra": false, "standalone": false,
 "prefer_replica": false, "force": false}
```

`password` and `sentinel_password` go straight to the keychain. They, and any user name or
password in `url`, are never in a reply or in the config file. A `token_command` is asked
about first (409 `ask`, showing the command). Writes on a `prod` profile need the name typed
(409 `type`). `sentinel` is one string, the Sentinels' `HOST:PORT` separated by commas.
Without `env`, the environment is worked out from the host as `conn add` does. A failure is
400 with `conn add`'s reason, credentials scrubbed and in the dashboard's terms (a name that
is taken: "tick Replace to overwrite it", that is `force`). The reply is 201
`{message, connection}`.

Example, `POST /api/connections {"name":"staging","url":"redis://app-user@10.0.0.5:6379/0","password":"<password>",...}`. Reply: HTTP 201

```json
{
  "connection": {
    "auth": "keychain",
    "db": 0,
    "endpoint": "10.0.0.5:6379",
    "env": "stage",
    "env_label": "STAGE",
    "name": "staging",
    "prefer_replica": false,
    "provider": null,
    "provider_notice": null,
    "read_only": false,
    "sentinel": null,
    "sentinel_tls": false,
    "ssh": null,
    "standalone": false,
    "tags": [
      "team-b"
    ],
    "tls": false,
    "token_command": false,
    "url": "redis://10.0.0.5:6379/0",
    "writes": true
  },
  "message": "Saved staging (10.0.0.5:6379, STAGE). Credentials are in the OS keychain. Tags: team-b."
}
```

Example, `POST /api/connections {"name":"sent","url":"redis://10.0.0.9:6379","sentinel":"10.0.0.7:26379,10.0.0.8:26379","sentinel_master":"mymaster"}`. Reply: HTTP 201

```json
{
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "10.0.0.9:6379",
    "env": "other",
    "env_label": "OTHER",
    "name": "sent",
    "prefer_replica": false,
    "provider": null,
    "provider_notice": null,
    "read_only": false,
    "sentinel": "mymaster via 10.0.0.7:26379 (+1)",
    "sentinel_tls": false,
    "ssh": null,
    "standalone": false,
    "tags": [],
    "tls": false,
    "token_command": false,
    "url": "redis://10.0.0.9:6379/0",
    "writes": true
  },
  "message": "Saved sent (10.0.0.9:6379, OTHER). Asks Sentinel for mymaster via 10.0.0.7:26379 (+1): the primary is found at every connect."
}
```

Example, `POST /api/connections (name taken)`. Reply: HTTP 400

```json
{
  "error": "a profile named staging already exists: tick Replace to overwrite it"
}
```

Example, `POST /api/connections (token command: asks first)`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "548b353dac1fc9219ab61eff51c0b274",
    "kind": "ask",
    "text": "This profile will run this command at every connect and send what it prints as the password: get-token --scope redis. It is run directly, not through a shell. Save it?"
  }
}
```

### `PATCH /api/connections/{name}`

The body is `{"writes": bool, "tag": [..], "untag": [..], "standalone": bool}` (each part
optional), as in `conn set`. Turning writes on for a prod profile asks for the name to be
typed; on other profiles it asks yes or no; turning writes off never asks. When the profile
is the one connected to, the session's read-only flag and tags change at once and a `state`
event follows. An unknown name is 404, a change `conn set` refuses is 400.

The environment can't be changed here, as `conn set` and the TUI can't change it: it decides
what every write asks, so it is set when the connection is added (add it again with `force`
to change it).

Example, `PATCH /api/connections/prod-cache {"writes":false}`. Reply: HTTP 200

```json
{
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:61725",
    "env": "prod",
    "env_label": "PROD",
    "name": "prod-cache",
    "prefer_replica": false,
    "provider": null,
    "provider_notice": null,
    "read_only": true,
    "sentinel": null,
    "sentinel_tls": false,
    "ssh": null,
    "standalone": false,
    "tags": [],
    "tls": false,
    "token_command": false,
    "url": "redis://127.0.0.1:61725/0",
    "writes": false
  },
  "message": "Writes are off for prod-cache: it is read-only."
}
```

Example, `PATCH /api/connections/sent {"standalone":true}`. Reply: HTTP 400

```json
{
  "error": "Standalone conflicts with a Sentinel profile: Sentinel already names one primary"
}
```

### `POST /api/session/writes`

The body is `{"on": bool}`: writes on or off for the connection the session is on, as `w` on
the TUI's Connections. For a saved profile it is `PATCH /api/connections/{name}`
`{"writes": ...}` (saved to the config file, the same questions). A Docker container has no
saved profile, so its writes change for this session only, as in the TUI: off at once, on
after a question (yes or no, or its name typed on `prod`). Without a session it is 409.

Example, `POST /api/session/writes {"on":false}`. Reply: HTTP 200

```json
{
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:61725",
    "env": "local",
    "env_label": "LOCAL",
    "name": "cache",
    "prefer_replica": false,
    "provider": null,
    "provider_notice": null,
    "read_only": true,
    "sentinel": null,
    "sentinel_tls": false,
    "ssh": null,
    "standalone": false,
    "tags": [
      "team-a"
    ],
    "tls": false,
    "token_command": false,
    "url": "redis://127.0.0.1:61725/0",
    "writes": false
  },
  "message": "Writes are off for cache: it is read-only."
}
```

Example, `POST /api/session/writes {"on":true}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "9b4b493afa8c4949b6e48bccf9deb145",
    "kind": "ask",
    "text": "Turn writes on for cache? Each write still asks before it is sent."
  }
}
```

Example, `POST /api/confirm/<id> (writes on)`. Reply: HTTP 200

```json
{
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:61725",
    "env": "local",
    "env_label": "LOCAL",
    "name": "cache",
    "prefer_replica": false,
    "provider": null,
    "provider_notice": null,
    "read_only": false,
    "sentinel": null,
    "sentinel_tls": false,
    "ssh": null,
    "standalone": false,
    "tags": [
      "team-a"
    ],
    "tls": false,
    "token_command": false,
    "url": "redis://127.0.0.1:61725/0",
    "writes": true
  },
  "message": "Writes are on for cache. Each write still asks as the table in the docs says."
}
```

### `DELETE /api/connections/{name}`

This is `conn remove`: it removes the profile and its keychain entries. An unknown name is 404. When the session is on that profile (connected, connecting or failed), it is closed, its open questions dropped, and the state goes back to `idle`.

Example, `DELETE /api/connections/staging`. Reply: HTTP 200

```json
{
  "message": "Removed staging."
}
```

Example, `DELETE /api/connections/nope`. Reply: HTTP 404

```json
{
  "error": "there is no saved connection by that name"
}
```

Example, `DELETE /api/connections/old (the connected profile)`. Reply: HTTP 200

```json
{
  "message": "Removed old."
}
```

Example, `GET /api/state (after removing the connected profile)`. Reply: HTTP 200 (`$.polling.intervals` cut to the first 3 of 8)

```json
{
  "analysis": {
    "db": 0,
    "depth": null,
    "error": null,
    "note": null,
    "previous": false,
    "report": null,
    "run": 0,
    "sampled": null,
    "started_unix": null,
    "status": "none",
    "target": null
  },
  "connected_at_unix": null,
  "connection": null,
  "docker": false,
  "error": null,
  "features": {
    "entra_id": false,
    "version": "0.1.0"
  },
  "live": "waiting",
  "live_error": null,
  "monitor": null,
  "monitor_limits": {
    "max_lines": 10000,
    "max_secs": 30
  },
  "nodes": [],
  "picked": 0,
  "polling": {
    "interval_secs": 2,
    "intervals": [
      1,
      2,
      3
    ],
    "paused": false
  },
  "refused": [],
  "sentinel": null,
  "server": null,
  "status": "idle",
  "tail": null,
  "target": null,
  "writes": false,
  "writes_off": null
}
```

### `POST /api/connections/{name}/test`

This is `conn test`: it connects once, as the TUI does (keychain entries are read on a blocking thread), and reports the server and the permission check. The permission probes take from the profile's command budget, the one a TUI or dashboard session on the same node takes from. A failed connect, a server that wants a password the profile does not send, or a keychain read that fails, is 502 with a fixed reason. An unknown name is 404.

Example, `POST /api/connections/cache/test`. Reply: HTTP 200 (`$.checks` cut to the first 3 of 6)

```json
{
  "at": "127.0.0.1:61725",
  "checks": [
    {
      "label": "SCAN",
      "reason": null,
      "status": "allowed"
    },
    {
      "label": "TYPE · TTL",
      "reason": null,
      "status": "allowed"
    },
    {
      "label": "MEMORY USAGE",
      "reason": null,
      "status": "allowed"
    }
  ],
  "connected": true,
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:61725",
    "env": "local",
    "env_label": "LOCAL",
    "name": "cache",
    "prefer_replica": false,
    "provider": null,
    "provider_notice": null,
    "read_only": false,
    "sentinel": null,
    "sentinel_tls": false,
    "ssh": null,
    "standalone": false,
    "tags": [
      "team-a"
    ],
    "tls": false,
    "token_command": false,
    "url": "redis://127.0.0.1:61725/0",
    "writes": true
  },
  "sentinel": null
}
```

Example, `POST /api/connections/locked/test (needs a password)`. Reply: HTTP 502

```json
{
  "error": "the server asks for a password, and this connection has none: add the connection again with its password"
}
```

### `GET /api/docker`

This is discovery: it lists running Redis/Valkey containers. A container's password, read from its environment, never leaves the server. When Docker cannot be asked, the reply is 502 with the masked error.

Example, `GET /api/docker`. Reply: HTTP 200

```json
{
  "instances": [
    {
      "auth": "none",
      "container": "dazzling_kare",
      "endpoint": "127.0.0.1:61725",
      "image": "redis:7.4-alpine",
      "url": "redis://127.0.0.1:61725/0"
    },
    {
      "auth": "password",
      "container": "happy_borg",
      "endpoint": "127.0.0.1:61726",
      "image": "redis:7.4-alpine",
      "url": "redis://127.0.0.1:61726/0"
    },
    {
      "auth": "required",
      "container": "redis-lens-demo-redis-1",
      "endpoint": "127.0.0.1:6390",
      "image": "redis:7.4-alpine",
      "url": "redis://127.0.0.1:6390/0"
    },
    {
      "auth": "none",
      "container": "redis-lens-demo-valkey-1",
      "endpoint": "127.0.0.1:6391",
      "image": "valkey/valkey:8.1-alpine",
      "url": "redis://127.0.0.1:6391/0"
    }
  ]
}
```

### `POST /api/connect`, `POST /api/disconnect`

`{"name": "<profile>"}` or `{"docker": "<container>"}`. The connection is made exactly as the
TUI makes it (same code). It replaces the current connection, its tail, its Monitor and its
open confirms. The reply is 202 with the state; progress arrives as `state` events, and
`status` turns to `connected` or `failed`. All tabs share the one connection.

While a bulk action counts or runs (see [Bulk actions](#bulk-actions)), both are 409
`{"error":"A bulk action is running: wait for it to end, or stop it first."}` and nothing changes: the TUI's bulk dialog takes every key until it
ends. A bulk action that waits for its question or has ended goes with the session, and a
`bulk` event of `{"bulk": null}` says so.

Example, `POST /api/connect {"name":"cache"}`. Reply: HTTP 202 (`$.polling.intervals` cut to the first 3 of 8)

```json
{
  "analysis": {
    "db": 0,
    "depth": null,
    "error": null,
    "note": null,
    "previous": false,
    "report": null,
    "run": 1,
    "sampled": null,
    "started_unix": null,
    "status": "none",
    "target": null
  },
  "connected_at_unix": null,
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:61725",
    "env": "local",
    "env_label": "LOCAL",
    "name": "cache",
    "prefer_replica": false,
    "provider": null,
    "provider_notice": null,
    "read_only": false,
    "sentinel": null,
    "sentinel_tls": false,
    "ssh": null,
    "standalone": false,
    "tags": [
      "team-a"
    ],
    "tls": false,
    "token_command": false,
    "url": "redis://127.0.0.1:61725/0",
    "writes": true
  },
  "docker": false,
  "error": null,
  "features": {
    "entra_id": false,
    "version": "0.1.0"
  },
  "live": "waiting",
  "live_error": null,
  "monitor": null,
  "monitor_limits": {
    "max_lines": 10000,
    "max_secs": 30
  },
  "nodes": [],
  "picked": 0,
  "polling": {
    "interval_secs": 2,
    "intervals": [
      1,
      2,
      3
    ],
    "paused": false
  },
  "refused": [],
  "sentinel": null,
  "server": null,
  "status": "connecting",
  "tail": null,
  "target": null,
  "writes": true,
  "writes_off": null
}
```

Example, `POST /api/disconnect`. Reply: HTTP 200 (`$.polling.intervals` cut to the first 3 of 8)

```json
{
  "analysis": {
    "db": 0,
    "depth": null,
    "error": null,
    "note": null,
    "previous": false,
    "report": null,
    "run": 0,
    "sampled": null,
    "started_unix": null,
    "status": "none",
    "target": null
  },
  "connected_at_unix": null,
  "connection": null,
  "docker": false,
  "error": null,
  "features": {
    "entra_id": false,
    "version": "0.1.0"
  },
  "live": "waiting",
  "live_error": null,
  "monitor": null,
  "monitor_limits": {
    "max_lines": 10000,
    "max_secs": 30
  },
  "nodes": [],
  "picked": 0,
  "polling": {
    "interval_secs": 2,
    "intervals": [
      1,
      2,
      3
    ],
    "paused": false
  },
  "refused": [],
  "sentinel": null,
  "server": null,
  "status": "idle",
  "tail": null,
  "target": null,
  "writes": false,
  "writes_off": null
}
```

### `POST /api/node`

The body is `{"index": N}`, an index into `state.nodes`. It picks the node that the overview, clients, slow log, Monitor and channels read. An index that does not exist, or a connection that is not a cluster, is 422.

Example, `POST /api/node {"index":1} (not a cluster)`. Reply: HTTP 422

```json
{
  "error": "there is no such node"
}
```

### `GET /api/overview`

`figures` holds the headline numbers for the connection's report (`figures_failed` says why
when there are none). `perms` is the permission check, and `shards` holds every node's
headline on a cluster. `last_analysis` is the last saved analysis for this profile, when
there is one. `analysis` is Overview's analysis part, `null` until an analysis has a report
(see [`GET /api/overview`: `analysis`](#get-apioverview-analysis)). `server` and `target` describe what was connected to. Live rates come as
`figures` events.

`findings` is "From the server", worked out by the TUI's own code from what the connect read
and the panels read since (no key is read): big keys the server counts (`INFO keysizes`),
the slowest slow-log entry, and many idle connections from one address. Each has
`severity` (`warn`, `note`, `info`), `text`, and where it is looked into: `screen`
(`big_keys`, `expiry`, `changes`, `hot_keys`, `slow_log`, `clients`, ...), with the TUI's
`area` and `panel` names.

Example, `GET /api/overview`. Reply: HTTP 200 (`$.perms` cut to the first 3 of 41)

```json
{
  "analysis": null,
  "figures": {
    "big_keys": null,
    "dataset_bytes": 475200,
    "gaps": [
      "This server has no INFO keysizes (Redis 8 and later have it), so there is no exact size histogram"
    ],
    "keys": 1,
    "overhead_bytes": 1004352,
    "server": {
      "flavor": "redis",
      "keys": 1,
      "lfu": false,
      "maxmemory": 0,
      "mode": "standalone",
      "policy": "noeviction",
      "used_memory": 1454280,
      "version": "7.4.12"
    },
    "ttl_share": 0.0,
    "with_ttl": 0
  },
  "figures_failed": null,
  "findings": [
    {
      "area": "Server",
      "panel": "Slow log",
      "screen": "slow_log",
      "severity": "note",
      "text": "INFO took 85 µs at 07:58 UTC."
    }
  ],
  "last_analysis": null,
  "live": {
    "info": {
      "aof_ok": true,
      "aof_on": false,
      "blocked": 0,
      "clients": 2,
      "commands": 70,
      "evicted": 0,
      "expired": 0,
      "fragmentation": 6.26,
      "hits": 4,
      "keys": 1,
      "link_up": null,
      "maxmemory": 0,
      "misses": 0,
      "ops_now": 0,
      "policy": "noeviction",
      "rdb_ok": true,
      "replicas": 0,
      "role": "primary",
      "uptime_secs": 2,
      "used_memory": 1548008
    },
    "node": 0,
    "rates": {
      "blocked": 0,
      "clients": 2,
      "cpu_sys": 0.10089701482056067,
      "cpu_user": 0.2890455010476239,
      "evicted": 0.0,
      "expired": 0.0,
      "fragmentation": 6.26,
      "hit_rate": null,
      "net_in": 6.222723381003744,
      "net_out": 2481.533188295993,
      "ops": 0.44448024150026744,
      "used_memory": 1548008
    }
  },
  "perms": [
    {
      "label": "SCAN",
      "reason": null,
      "status": "allowed"
    },
    {
      "label": "TYPE · TTL",
      "reason": null,
      "status": "allowed"
    },
    {
      "label": "MEMORY USAGE",
      "reason": null,
      "status": "allowed"
    }
  ],
  "provider": null,
  "server": {
    "flavor": "redis",
    "keys": 1,
    "lfu": false,
    "maxmemory": 0,
    "mode": "standalone",
    "policy": "noeviction",
    "used_memory": 1454280,
    "version": "7.4.12"
  },
  "shards": [],
  "status": "connected",
  "target": "primary"
}
```

### `POST /api/polling`

The body is `{"paused": bool, "interval_secs": N}`, either or both: `p` and `+` `-` on the
TUI's Overview. `interval_secs` is one of 1, 2, 3, 5, 10, 15, 30 and 60 (anything else is
422); it starts at 2. Paused, no `INFO` is sent for the live figures. The polling is the
dashboard's, for every tab, and lasts across connections. The reply is the polling as
`state.polling` shows it, and a `state` event follows.

Example, `POST /api/polling {"paused":true,"interval_secs":5}`. Reply: HTTP 200 (`$.intervals` cut to the first 3 of 8)

```json
{
  "interval_secs": 5,
  "intervals": [
    1,
    2,
    3
  ],
  "paused": true
}
```

Example, `POST /api/polling {"interval_secs":4}`. Reply: HTTP 422

```json
{
  "error": "the interval must be one of 1, 2, 3, 5, 10, 15, 30 or 60 seconds"
}
```

### `GET /api/server/clients`

`CLIENT LIST` on the picked node, grouped as the TUI groups it. Clients the dashboard itself uses are marked `own`. A missing permission gives `{"off": ...}`.

Example, `GET /api/server/clients`. Reply: HTTP 200

```json
{
  "connections": [
    {
      "addr": "192.168.65.1:20270",
      "age": 2,
      "cmd": "set",
      "db": 0,
      "id": 3,
      "idle": 2,
      "lib": "redis-rs 1.8.1",
      "mem": 22400,
      "name": "",
      "own": false
    },
    {
      "addr": "192.168.65.1:58823",
      "age": 2,
      "cmd": "client|list",
      "db": 0,
      "id": 7,
      "idle": 0,
      "lib": "redis-rs 1.8.1",
      "mem": 27546,
      "name": "",
      "own": true
    }
  ],
  "groups": [
    {
      "connections": 2,
      "flagged": false,
      "idle_conns": 0,
      "kind": "lib",
      "label": "redis-rs 1.8.1",
      "longest_idle": 2,
      "mem": 49946,
      "members": [
        0,
        1
      ],
      "oldest_age": 2,
      "top_cmd": "client|list"
    },
    {
      "connections": 2,
      "flagged": false,
      "idle_conns": 0,
      "kind": "addr",
      "label": "192.168.65.0/24",
      "longest_idle": 2,
      "mem": 49946,
      "members": [
        0,
        1
      ],
      "oldest_age": 2,
      "top_cmd": "client|list"
    }
  ],
  "node": 0
}
```

### `GET /api/server/slowlog`

`SLOWLOG GET` on the picked node. Each `line` is masked: the capture ran `CONFIG SET masterauth example-secret` first, and its line is `CONFIG SET masterauth ***`. `settings` holds the threshold and length or `settings_off` says why they are missing (`CONFIG GET` refused), in every reply, not only the first: they are not asked for again once refused. `keys_off` says why the key list is missing, the same way.

Example, `GET /api/server/slowlog`. Reply: HTTP 200 (`$.entries` cut to the first 3 of 71)

```json
{
  "entries": [
    {
      "args_cut": 0,
      "client": "192.168.65.1:20270",
      "client_name": "",
      "command": "CONFIG SET",
      "id": 70,
      "keys": [],
      "line": "CONFIG SET masterauth ***",
      "micros": 11,
      "more_keys": 0,
      "note": null,
      "unix": 1794729497
    },
    {
      "args_cut": 0,
      "client": "192.168.65.1:58823",
      "client_name": "",
      "command": "CLIENT LIST",
      "id": 69,
      "keys": [],
      "line": "CLIENT LIST",
      "micros": 12,
      "more_keys": 0,
      "note": null,
      "unix": 1794729497
    },
    {
      "args_cut": 0,
      "client": "192.168.65.1:58823",
      "client_name": "",
      "command": "INFO",
      "id": 68,
      "keys": [],
      "line": "INFO",
      "micros": 85,
      "more_keys": 0,
      "note": null,
      "unix": 1794729497
    }
  ],
  "keys_off": null,
  "node": 0,
  "settings": {
    "max_len": 128,
    "threshold_micros": 0
  },
  "settings_off": null
}
```

### `POST /api/server/monitor`, `DELETE /api/server/monitor`

Monitor always asks first, as in the TUI, read-only profiles included (it writes nothing
but slows the server, and shows every command, other users' values included): POST answers
409 with a confirm, `ask` (yes or no), or on a `prod` profile `type` with the profile's
name in `expect`. The answer, `POST /api/confirm/<id>`, starts the run on the picked node
through the TUI's runner and answers 202 `{run, max_lines, max_secs}`; a wrong typed name is
422 and nothing starts. The run stops after `max_lines` lines or `max_secs` seconds (also in
`state.monitor_limits`), and lines are masked. Lines come as `monitor` events. POST is 422
at once when a run is going, the live connection is not open, the server has no MONITOR
(some managed services) or the node is one credentials never go to. DELETE stops the run
and answers `{"stopped": bool}`.

Example, `POST /api/server/monitor`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "777060d149828f6a9c6bcb51bbaed49d",
    "kind": "ask",
    "text": "MONITOR shows every command the server runs, other users' values included, and slows the server. It stops after 30 s or 10,000 lines. Start it on cache?"
  }
}
```

Example, `POST /api/confirm/<id> (Monitor)`. Reply: HTTP 202

```json
{
  "max_lines": 10000,
  "max_secs": 30,
  "run": 1
}
```

Example, `POST /api/server/monitor (prod)`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": "prod-rw",
    "id": "8fd3113ec41fcdbcda03e42071c6e763",
    "kind": "type",
    "text": "MONITOR shows every command the server runs, other users' values included, and slows the server. It stops after 30 s or 10,000 lines. Type prod-rw to start it on prod-rw."
  }
}
```

Example, `event: monitor`. Event

```json
{
  "at_unix_ms": 1794729498016,
  "kind": "ready",
  "max_lines": 10000,
  "max_secs": 30,
  "run": 1
}
```

Example, `event: monitor`. Event

```json
{
  "kind": "lines",
  "lines": [
    {
      "client": "192.168.65.1:20270",
      "cut": 0,
      "db": "0",
      "line": "SET user:2 y",
      "millis": 15,
      "unix": 1794729498
    }
  ],
  "run": 1
}
```

Example, `event: monitor`. Event

```json
{
  "end": "stopped",
  "error": null,
  "kind": "closed",
  "run": 1
}
```

Example, `DELETE /api/server/monitor`. Reply: HTTP 200

```json
{
  "stopped": true
}
```

### `GET /api/pubsub/channels`

`PUBSUB CHANNELS` and `NUMSUB` on the picked node, plus `PUBSUB SHARDCHANNELS` on each node
when the server has them (`shards_off` says why not). Dashboard-own subscriptions count like
any other.

Example, `GET /api/pubsub/channels`. Reply: HTTP 200

```json
{
  "channels": [
    {
      "name": "orders",
      "subscribers": 1
    }
  ],
  "node": 0,
  "notes": [],
  "patterns": 0,
  "shards": {
    "nodes": [
      {
        "channels": [],
        "node": 0,
        "notes": [],
        "total_channels": 0
      }
    ],
    "off": null
  },
  "total_channels": 1
}
```

### `POST /api/pubsub/subscribe`, `DELETE /api/pubsub/subscribe`

The body is `{"name": "orders.*", "kind": "auto|channel|pattern|shard"}`. `auto`, the
default, picks a pattern when the name has `*`, `?` or `[`. This starts the live tail through
the TUI's runner, replacing the one that runs. A shard channel is tailed on the node that
owns its slot. The reply is 202 `{tail, kind, node}` (`kind` as resolved), and messages come as `tail` events.
Values are previews (escaped and cut, as in the TUI).

Example, `POST /api/pubsub/subscribe {"name":"orders.*"}`. Reply: HTTP 202

```json
{
  "kind": "pattern",
  "node": 0,
  "tail": 2
}
```

Example, `event: tail`. Event

```json
{
  "kind": "ready",
  "tail": 2
}
```

Example, `event: tail`. Event

```json
{
  "dropped": 0,
  "kind": "messages",
  "messages": [
    {
      "at_unix_ms": 1794729498128,
      "bytes": 9,
      "channel": "orders",
      "pattern": null,
      "preview": "{\"id\":42}"
    }
  ],
  "tail": 3
}
```

Example, `DELETE /api/pubsub/subscribe`. Reply: HTTP 200

```json
{
  "stopped": true
}
```

### `POST /api/pubsub/publish`

The body is `{"channel": "...", "message": "...", "shard": false}`. This is a write: it goes
through the one write gate. A publish is an edit: a read-only profile gets 422, a prod
profile with writes on gets a 409 `ask` question, and a profile with writes on that is not
prod sends at once. While a bulk action counts or runs it is 409
`{"error":"A bulk action is running: wait for it to end, or stop it first."}`, asked or
answered, as every write but a typed command's is. The reply is
`{sent: true, reply}`, where `reply` is the receiver count.

Example, `POST /api/pubsub/publish {"channel":"orders","message":"{\"id\":42}"}`. Reply: HTTP 200

```json
{
  "reply": {
    "kind": "int",
    "value": 1
  },
  "sent": true
}
```

Example, `POST /api/pubsub/publish (prod, writes on)`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "db3fef1fc435f2f6223f2c57f0e93b1d",
    "kind": "ask",
    "text": "Send PUBLISH orders x to prod-rw?"
  }
}
```

Example, `POST /api/pubsub/publish (read-only prod)`. Reply: HTTP 422

```json
{
  "error": "Not sent: prod-cache is read-only. Turn writes on in Connections."
}
```

### `POST /api/cmdline`

The body is `{"line": "<command line>", "pipeline": false}`. It runs under the TUI command
line's rules, using the same code:

- A leading count (`3 GET k`) repeats the command, up to 100. Repeated writes ask first.
- Commands that would block or take over the connection (`MONITOR`, `SUBSCRIBE`, `MULTI`,
  `BLPOP`...) and scripts are refused with a note.
- `KEYS` and other commands the TUI asks about get a 409 question. Writes go through the
  write gate. Commands in `state.refused` are refused.
- On a cluster the command is sent to the node that owns its keys.

The reply is `{line, status: "ok"|"note"|"failed", text, node, route_note, times, sent,
replies}`. `line` is the masked line, `text` says why it was not sent (in the dashboard's
terms), and `replies` holds one entry per reply, in the TUI's four views (`text`, `table`,
`json`, `raw`, each a list of lines), with `error` true when the server answered the command
with an error (that reply is the command's own, so it is shown as it is), `cut` when it was
cut, and `took_ms`.

The TUI panel's limits apply. A line longer than 8,192 characters gets 413
`{"error":"Not sent: a line holds at most 8,192 characters."}`. One line runs at a time
per dashboard, answers to its questions included: a second one while the first has not
answered gets 409
`{"error":"Not sent: the line before it is still running. Wait for its reply, then send it again."}`.
A repeat's replies are kept up to the TUI's 4 MiB output cap. Past that the oldest go,
`dropped` counts them, and `dropped_note` holds the TUI's notice
(`(the first N replies were dropped: the output keeps at most 4 MiB)`); otherwise
`dropped` is 0 and `dropped_note` is null. Before every write, a repeat's included, the
profile's `read_only` and `env` are read again from the config file. A profile made
read-only elsewhere (`w` in the TUI, `conn set --writes off`) refuses at once.

Example, `POST /api/cmdline {"line":"GET user:1"}`. Reply: HTTP 200

```json
{
  "dropped": 0,
  "dropped_note": null,
  "line": "GET user:1",
  "node": null,
  "replies": [
    {
      "cut": false,
      "error": false,
      "json": [
        "\"x\""
      ],
      "raw": [
        "[bulk string] $1 \"x\""
      ],
      "table": [
        "\"x\""
      ],
      "text": [
        "\"x\""
      ],
      "took_ms": 0.230875
    }
  ],
  "route_note": null,
  "sent": 1,
  "status": "ok",
  "text": null,
  "times": 1
}
```

Example, `POST /api/cmdline {"line":"HGETALL nope"}`. Reply: HTTP 200

```json
{
  "dropped": 0,
  "dropped_note": null,
  "line": "HGETALL nope",
  "node": null,
  "replies": [
    {
      "cut": false,
      "error": false,
      "json": [
        "[]"
      ],
      "raw": [
        "[array] *0"
      ],
      "table": [
        "(empty array)"
      ],
      "text": [
        "(empty array)"
      ],
      "took_ms": 0.200084
    }
  ],
  "route_note": null,
  "sent": 1,
  "status": "ok",
  "text": null,
  "times": 1
}
```

Example, `POST /api/cmdline {"line":"2 GET user:1"}`. Reply: HTTP 200

```json
{
  "dropped": 0,
  "dropped_note": null,
  "line": "2 GET user:1",
  "node": null,
  "replies": [
    {
      "cut": false,
      "error": false,
      "json": [
        "\"x\""
      ],
      "raw": [
        "[bulk string] $1 \"x\""
      ],
      "table": [
        "\"x\""
      ],
      "text": [
        "\"x\""
      ],
      "took_ms": 0.216958
    },
    {
      "cut": false,
      "error": false,
      "json": [
        "\"x\""
      ],
      "raw": [
        "[bulk string] $1 \"x\""
      ],
      "table": [
        "\"x\""
      ],
      "text": [
        "\"x\""
      ],
      "took_ms": 0.214792
    }
  ],
  "route_note": null,
  "sent": 2,
  "status": "ok",
  "text": null,
  "times": 2
}
```

Example, `POST /api/cmdline {"line":"EVAL \"return 1\" 0"}`. Reply: HTTP 200

```json
{
  "dropped": 0,
  "dropped_note": null,
  "line": "EVAL \"return 1\" 0",
  "node": null,
  "replies": [],
  "route_note": null,
  "sent": 0,
  "status": "note",
  "text": "Not sent: EVAL runs a script, whose commands the command line can't check.",
  "times": 1
}
```

Example, `POST /api/cmdline {"line":"SUBSCRIBE news"}`. Reply: HTTP 200

```json
{
  "dropped": 0,
  "dropped_note": null,
  "line": "SUBSCRIBE news",
  "node": null,
  "replies": [],
  "route_note": null,
  "sent": 0,
  "status": "note",
  "text": "Not sent here: use the Pub/Sub screen to subscribe.",
  "times": 1
}
```

Example, `POST /api/cmdline {"line":"SET user:3 z"}`. Reply: HTTP 200

```json
{
  "dropped": 0,
  "dropped_note": null,
  "line": "SET user:3 z",
  "node": null,
  "replies": [
    {
      "cut": false,
      "error": false,
      "json": [
        "\"OK\""
      ],
      "raw": [
        "[simple string] +OK"
      ],
      "table": [
        "OK"
      ],
      "text": [
        "OK"
      ],
      "took_ms": 0.20708400000000002
    }
  ],
  "route_note": null,
  "sent": 1,
  "status": "ok",
  "text": null,
  "times": 1
}
```

Example, `POST /api/cmdline {"line":"KEYS user:*"}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "862aad6c048d4f79236324144d32ab63",
    "kind": "ask",
    "text": "KEYS is @dangerous: it can slow the server down. Send it? KEYS user:*"
  }
}
```

Example, `POST /api/cmdline {"line":"KEYS user:*"}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "862aad6c048d4f79236324144d32ab63",
    "kind": "ask",
    "text": "KEYS is @dangerous: it can slow the server down. Send it? KEYS user:*"
  }
}
```

Example, `POST /api/confirm/<id> (KEYS)`. Reply: HTTP 200 (`$.replies[].json` cut to the first 3 of 5)

```json
{
  "dropped": 0,
  "dropped_note": null,
  "line": "KEYS user:*",
  "node": null,
  "replies": [
    {
      "cut": false,
      "error": false,
      "json": [
        "[",
        "  \"user:1\",",
        "  \"user:3\","
      ],
      "raw": [
        "[array] *3",
        "  [bulk string] $6 \"user:1\"",
        "  [bulk string] $6 \"user:3\"",
        "  [bulk string] $6 \"user:2\""
      ],
      "table": [
        "#  value",
        "1  user:1",
        "2  user:3",
        "3  user:2"
      ],
      "text": [
        "1) \"user:1\"",
        "2) \"user:3\"",
        "3) \"user:2\""
      ],
      "took_ms": 0.209084
    }
  ],
  "route_note": null,
  "sent": 1,
  "status": "ok",
  "text": null,
  "times": 1
}
```

Example, `POST /api/cmdline {"line":"SET user:3 z"} (read-only profile)`. Reply: HTTP 200

```json
{
  "dropped": 0,
  "dropped_note": null,
  "line": "SET user:3 z",
  "node": null,
  "replies": [],
  "route_note": null,
  "sent": 0,
  "status": "note",
  "text": "Not sent: prod-cache is read-only. Turn writes on in Connections.",
  "times": 1
}
```

### `GET /api/cmdline/commands`

`COMMAND LIST`, cached per connection, for completion. When the server has no `COMMAND LIST`, the reply is `{"off": ...}`.

Example, `GET /api/cmdline/commands (first 5 of 379)`. Reply: HTTP 200 (`$.names` cut to the first 3 of 5)

```json
{
  "names": [
    "acl",
    "acl|cat",
    "acl|deluser"
  ]
}
```

### `GET /api/cmdline/help?name=GET`

`COMMAND DOCS` for one command. When the server has no `COMMAND DOCS`, the TUI's built-in help is used.

Example, `GET /api/cmdline/help?name=get`. Reply: HTTP 200

```json
{
  "arguments": "key",
  "complexity": "O(1)",
  "name": "get",
  "since": "1.0.0",
  "subcommands": [],
  "summary": "Returns the string value of a key."
}
```

## Browse, Analyze and snapshots

These routes are the TUI's Browse, Analyze and snapshots, on the session's connection. Each
runs the TUI's own code: the same `live` requests (paced by the same command budget the
scan and the live views share), the same `scan` run for an analysis, the same views' state
for key names, key lists, values and slot statistics, and the same snapshot writer. The
dashboard sends no command of its own: `SCAN`, never `KEYS`, and nothing here writes.

The examples in this part were captured by `tests/web_p2_it.rs` with
`REDIS_LENS_P2_CAPTURE=<file>`, against a Redis 7.4 container (a Valkey 8.1 or a cluster
one where it says so) seeded with keys of every type, `cart:{n}` with a TTL, `lock:job:{n}`
without one, a key named `evil<ESC>[31mkey` and three keys in db 1.

### Key and pattern names

A key name is bytes, and it can hold control characters or bidirectional controls. Every
key or pattern a reply names is an object of three:

- `name`: for display, escaped with `value::escape` as the TUI escapes it (`\x1b` for ESC).
- `id`: the name's bytes in lowercase hex. The page sends this back to name a key
  (`/api/value?key=<id>`) or a pattern (`/api/keys?pattern=<id>`), never the display text.
  An id that is not even-length hex of UTF-8 bytes, or longer than 131,072 digits, is 400
  `{"error":"the key id is not the hex the dashboard gave for a key"}`.
- `copy`: the name as it is, for the clipboard, when escaping changes nothing; `null`
  otherwise (copying the escaped text would name another key).

Key names that are not UTF-8 are counted as unreadable by the live layers (`unreadable`
in a key list) and never listed, as in the TUI. Field names, members and values are shown
the same way: valid UTF-8 escaped, other bytes as `\xNN`.

### The `progress` event

An analysis reports on the event stream as `progress` events, each with the session's
`run` (the same number `POST /api/analysis` answered) and a `kind`:

| `kind` | When | Data |
|---|---|---|
| `started` | `POST /api/analysis` was taken | `{run, kind, depth, sample_limit}` |
| `sampling` | the first progress, then at most every 250 ms while keys are sampled | `{run, kind, sampled, target, patterns, stopping}` (`patterns` is `null` until the first partial results) |
| `stopping` | `DELETE /api/analysis` was taken | `{run, kind}` |
| `done` | the report is in: the panels can be read | `{run, kind, sampled_keys, patterns, complete, stopped, taken_at_unix}` (`stopped`: stopped early, so partial) |
| `stopped` | stopped before any key was sampled: nothing changed | `{run, kind, kept: false, text}` |
| `failed` | the scan failed; the session stays connected and keeps any earlier report | `{run, kind, error}` in fixed words |
| `compared` | after `done`, once the newest earlier snapshot was looked up and diffed (the Changes panel and Overview's `changes`), and the trend loaded | `{run, kind}` |

A `state` event comes with `started`, `done`, `stopped` and `failed` too (and an
`overview` event after `done` and `compared`, when Overview's part changed). A database
switch is a run as well, but it reads no key, so it sends no `progress` event.

Example, `event: progress` (`started`). Event

```json
{
  "depth": "quick",
  "kind": "started",
  "run": 4,
  "sample_limit": 10000
}
```

Example, `event: progress` (`sampling`). Event

```json
{
  "kind": "sampling",
  "patterns": 11,
  "run": 4,
  "sampled": 74,
  "stopping": false,
  "target": 74
}
```

Example, `event: progress` (`done`). Event

```json
{
  "complete": true,
  "kind": "done",
  "patterns": 11,
  "run": 4,
  "sampled_keys": 74,
  "stopped": false,
  "taken_at_unix": 1794745502
}
```

Example, `event: progress` (`compared`). Event

```json
{
  "kind": "compared",
  "run": 4
}
```

Example, `event: progress` (`stopping`). Event

```json
{
  "kind": "stopping",
  "run": 6
}
```

Example, `event: progress` (`stopped`). Event

```json
{
  "kept": false,
  "kind": "stopped",
  "run": 6,
  "text": "Analysis stopped before it sampled any key. Nothing changed."
}
```

A `failed` event's `error` (and `state.analysis.error`) says that the analysis failed,
never that the connection was refused, in one of these sentences: `The analysis failed:
the server refused a command it needs.` (a command refused or answered with an error, such
as `SCAN` for a user without it), `The analysis failed: the connection to the server
broke.`, the two password sentences of a refused login, or redis-lens's own sentence when
it gave one. The server's text never reaches it.

Example, `event: progress` (`failed`), a user without `SCAN`. Event

```json
{
  "error": "The analysis failed: the server refused a command it needs.",
  "kind": "failed",
  "run": 2
}
```

### `state.analysis`

`GET /api/state` (and every `state` event) has `analysis`:

- `status`: `none` (no report), `connecting` (an analysis's connect), `sampling`,
  `stopping` or `done` (a report the panels show).
- `run`: the session's run. A connect, an analysis and a database switch each start one.
- `depth`: `quick` or `deep`, while one runs and for the one that made the report.
- `sampled`, `target`: the progress while one runs.
- `started_unix`: when the running or last analysis started.
- `error`: why the last analysis failed, in fixed words.
- `note`: what the last stop did when it kept nothing.
- `report`: the report the panels show, or `null`: `taken_at_unix`, `age_secs` (its age now),
  `sampled_keys`, `patterns`, `complete` (every key was looked at), `stopped` (stopped
  early: partial, not saved), `sizes_available` (`MEMORY USAGE` was allowed), `cluster`.
- `previous`: the report is the earlier run's, kept while a new analysis has no results.
- `db`: the database the session reads.

Example, `GET /api/state`. Reply: HTTP 200 (`$.polling.intervals` cut to the first 3 of 8)

```json
{
  "analysis": {
    "db": 0,
    "depth": "quick",
    "error": null,
    "note": null,
    "previous": false,
    "report": {
      "age_secs": 0,
      "cluster": false,
      "complete": true,
      "patterns": 11,
      "sampled_keys": 74,
      "sizes_available": true,
      "stopped": false,
      "taken_at_unix": 1794745502
    },
    "run": 4,
    "sampled": 74,
    "started_unix": 1794745502,
    "status": "done",
    "target": 74
  },
  "connected_at_unix": 1794745502,
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:62093",
    "env": "local",
    "env_label": "LOCAL",
    "name": "local",
    "prefer_replica": false,
    "provider": null,
    "provider_notice": null,
    "read_only": false,
    "sentinel": null,
    "sentinel_tls": false,
    "ssh": null,
    "standalone": false,
    "tags": [],
    "tls": false,
    "token_command": false,
    "url": "redis://127.0.0.1:62093/0",
    "writes": true
  },
  "docker": false,
  "error": null,
  "features": {
    "entra_id": false,
    "version": "0.1.0"
  },
  "live": "ready",
  "live_error": null,
  "monitor": null,
  "monitor_limits": {
    "max_lines": 10000,
    "max_secs": 30
  },
  "nodes": [],
  "picked": 0,
  "polling": {
    "interval_secs": 2,
    "intervals": [
      1,
      2,
      3
    ],
    "paused": false
  },
  "refused": [],
  "sentinel": null,
  "server": {
    "flavor": "redis",
    "keys": 74,
    "lfu": true,
    "maxmemory": 0,
    "mode": "standalone",
    "policy": "allkeys-lfu",
    "used_memory": 1944104,
    "version": "7.4.12"
  },
  "status": "connected",
  "tail": null,
  "target": "primary",
  "writes": true,
  "writes_off": null
}
```

### `GET /api/overview`: `analysis`

Overview's analysis part, `null` until an analysis has a report and while one runs (the
TUI's Overview shows "Things to look at" then): `taken_at_unix`, `sampled_keys`,
`patterns`, `complete`, `stopped`, `sizes_available`, `findings` (the TUI's analysis
findings, with `severity`, `text`, `screen`, `area` and `panel` as the connect's
findings), `top_patterns` (the 5 biggest by memory: `pattern`, `est_keys`, `est_bytes`,
`share`), `never_expires_share` (of the memory sampled), `changes` (`pending`, `none`,
`failed` or `diff`, as the Changes panel) and `note`. The `overview` event carries it too.

Example, `GET /api/overview`, its `analysis` part only. Reply: HTTP 200 (`$.analysis.findings` cut to the first 3 of 11; `$.analysis.top_patterns` cut to the first 3 of 5)

```json
{
  "analysis": {
    "changes": "none",
    "complete": true,
    "findings": [
      {
        "area": "Analyze",
        "panel": "Expiry",
        "screen": "expiry",
        "severity": "note",
        "text": "lock:job:{id}: 100% of its keys never expire."
      },
      {
        "area": "Analyze",
        "panel": "Expiry",
        "screen": "expiry",
        "severity": "info",
        "text": "57% of memory never expires: 5.5K of 9.6K."
      },
      {
        "area": "Analyze",
        "panel": "Expiry",
        "screen": "expiry",
        "severity": "info",
        "text": "events:log never expires."
      }
    ],
    "never_expires_share": 0.5708502024291497,
    "note": null,
    "patterns": 11,
    "sampled_keys": 74,
    "sizes_available": true,
    "stopped": false,
    "taken_at_unix": 1794745502,
    "top_patterns": [
      {
        "est_bytes": 4736,
        "est_keys": 1,
        "pattern": {
          "copy": "events:log",
          "id": "6576656e74733a6c6f67",
          "name": "events:log"
        },
        "share": 0.47935222672064776
      },
      {
        "est_bytes": 4240,
        "est_keys": 60,
        "pattern": {
          "copy": "cart:{id}",
          "id": "636172743a7b69647d",
          "name": "cart:{id}"
        },
        "share": 0.4291497975708502
      },
      {
        "est_bytes": 280,
        "est_keys": 5,
        "pattern": {
          "copy": "lock:job:{id}",
          "id": "6c6f636b3a6a6f623a7b69647d",
          "name": "lock:job:{id}"
        },
        "share": 0.02834008097165992
      }
    ]
  }
}
```

### `GET /api/dbs`

The databases and their key counts (`INFO keyspace` on the picked node), the current one
always listed: `current`, `rows` (`db`, `keys`, `expires`, `current`), `unread` (why the
counts could not be read, with the current database still listed), `saved_as` (the
profile's name; `null` for a Docker container) and `note`. On a cluster it is
`{"off": "A cluster only has database 0."}`. 409 while connecting, while an analysis runs
or when the live connection is closed, each with its reason.

Example, `GET /api/dbs`. Reply: HTTP 200

```json
{
  "current": 0,
  "note": "Picking a database is for this session. The saved profile is unchanged.",
  "rows": [
    {
      "current": true,
      "db": 0,
      "expires": 60,
      "keys": 74
    },
    {
      "current": false,
      "db": 1,
      "expires": 0,
      "keys": 3
    }
  ],
  "saved_as": "local",
  "unread": null
}
```

Example, `GET /api/dbs`. Reply: HTTP 200 [cluster redis]

```json
{
  "off": "A cluster only has database 0."
}
```

### `POST /api/db`

`{"db": n}` switches the session to database `n`, as picking one in the TUI's database
list does. Only a database the last `GET /api/dbs` listed is taken (422 otherwise). It is a
new run that reads no key: the earlier results go, the live connection opens again on the
new database, a running pub/sub tail is subscribed again, a running Monitor is stopped, and
Browse reads the new database's key names. For this session only: the saved profile is not
changed. The reply is 202 with the run; a `state` event follows, and
`state.analysis.db` says the database. The current database is 200
`{"db": n, "switched": false, "run": null, "text": null}` (nothing changes). 409 on a
cluster (`A cluster only has database 0.`), while an analysis runs, while a bulk action
counts or runs (`A bulk action is running: wait for it to end, or stop it first.`), and
when no run can
start: no session (`not connected: connect first, or the live connection is closed`), the
switch before still connecting
(`Still connecting: wait until the connection is ready.`) or a failed connect
(`The connect failed: connect again first.`).

The switch drops Browse's key names, key lists and open values (they were of the other
database); their `list` and `view` numbers are not given again, so an old number answers
404. A write question asked before the switch is refused when answered after it (409, see
"Confirms"). An analysis, a run on the same database, keeps the key lists and values
open.

Example, `POST /api/db {"db":1}`. Reply: HTTP 202

```json
{
  "db": 1,
  "run": 2,
  "switched": true,
  "text": "Switched to db 1 for this session. The saved profile is unchanged."
}
```

Example, `POST /api/db {"db":0}` on database 0. Reply: HTTP 200

```json
{
  "db": 0,
  "run": null,
  "switched": false,
  "text": null
}
```

Example, `POST /api/db {"db":7}`. Reply: HTTP 422

```json
{
  "error": "Pick a database the list shows (read the list first): it shows the databases with keys."
}
```

### `GET /api/browse`

Browse › Keys: the pattern tree. `?sort=` is one of `sorts` (`memory`, `keys`, `avg_size`,
`no_ttl`, `name`: sizes need an analysis; 400 for anything else), `?node=<i>` narrows an
analysis of a cluster to one shard (`nodes` lists them; 422 for one not there; 400 for
one that is not a number). `node` in the reply is the node the rows are narrowed to: only
an analysis's rows can be, so it is `null` for rows from key names or from partial
results, whatever `?node=` asked.

`source` says where the rows come from:

- `names`: before any analysis, the dashboard reads key names as the TUI does (`SCAN` in
  pages of 1,000, the first `names.first_pages` pages at once, every shard on a cluster)
  and groups them into patterns. Only names are known: counts, no sizes, no verdict.
  `names` says how far it got: `pages`, `names_read`, `dbsize`, `complete`, `done`,
  `reading`, `error`, `refused`, `skipped` and, on a cluster, `missing` (shards not read)
  and `nodes_done` (`[read, of]`).
- `partial`: an analysis is running and has partial results (`partial` holds
  `{sampled, target}`).
- `analysis`: the report. Rows have sizes, TTL histograms, the verdict and the TUI's
  reason, and `change` when the last comparison found one.
- `none`: nothing yet; `why` says why.

`rows` are pattern rows, or `group` rows (`label`, `key`, `children` of pattern rows) when
patterns share a prefix, as in the TUI. A pattern row: `pattern` (a name, above), `type`,
`est_keys`, `est_bytes`, `avg_bytes`, `sampled_keys`, `sampled_bytes`, `share`, `no_ttl`,
`no_ttl_ratio`, `ttl_hist` (counts per `ttl_labels`), `ttl_verdict`, `verdict`
(`leak?`, `persistent`, ...), `reason`, `warning`, `size_p50`, `size_p99`, `size_max`,
`examples` (sampled keys, as in a key list), `command` (the `redis-cli --scan` line that
lists them), `marked`, `change` and, from names only, `names_seen` and `literal`. `total`
is the memory (or keys, from names) the rows add up to; `previous` says the rows are the
earlier run's.

Example, `GET /api/browse?sort=keys`. Reply: HTTP 200 (`$.rows` cut to the first 2 of 9; `$.ttl_labels` cut to the first 2 of 7)

```json
{
  "names": {
    "complete": true,
    "dbsize": 74,
    "done": true,
    "error": null,
    "first_pages": 8,
    "missing": [],
    "names_read": 74,
    "nodes_done": null,
    "pages": 1,
    "reading": false,
    "refused": [],
    "skipped": []
  },
  "node": null,
  "nodes": [],
  "partial": null,
  "previous": false,
  "rows": [
    {
      "avg_bytes": null,
      "change": null,
      "command": "redis-cli --scan --pattern \"cart:*\"",
      "est_bytes": null,
      "est_keys": 60,
      "examples": [],
      "kind": "pattern",
      "literal": false,
      "marked": false,
      "names_seen": 60,
      "no_ttl": null,
      "no_ttl_ratio": null,
      "pattern": {
        "copy": "cart:{id}",
        "id": "636172743a7b69647d",
        "name": "cart:{id}"
      },
      "reason": null,
      "sampled_bytes": null,
      "sampled_keys": 60,
      "share": 0.8108108108108109,
      "size_max": null,
      "size_p50": null,
      "size_p99": null,
      "ttl_hist": null,
      "ttl_verdict": null,
      "type": null,
      "verdict": null,
      "warning": false
    },
    {
      "avg_bytes": null,
      "change": null,
      "command": "redis-cli --scan --pattern \"lock:job:*\"",
      "est_bytes": null,
      "est_keys": 5,
      "examples": [],
      "kind": "pattern",
      "literal": false,
      "marked": false,
      "names_seen": 5,
      "no_ttl": null,
      "no_ttl_ratio": null,
      "pattern": {
        "copy": "lock:job:{id}",
        "id": "6c6f636b3a6a6f623a7b69647d",
        "name": "lock:job:{id}"
      },
      "reason": null,
      "sampled_bytes": null,
      "sampled_keys": 5,
      "share": 0.06756756756756757,
      "size_max": null,
      "size_p50": null,
      "size_p99": null,
      "ttl_hist": null,
      "ttl_verdict": null,
      "type": null,
      "verdict": null,
      "warning": false
    }
  ],
  "sizes": false,
  "sort": "keys",
  "sorts": [
    "keys",
    "name"
  ],
  "source": "names",
  "total": 74,
  "ttl_labels": [
    "none",
    "<1m"
  ],
  "why": null
}
```

Example, `GET /api/browse`. Reply: HTTP 200 (`$.rows` cut to the first 2 of 9; `$.rows[*].ttl_hist` cut to the first 2 of 7; `$.rows[*].examples` cut to the first 2 of 5; `$.sorts` cut to the first 2 of 5; `$.ttl_labels` cut to the first 2 of 7)

```json
{
  "names": null,
  "node": null,
  "nodes": [],
  "partial": null,
  "previous": false,
  "rows": [
    {
      "avg_bytes": 4736,
      "change": null,
      "command": "redis-cli --scan --pattern \"events:log\"",
      "est_bytes": 4736,
      "est_keys": 1,
      "examples": [
        {
          "bytes": 4736,
          "copy": "events:log",
          "freq": 6,
          "id": "6576656e74733a6c6f67",
          "name": "events:log",
          "node": null,
          "ttl": "never",
          "ttl_secs": null,
          "type": "stream"
        }
      ],
      "kind": "pattern",
      "literal": null,
      "marked": false,
      "names_seen": null,
      "no_ttl": 1,
      "no_ttl_ratio": 1.0,
      "pattern": {
        "copy": "events:log",
        "id": "6576656e74733a6c6f67",
        "name": "events:log"
      },
      "reason": "All 1 keys never expire and the pattern has no earlier snapshot to compare with. This looks intentional.",
      "sampled_bytes": 4736,
      "sampled_keys": 1,
      "share": 0.47935222672064776,
      "size_max": 4736,
      "size_p50": 4736,
      "size_p99": 4736,
      "ttl_hist": [
        1,
        0
      ],
      "ttl_verdict": "persistent · stable",
      "type": "stream",
      "verdict": "persistent",
      "warning": false
    },
    {
      "avg_bytes": 70,
      "change": null,
      "command": "redis-cli --scan --pattern \"cart:*\"",
      "est_bytes": 4240,
      "est_keys": 60,
      "examples": [
        {
          "bytes": 72,
          "copy": "cart:10",
          "freq": 6,
          "id": "636172743a3130",
          "name": "cart:10",
          "node": null,
          "ttl": "59m 59s",
          "ttl_secs": 3599,
          "type": "string"
        },
        {
          "bytes": 72,
          "copy": "cart:11",
          "freq": 6,
          "id": "636172743a3131",
          "name": "cart:11",
          "node": null,
          "ttl": "59m 59s",
          "ttl_secs": 3599,
          "type": "string"
        }
      ],
      "kind": "pattern",
      "literal": null,
      "marked": false,
      "names_seen": null,
      "no_ttl": 0,
      "no_ttl_ratio": 0.0,
      "pattern": {
        "copy": "cart:{id}",
        "id": "636172743a7b69647d",
        "name": "cart:{id}"
      },
      "reason": "All 60 keys expire, typically in under an hour.",
      "sampled_bytes": 4240,
      "sampled_keys": 60,
      "share": 0.4291497975708502,
      "size_max": 72,
      "size_p50": 72,
      "size_p99": 72,
      "ttl_hist": [
        0,
        0
      ],
      "ttl_verdict": "ok",
      "type": "string",
      "verdict": "ok",
      "warning": false
    }
  ],
  "sizes": true,
  "sort": "memory",
  "sorts": [
    "memory",
    "keys"
  ],
  "source": "analysis",
  "total": 9880,
  "ttl_labels": [
    "none",
    "<1m"
  ],
  "why": null
}
```

Example, `GET /api/browse`. Reply: HTTP 200 (`$.rows` cut to the first 2 of 31; `$.ttl_labels` cut to the first 2 of 7) [cluster redis]

```json
{
  "names": {
    "complete": true,
    "dbsize": 120,
    "done": true,
    "error": null,
    "first_pages": 8,
    "missing": [],
    "names_read": 120,
    "nodes_done": [
      3,
      3
    ],
    "pages": 3,
    "reading": false,
    "refused": [],
    "skipped": []
  },
  "node": null,
  "nodes": [],
  "partial": null,
  "previous": false,
  "rows": [
    {
      "avg_bytes": null,
      "change": null,
      "command": "redis-cli --scan --pattern \"item:*\"",
      "est_bytes": null,
      "est_keys": 90,
      "examples": [],
      "kind": "pattern",
      "literal": false,
      "marked": false,
      "names_seen": 90,
      "no_ttl": null,
      "no_ttl_ratio": null,
      "pattern": {
        "copy": "item:{id}",
        "id": "6974656d3a7b69647d",
        "name": "item:{id}"
      },
      "reason": null,
      "sampled_bytes": null,
      "sampled_keys": 90,
      "share": 0.75,
      "size_max": null,
      "size_p50": null,
      "size_p99": null,
      "ttl_hist": null,
      "ttl_verdict": null,
      "type": null,
      "verdict": null,
      "warning": false
    },
    {
      "avg_bytes": null,
      "change": null,
      "command": "redis-cli --scan --pattern \"*:name\"",
      "est_bytes": null,
      "est_keys": 1,
      "examples": [],
      "kind": "pattern",
      "literal": true,
      "marked": false,
      "names_seen": 1,
      "no_ttl": null,
      "no_ttl_ratio": null,
      "pattern": {
        "copy": "{user0}:name",
        "id": "7b75736572307d3a6e616d65",
        "name": "{user0}:name"
      },
      "reason": null,
      "sampled_bytes": null,
      "sampled_keys": 1,
      "share": 0.008333333333333333,
      "size_max": null,
      "size_p50": null,
      "size_p99": null,
      "ttl_hist": null,
      "ttl_verdict": null,
      "type": null,
      "verdict": null,
      "warning": false
    }
  ],
  "sizes": false,
  "sort": "keys",
  "sorts": [
    "keys",
    "name"
  ],
  "source": "names",
  "total": 120,
  "ttl_labels": [
    "none",
    "<1m"
  ],
  "why": null
}
```

### `POST /api/browse/more`

One more page of key names (1,000 names), with `{"sort": ...}` as `GET /api/browse` takes
it; the reply is the tree. 409 once an analysis's patterns are shown, while a page is
being read, or when every name is read.

Example, `POST /api/browse/more {}`. Reply: HTTP 409

```json
{
  "error": "Every key name is read: 74."
}
```

### `GET /api/keys`, `GET /api/find`, `GET /api/keys/more`

A key list, as Browse › Keys lists a pattern's keys and Find key finds keys:

- `GET /api/keys?pattern=<id>[&node=<i>]`: the keys of a pattern from the tree (its `id`),
  matched with the pattern's glob. `node` narrows a cluster's list to one shard.
- `GET /api/find?q=<text>`: the keys of the whole database whose names match: text with
  `*`, `?` or `[` is a glob used as typed, other text means "contains" (`*text*`). Control
  characters are dropped, the text is cut at 512 bytes and trimmed. 400 for empty text;
  then, as the TUI checks them, 409 while connecting or once the live connection is
  closed, and 422 for a glob the TUI refuses (too many wildcards, in its words).
- `GET /api/keys/more?list=<n>`: the next page of list `n`. 404 for a list no longer kept
  (the newest 16 are, and a database switch drops them all), 409 while it loads a page,
  when it is complete, or once it holds 50,000 keys or more
  (`{"error":"This list holds 50,000 keys or more, so no more are loaded. Find a narrower name to see others."}`).

`GET /api/keys` is 422 `{"error":"That node is not one of this analysis's nodes."}` for a
`node` the analysis does not have. Every route of the three is 400 for a parameter
missing or not of its type ("Conventions").

The first page is read before the reply. If the browser drops the request while a page is
read, the page still lands in the list: the next `more` reads on from it. A list: `list`
(its number), `kind` (`pattern` or `find`), `pattern` or `find` (`{text, glob}`), `glob`,
`rows`, `loaded` (rows held), `new_from`, `pages`, `page_keys`, `done`, `capped` (it holds
`max_rows` keys or more: no more pages are read), `max_rows` (50,000), `error`, `refused`
(metadata the server refused, in fixed words), `other_pattern` (matched the glob but
belong to another pattern), `unreadable`, `sizes`, `lfu`, `freq` and, on a cluster,
`cluster` (`walk`: the shards in order, `at`, `nodes_done`, `missing`, `narrowed`).

`rows` holds the rows the reply's page added, from row `new_from` on: every row for
`GET /api/keys` and `GET /api/find` (`new_from` is 0), only the new ones for
`GET /api/keys/more` (`new_from` is the number held before it). The page appends them to
the rows it has. A row is a key name with
`type`, `ttl` (`never` or the TUI's `59m 59s`), `ttl_secs`, `bytes`, `encoding`, `length`,
`length_unit`, `freq` (the LFU counter, when the policy keeps them) and `node`.

Example, `GET /api/keys?pattern=636172743a7b69647d`. Reply: HTTP 200 (`$.rows` cut to the first 2 of 60)

```json
{
  "capped": false,
  "cluster": null,
  "done": true,
  "error": null,
  "find": null,
  "freq": true,
  "glob": "cart:*",
  "kind": "pattern",
  "lfu": true,
  "list": 1,
  "loaded": 60,
  "max_rows": 50000,
  "new_from": 0,
  "other_pattern": 0,
  "page_keys": 1000,
  "pages": 1,
  "pattern": {
    "copy": "cart:{id}",
    "id": "636172743a7b69647d",
    "name": "cart:{id}"
  },
  "refused": [],
  "rows": [
    {
      "bytes": 72,
      "copy": "cart:57",
      "encoding": "embstr",
      "freq": 5,
      "id": "636172743a3537",
      "length": 5,
      "length_unit": "bytes",
      "name": "cart:57",
      "node": null,
      "ttl": "59m 59s",
      "ttl_secs": 3599,
      "type": "string"
    },
    {
      "bytes": 64,
      "copy": "cart:9",
      "encoding": "embstr",
      "freq": 5,
      "id": "636172743a39",
      "length": 5,
      "length_unit": "bytes",
      "name": "cart:9",
      "node": null,
      "ttl": "59m 59s",
      "ttl_secs": 3599,
      "type": "string"
    }
  ],
  "sizes": true,
  "unreadable": 0
}
```

Example, `GET /api/find?q=user`. Reply: HTTP 200

```json
{
  "capped": false,
  "cluster": null,
  "done": true,
  "error": null,
  "find": {
    "glob": "*user*",
    "text": "user"
  },
  "freq": true,
  "glob": "*user*",
  "kind": "find",
  "lfu": true,
  "list": 2,
  "loaded": 1,
  "max_rows": 50000,
  "new_from": 0,
  "other_pattern": 0,
  "page_keys": 1000,
  "pages": 1,
  "pattern": null,
  "refused": [],
  "rows": [
    {
      "bytes": 104,
      "copy": "user:1:profile",
      "encoding": "listpack",
      "freq": 5,
      "id": "757365723a313a70726f66696c65",
      "length": 2,
      "length_unit": "fields",
      "name": "user:1:profile",
      "node": null,
      "ttl": "never",
      "ttl_secs": null,
      "type": "hash"
    }
  ],
  "sizes": true,
  "unreadable": 0
}
```

Example, `GET /api/find?q=evil%2A`. Reply: HTTP 200

```json
{
  "capped": false,
  "cluster": null,
  "done": true,
  "error": null,
  "find": {
    "glob": "evil*",
    "text": "evil*"
  },
  "freq": true,
  "glob": "evil*",
  "kind": "find",
  "lfu": true,
  "list": 3,
  "loaded": 1,
  "max_rows": 50000,
  "new_from": 0,
  "other_pattern": 0,
  "page_keys": 1000,
  "pages": 1,
  "pattern": null,
  "refused": [],
  "rows": [
    {
      "bytes": 64,
      "copy": null,
      "encoding": "embstr",
      "freq": 5,
      "id": "6576696c1b5b33316d6b6579",
      "length": 1,
      "length_unit": "bytes",
      "name": "evil\\x1b[31mkey",
      "node": null,
      "ttl": "never",
      "ttl_secs": null,
      "type": "string"
    }
  ],
  "sizes": true,
  "unreadable": 0
}
```

Example, `GET /api/find?q=%2Aa%2Ab%2Ac%2Ad%2Ae%2Af%2Ag%2Ah%2Ai%2A`. Reply: HTTP 422

```json
{
  "error": "A pattern may have at most 8 * wildcards, so matching it stays cheap for the server. This one has 10."
}
```

Example, `GET /api/keys/more?list=1`. Reply: HTTP 409

```json
{
  "error": "Every key of this list is loaded."
}
```

Example, `GET /api/keys?pattern=6974656d3a7b69647d`. Reply: HTTP 200 (`$.cluster.walk` cut to the first 2 of 3; `$.rows` cut to the first 2 of 90) [cluster valkey]

```json
{
  "capped": false,
  "cluster": {
    "at": 2,
    "missing": [],
    "narrowed": null,
    "nodes_done": 3,
    "walk": [
      0,
      1
    ]
  },
  "done": true,
  "error": null,
  "find": null,
  "freq": false,
  "glob": "item:*",
  "kind": "pattern",
  "lfu": false,
  "list": 1,
  "loaded": 90,
  "max_rows": 50000,
  "new_from": 0,
  "other_pattern": 0,
  "page_keys": 1000,
  "pages": 3,
  "pattern": {
    "copy": "item:{id}",
    "id": "6974656d3a7b69647d",
    "name": "item:{id}"
  },
  "refused": [],
  "rows": [
    {
      "bytes": 32,
      "copy": "item:18",
      "encoding": "embstr",
      "freq": null,
      "id": "6974656d3a3138",
      "length": 1,
      "length_unit": "bytes",
      "name": "item:18",
      "node": 0,
      "ttl": "never",
      "ttl_secs": null,
      "type": "string"
    },
    {
      "bytes": 32,
      "copy": "item:6",
      "encoding": "embstr",
      "freq": null,
      "id": "6974656d3a36",
      "length": 1,
      "length_unit": "bytes",
      "name": "item:6",
      "node": 0,
      "ttl": "never",
      "ttl_secs": null,
      "type": "string"
    }
  ],
  "sizes": true,
  "unreadable": 0
}
```

### `GET /api/value`, `GET /api/value/more`, `GET /api/value/item`

A key's value, as the TUI's key detail reads it (the same requests, paged the same way,
at most 10,000 items held):

- `GET /api/value?key=<id>[&type=<t>]` opens it and reads the first page. The key's
  metadata (`meta`) is its row in the newest key list that has it, else what the analysis
  sampled of it, else only the name, with the type the first read says. The newest 8
  values are kept open.
- `GET /api/value/more?view=<n>` reads the next page. When there is none it reads nothing
  and `notice` says why, in the TUI's words. 404 for a view no longer open (a database
  switch closes them all), 409 `{"error":"Still loading the value: wait for it."}` while a
  page of it loads: one page of a value is read at a time, so two requests at once never
  read the same page twice. If the browser drops a request while its page is read, the
  page still lands in the value.
- `GET /api/value/item?view=<n>&index=<i>[&field=<j>][&format=<f>][&width=<w>]` renders one
  item whole: a hash field's value, a list, set or vector element, a sorted set member, a
  stream entry's field `field` (a stream entry needs one: 422), or `index=0` for a string
  or a JSON document. `format` is one of the value's `formats` (`text`, `hex`, `JSON`,
  `MessagePack`, `PHP`, `Pickle`, `Protobuf`, `Java`, any case; `msgpack` too); without
  one the format is detected.

Every route of the three is 400 for a parameter missing or not of its type
("Conventions"): `key`, `view`, `index`, `field` and `width` are numbers, `index` 0 or
more.

A value: `view`, `key` (a name), `type` (the server's), `kind` (`string`, `hash`, `list`,
`set`, `zset`, `stream`, `json`, `vector`, `array`, `module`, `other`), `command` (what
reads it), `meta`, `pattern`, `loading`, `pages`, `loaded`, `length`, `done`, `more`,
`capped`, `gone` (the key no longer exists), `error`, `notice`, `refused`, `field_ttls`
(hash field TTLs are read: Redis 7.4 and later) and `field_ttls_off`, `items`, `value`,
`cut`, `message` (a type that is not read, a document too big), `module`, `cluster` and
`max_items`.

`items` (hash, list, set, sorted set, stream, vector) are previews of at most 80
characters: `index`, `name` (a field, member or entry id), `value`, `ttl`, `at`, and a
stream entry's `fields`. `value` (a string or document) and an item's `shown` are rendered
as the TUI renders them: `format`, `detected`, `picked`, `formats`, `bytes`,
`compression`, `truncated`, `note`, `header`, `lines` and `copy`.

Copy: the server gives the text to copy and the page puts it on the clipboard. `copy` is
the value as shown when that is safe text; for a value with control characters it is
`null` and `copy_refused` says to show it as hex, whose text copies.

Example, `GET /api/value?key=7374723a706c61696e`. Reply: HTTP 200 (`$.value.formats` cut to the first 3 of 8)

```json
{
  "capped": false,
  "cluster": false,
  "command": "GETRANGE",
  "cut": null,
  "done": true,
  "error": null,
  "field_ttls": true,
  "field_ttls_off": null,
  "gone": false,
  "items": [],
  "key": {
    "copy": "str:plain",
    "id": "7374723a706c61696e",
    "name": "str:plain"
  },
  "kind": "string",
  "length": 11,
  "loaded": 1,
  "loading": false,
  "max_items": 10000,
  "message": null,
  "meta": {
    "bytes": null,
    "copy": "str:plain",
    "encoding": null,
    "freq": null,
    "id": "7374723a706c61696e",
    "length": 11,
    "length_unit": "bytes",
    "name": "str:plain",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "string"
  },
  "module": null,
  "more": false,
  "notice": "The value is loaded.",
  "pages": 1,
  "pattern": null,
  "refused": [],
  "type": "string",
  "value": {
    "bytes": 11,
    "compression": null,
    "copy": "hello world",
    "copy_refused": null,
    "detected": "text",
    "format": "text",
    "formats": [
      "text",
      "hex",
      "JSON"
    ],
    "header": "value · text · 11 bytes · auto-detected",
    "lines": [
      "hello world"
    ],
    "note": null,
    "picked": null,
    "truncated": false
  },
  "view": 1
}
```

Example, `GET /api/value?key=7374723a62696e`. Reply: HTTP 200 (`$.value.formats` cut to the first 3 of 8)

```json
{
  "capped": false,
  "cluster": false,
  "command": "GETRANGE",
  "cut": null,
  "done": true,
  "error": null,
  "field_ttls": true,
  "field_ttls_off": null,
  "gone": false,
  "items": [],
  "key": {
    "copy": "str:bin",
    "id": "7374723a62696e",
    "name": "str:bin"
  },
  "kind": "string",
  "length": 7,
  "loaded": 1,
  "loading": false,
  "max_items": 10000,
  "message": null,
  "meta": {
    "bytes": null,
    "copy": "str:bin",
    "encoding": null,
    "freq": null,
    "id": "7374723a62696e",
    "length": 7,
    "length_unit": "bytes",
    "name": "str:bin",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "string"
  },
  "module": null,
  "more": false,
  "notice": "The value is loaded.",
  "pages": 1,
  "pattern": null,
  "refused": [],
  "type": "string",
  "value": {
    "bytes": 7,
    "compression": null,
    "copy": "0001ff62696e1b",
    "copy_refused": null,
    "detected": "hex",
    "format": "hex",
    "formats": [
      "text",
      "hex",
      "JSON"
    ],
    "header": "value · hex · 7 bytes · auto-detected",
    "lines": [
      "00000000  00 01 ff 62 69 6e 1b                              |...bin.|"
    ],
    "note": null,
    "picked": null,
    "truncated": false
  },
  "view": 3
}
```

Example, `GET /api/value/item?view=3&index=0&format=text`. Reply: HTTP 200 (`$.shown.formats` cut to the first 3 of 8)

```json
{
  "field": null,
  "index": 0,
  "name": null,
  "shown": {
    "bytes": 7,
    "compression": null,
    "copy": null,
    "copy_refused": "The value has control characters, so it is not copied as text: show it as hex to copy it.",
    "detected": "hex",
    "format": "text",
    "formats": [
      "text",
      "hex",
      "JSON"
    ],
    "header": "value · 7 bytes · shown as text",
    "lines": [
      "\\x00\\x01\\xffbin\\x1b"
    ],
    "note": null,
    "picked": "text",
    "truncated": false
  }
}
```

Example, `GET /api/value?key=757365723a313a70726f66696c65`. Reply: HTTP 200

```json
{
  "capped": false,
  "cluster": false,
  "command": "HSCAN",
  "cut": null,
  "done": true,
  "error": null,
  "field_ttls": true,
  "field_ttls_off": null,
  "gone": false,
  "items": [
    {
      "at": null,
      "fields": null,
      "index": 0,
      "name": "name",
      "ttl": "never",
      "value": "Ada"
    },
    {
      "at": null,
      "fields": null,
      "index": 1,
      "name": "email",
      "ttl": "never",
      "value": "ada@example.com"
    }
  ],
  "key": {
    "copy": "user:1:profile",
    "id": "757365723a313a70726f66696c65",
    "name": "user:1:profile"
  },
  "kind": "hash",
  "length": 2,
  "loaded": 2,
  "loading": false,
  "max_items": 10000,
  "message": null,
  "meta": {
    "bytes": 104,
    "copy": "user:1:profile",
    "encoding": "listpack",
    "freq": 5,
    "id": "757365723a313a70726f66696c65",
    "length": 2,
    "length_unit": "fields",
    "name": "user:1:profile",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "hash"
  },
  "module": null,
  "more": false,
  "notice": "All 2 items are loaded.",
  "pages": 1,
  "pattern": null,
  "refused": [],
  "type": "hash",
  "value": null,
  "view": 4
}
```

Example, `GET /api/value/item?view=4&index=0`. Reply: HTTP 200 (`$.shown.formats` cut to the first 3 of 8)

```json
{
  "field": null,
  "index": 0,
  "name": "name",
  "shown": {
    "bytes": 3,
    "compression": null,
    "copy": "Ada",
    "copy_refused": null,
    "detected": "text",
    "format": "text",
    "formats": [
      "text",
      "hex",
      "JSON"
    ],
    "header": "value · text · 3 bytes · auto-detected",
    "lines": [
      "Ada"
    ],
    "note": null,
    "picked": null,
    "truncated": false
  }
}
```

Example, `GET /api/value?key=71756575653a6a6f6273`. Reply: HTTP 200

```json
{
  "capped": false,
  "cluster": false,
  "command": "LRANGE",
  "cut": null,
  "done": true,
  "error": null,
  "field_ttls": true,
  "field_ttls_off": null,
  "gone": false,
  "items": [
    {
      "at": null,
      "fields": null,
      "index": 0,
      "name": null,
      "ttl": null,
      "value": "j1"
    },
    {
      "at": null,
      "fields": null,
      "index": 1,
      "name": null,
      "ttl": null,
      "value": "j2"
    },
    {
      "at": null,
      "fields": null,
      "index": 2,
      "name": null,
      "ttl": null,
      "value": "j3"
    }
  ],
  "key": {
    "copy": "queue:jobs",
    "id": "71756575653a6a6f6273",
    "name": "queue:jobs"
  },
  "kind": "list",
  "length": null,
  "loaded": 3,
  "loading": false,
  "max_items": 10000,
  "message": null,
  "meta": {
    "bytes": null,
    "copy": "queue:jobs",
    "encoding": null,
    "freq": null,
    "id": "71756575653a6a6f6273",
    "length": null,
    "length_unit": "items",
    "name": "queue:jobs",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "list"
  },
  "module": null,
  "more": false,
  "notice": "All 3 items are loaded.",
  "pages": 1,
  "pattern": null,
  "refused": [],
  "type": "list",
  "value": null,
  "view": 5
}
```

Example, `GET /api/value/more?view=5`. Reply: HTTP 200

```json
{
  "capped": false,
  "cluster": false,
  "command": "LRANGE",
  "cut": null,
  "done": true,
  "error": null,
  "field_ttls": true,
  "field_ttls_off": null,
  "gone": false,
  "items": [
    {
      "at": null,
      "fields": null,
      "index": 0,
      "name": null,
      "ttl": null,
      "value": "j1"
    },
    {
      "at": null,
      "fields": null,
      "index": 1,
      "name": null,
      "ttl": null,
      "value": "j2"
    },
    {
      "at": null,
      "fields": null,
      "index": 2,
      "name": null,
      "ttl": null,
      "value": "j3"
    }
  ],
  "key": {
    "copy": "queue:jobs",
    "id": "71756575653a6a6f6273",
    "name": "queue:jobs"
  },
  "kind": "list",
  "length": null,
  "loaded": 3,
  "loading": false,
  "max_items": 10000,
  "message": null,
  "meta": {
    "bytes": null,
    "copy": "queue:jobs",
    "encoding": null,
    "freq": null,
    "id": "71756575653a6a6f6273",
    "length": null,
    "length_unit": "items",
    "name": "queue:jobs",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "list"
  },
  "module": null,
  "more": false,
  "notice": "All 3 items are loaded.",
  "pages": 1,
  "pattern": null,
  "refused": [],
  "type": "list",
  "value": null,
  "view": 5
}
```

Example, `GET /api/value?key=626f6172643a73636f726573`. Reply: HTTP 200

```json
{
  "capped": false,
  "cluster": false,
  "command": "ZRANGE",
  "cut": null,
  "done": true,
  "error": null,
  "field_ttls": true,
  "field_ttls_off": null,
  "gone": false,
  "items": [
    {
      "at": null,
      "fields": null,
      "index": 0,
      "name": "ann",
      "ttl": null,
      "value": "10"
    },
    {
      "at": null,
      "fields": null,
      "index": 1,
      "name": "bob",
      "ttl": null,
      "value": "20"
    }
  ],
  "key": {
    "copy": "board:scores",
    "id": "626f6172643a73636f726573",
    "name": "board:scores"
  },
  "kind": "zset",
  "length": null,
  "loaded": 2,
  "loading": false,
  "max_items": 10000,
  "message": null,
  "meta": {
    "bytes": null,
    "copy": "board:scores",
    "encoding": null,
    "freq": null,
    "id": "626f6172643a73636f726573",
    "length": null,
    "length_unit": "members",
    "name": "board:scores",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "zset"
  },
  "module": null,
  "more": false,
  "notice": "All 2 items are loaded.",
  "pages": 1,
  "pattern": null,
  "refused": [],
  "type": "zset",
  "value": null,
  "view": 7
}
```

Example, `GET /api/value?key=6576656e74733a6c6f67`. Reply: HTTP 200

```json
{
  "capped": false,
  "cluster": false,
  "command": "XREVRANGE",
  "cut": null,
  "done": true,
  "error": null,
  "field_ttls": true,
  "field_ttls_off": null,
  "gone": false,
  "items": [
    {
      "at": null,
      "fields": [
        {
          "name": "kind",
          "value": "logout"
        }
      ],
      "index": 0,
      "name": "1794745502183-1",
      "ttl": null,
      "value": null
    },
    {
      "at": null,
      "fields": [
        {
          "name": "kind",
          "value": "login"
        },
        {
          "name": "who",
          "value": "ada"
        }
      ],
      "index": 1,
      "name": "1794745502183-0",
      "ttl": null,
      "value": null
    }
  ],
  "key": {
    "copy": "events:log",
    "id": "6576656e74733a6c6f67",
    "name": "events:log"
  },
  "kind": "stream",
  "length": null,
  "loaded": 2,
  "loading": false,
  "max_items": 10000,
  "message": null,
  "meta": {
    "bytes": null,
    "copy": "events:log",
    "encoding": null,
    "freq": null,
    "id": "6576656e74733a6c6f67",
    "length": null,
    "length_unit": "entries",
    "name": "events:log",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "stream"
  },
  "module": null,
  "more": false,
  "notice": "All 2 items are loaded.",
  "pages": 1,
  "pattern": null,
  "refused": [],
  "type": "stream",
  "value": null,
  "view": 8
}
```

Example, `GET /api/value/item?view=8&index=1&field=1`. Reply: HTTP 200 (`$.shown.formats` cut to the first 3 of 8)

```json
{
  "field": 1,
  "index": 1,
  "name": "who",
  "shown": {
    "bytes": 3,
    "compression": null,
    "copy": "ada",
    "copy_refused": null,
    "detected": "text",
    "format": "text",
    "formats": [
      "text",
      "hex",
      "JSON"
    ],
    "header": "value · text · 3 bytes · auto-detected",
    "lines": [
      "ada"
    ],
    "note": null,
    "picked": null,
    "truncated": false
  }
}
```

Example, `GET /api/value/item?view=8&index=1`. Reply: HTTP 422

```json
{
  "error": "A stream entry has several fields: open one field to see or copy its value."
}
```

Example, `GET /api/value?key=6576696c1b5b33316d6b6579`. Reply: HTTP 200 (`$.value.formats` cut to the first 3 of 8)

```json
{
  "capped": false,
  "cluster": false,
  "command": "GETRANGE",
  "cut": null,
  "done": true,
  "error": null,
  "field_ttls": true,
  "field_ttls_off": null,
  "gone": false,
  "items": [],
  "key": {
    "copy": null,
    "id": "6576696c1b5b33316d6b6579",
    "name": "evil\\x1b[31mkey"
  },
  "kind": "string",
  "length": 1,
  "loaded": 1,
  "loading": false,
  "max_items": 10000,
  "message": null,
  "meta": {
    "bytes": 64,
    "copy": null,
    "encoding": "embstr",
    "freq": 5,
    "id": "6576696c1b5b33316d6b6579",
    "length": 1,
    "length_unit": "bytes",
    "name": "evil\\x1b[31mkey",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "string"
  },
  "module": null,
  "more": false,
  "notice": "The value is loaded.",
  "pages": 1,
  "pattern": null,
  "refused": [],
  "type": "string",
  "value": {
    "bytes": 1,
    "compression": null,
    "copy": "v",
    "copy_refused": null,
    "detected": "text",
    "format": "text",
    "formats": [
      "text",
      "hex",
      "JSON"
    ],
    "header": "value · text · 1 bytes · auto-detected",
    "lines": [
      "v"
    ],
    "note": null,
    "picked": null,
    "truncated": false
  },
  "view": 9
}
```

Example, `GET /api/value?key=6e6f3a737563683a6b6579`. Reply: HTTP 200

```json
{
  "capped": false,
  "cluster": false,
  "command": "GETRANGE",
  "cut": null,
  "done": false,
  "error": null,
  "field_ttls": true,
  "field_ttls_off": null,
  "gone": true,
  "items": [],
  "key": {
    "copy": "no:such:key",
    "id": "6e6f3a737563683a6b6579",
    "name": "no:such:key"
  },
  "kind": "string",
  "length": null,
  "loaded": 0,
  "loading": false,
  "max_items": 10000,
  "message": null,
  "meta": {
    "bytes": null,
    "copy": "no:such:key",
    "encoding": null,
    "freq": null,
    "id": "6e6f3a737563683a6b6579",
    "length": null,
    "length_unit": "bytes",
    "name": "no:such:key",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "string"
  },
  "module": null,
  "more": false,
  "notice": "The key no longer exists.",
  "pages": 0,
  "pattern": null,
  "refused": [],
  "type": "string",
  "value": null,
  "view": 10
}
```

Example, `GET /api/value?key=757365723a313a70726f66696c65`. Reply: HTTP 200 [valkey]

```json
{
  "capped": false,
  "cluster": false,
  "command": "HSCAN",
  "cut": null,
  "done": true,
  "error": null,
  "field_ttls": false,
  "field_ttls_off": null,
  "gone": false,
  "items": [
    {
      "at": null,
      "fields": null,
      "index": 0,
      "name": "name",
      "ttl": null,
      "value": "Ada"
    },
    {
      "at": null,
      "fields": null,
      "index": 1,
      "name": "email",
      "ttl": null,
      "value": "ada@example.com"
    }
  ],
  "key": {
    "copy": "user:1:profile",
    "id": "757365723a313a70726f66696c65",
    "name": "user:1:profile"
  },
  "kind": "hash",
  "length": 2,
  "loaded": 2,
  "loading": false,
  "max_items": 10000,
  "message": null,
  "meta": {
    "bytes": 88,
    "copy": "user:1:profile",
    "encoding": "listpack",
    "freq": 5,
    "id": "757365723a313a70726f66696c65",
    "length": 2,
    "length_unit": "fields",
    "name": "user:1:profile",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "hash"
  },
  "module": null,
  "more": false,
  "notice": "All 2 items are loaded.",
  "pages": 1,
  "pattern": null,
  "refused": [],
  "type": "hash",
  "value": null,
  "view": 4
}
```

### `GET /api/analysis`, `POST /api/analysis`, `DELETE /api/analysis`

- `GET /api/analysis`: `analysis` (as `state.analysis`), `running`, and `estimates`: what
  a Quick and a Deep analysis would cost, as the TUI's Analyze screen says it (`depth`,
  `sample_limit`, `keys`, `commands`, `per_sec`, `duration_secs`, `about`, `shards`,
  `sizes`, `freq`).
- `POST /api/analysis {"depth": "quick"|"deep"}` (Quick when left out) starts one, as the
  TUI's Analyze does: a Quick one samples up to 10,000 keys, a Deep one up to 100,000. The
  reply is 202 `{run, depth, sample_limit, estimate}`; progress comes as `progress`
  events. 400 for another depth, 409 while one runs, while connecting or after a failed
  connect, and while a bulk action counts or runs
  (`A bulk action is running: wait for it to end, or stop it first.`).
- `DELETE /api/analysis` stops the running one: it stops within one batch, and what it
  sampled becomes the report, marked stopped (partial, not saved). Stopped before any key
  is sampled, nothing changes. 200 `{stopping, text}`; 409 when none runs.

Example, `GET /api/analysis`. Reply: HTTP 200

```json
{
  "analysis": {
    "db": 0,
    "depth": null,
    "error": null,
    "note": null,
    "previous": false,
    "report": null,
    "run": 3,
    "sampled": null,
    "started_unix": null,
    "status": "none",
    "target": null
  },
  "estimates": [
    {
      "about": "as fast as the server answers",
      "commands": 303,
      "depth": "quick",
      "duration_secs": null,
      "freq": true,
      "keys": 74,
      "per_sec": null,
      "sample_limit": 10000,
      "shards": 0,
      "sizes": true
    },
    {
      "about": "as fast as the server answers",
      "commands": 303,
      "depth": "deep",
      "duration_secs": null,
      "freq": true,
      "keys": 74,
      "per_sec": null,
      "sample_limit": 100000,
      "shards": 0,
      "sizes": true
    }
  ],
  "running": false
}
```

Example, `POST /api/analysis {"depth":"quick"}`. Reply: HTTP 202

```json
{
  "depth": "quick",
  "estimate": {
    "about": "as fast as the server answers",
    "commands": 303,
    "depth": "quick",
    "duration_secs": null,
    "freq": true,
    "keys": 74,
    "per_sec": null,
    "sample_limit": 10000,
    "shards": 0,
    "sizes": true
  },
  "run": 4,
  "sample_limit": 10000
}
```

Example, `POST /api/analysis {}`. Reply: HTTP 409

```json
{
  "error": "An analysis is already running."
}
```

Example, `DELETE /api/analysis`. Reply: HTTP 200

```json
{
  "stopping": true,
  "text": "Stopping the analysis. What it sampled so far is kept."
}
```

### The Analyze panels

Each panel is the TUI's, from the session's report. Without one each is 409
`{"error":"No analysis yet: analyze the keys first."}`. Each has `report` (the report's
summary: `taken_at_unix`, `sampled_keys`, `complete`, `stopped`, `sizes_available`,
`previous`) and `empty` (why
there are no rows).

- `GET /api/analysis/big_keys`: the biggest sampled keys (`rows`: `key`, `pattern`,
  `length`, `length_unit`, `encoding`, `big`, `node`: the node's index on a cluster, else null, and `endpoint`: its `host:port`), and the recommendations ("Things to
  look at": `severity`, `text`, `hint`). Lengths and encodings are read for the listed keys
  (pipelined, per shard) once per report: a request that comes while they are read waits
  for that read instead of reading them again, and an `overview` event follows once they
  are in (Overview's findings use them). `facts_read` says they were, `notes` what was
  refused. `big_key_bytes` is the TUI's size for a big key.
- `GET /api/analysis/expiry`: patterns by TTL risk in the TUI's order (`status`,
  `explain`, `ttl_hist`, `no_ttl`, ...), and `freed`: the memory freed by TTL within each
  `within` horizon, `later`, `never` and `never_share`; `freed_off` says why it is not
  shown.
- `GET /api/analysis/changes`: this report against the newest snapshot before it.
  `status` is `pending` (being compared), `none` (`text` says there is no earlier one),
  `failed` (`text`) or `diff`: `before_*` and `after_*` times and `used_memory`, `rows` of
  patterns that changed (`keys_before`, `keys_after`, `bytes_before`, `bytes_after`,
  `change` with `delta_bytes`, `delta_keys`, `cause`, `cause_words`, `alarming`, and
  `trend` from the earlier snapshots when there are enough), `trend_points` and `note`.
- `GET /api/analysis/hot_keys`: the sampled keys with the highest LFU counters (`rows`:
  `key`, `pattern`, `node`, `endpoint`, `hot_and_big`). Off (`{"off": ...}`, the TUI's words) when the
  policy keeps no LFU counters (not an `*-lfu` policy) or `OBJECT FREQ` was refused.
- `GET /api/analysis/nodes`: on a cluster, each shard and the node read for it (`index`,
  `id`, `primary_id`, `endpoint`, `role`, `version`, `status`, `complete`, `keys`,
  `sampled_keys`, `patterns`, `memory`, `slots`, `slot_count`, `sizes_available`, `note`).
  Off on a standalone server.
- `GET /api/analysis/slots[?order=keys|cpu|net_in|net_out][&refresh=1]`: on a cluster,
  `CLUSTER SLOT-STATS` of each shard's slot ranges (at most four shards at once), read
  once per run unless `refresh=1`: `ranges` (per shard: `status`, `error`, the figures),
  `hottest` (the hottest slots by `order`, with the sampled keys in each), `order`,
  `memory_column`, `cpu_and_network_zero` and `note`. Off on a standalone server, on a
  version without the command (Redis before 8.2, Valkey before 8.0), or when it is refused.

Example, `GET /api/analysis/big_keys`. Reply: HTTP 200 (`$.recommendations` cut to the first 2 of 11; `$.rows` cut to the first 2 of 50)

```json
{
  "big_key_bytes": 67108864,
  "empty": null,
  "facts_read": true,
  "notes": [],
  "recommendations": [
    {
      "hint": "A lock should always carry a TTL.",
      "severity": "note",
      "text": "lock:job:{id}: 100% of its keys never expire."
    },
    {
      "hint": "Fine for data that must stay; caches should expire.",
      "severity": "info",
      "text": "57% of memory never expires: 5.5K of 9.6K."
    }
  ],
  "report": {
    "complete": true,
    "previous": false,
    "sampled_keys": 74,
    "sizes_available": true,
    "stopped": false,
    "taken_at_unix": 1794745502
  },
  "rows": [
    {
      "big": false,
      "encoding": "stream",
      "key": {
        "bytes": 4736,
        "copy": "events:log",
        "freq": 6,
        "id": "6576656e74733a6c6f67",
        "name": "events:log",
        "node": null,
        "ttl": "never",
        "ttl_secs": null,
        "type": "stream"
      },
      "length": 2,
      "length_unit": "entries",
      "endpoint": null,
      "node": null,
      "pattern": {
        "copy": "events:log",
        "id": "6576656e74733a6c6f67",
        "name": "events:log"
      }
    },
    {
      "big": false,
      "encoding": "listpack",
      "key": {
        "bytes": 104,
        "copy": "user:1:profile",
        "freq": 6,
        "id": "757365723a313a70726f66696c65",
        "name": "user:1:profile",
        "node": null,
        "ttl": "never",
        "ttl_secs": null,
        "type": "hash"
      },
      "length": 2,
      "length_unit": "fields",
      "endpoint": null,
      "node": null,
      "pattern": {
        "copy": "user:{id}:profile",
        "id": "757365723a7b69647d3a70726f66696c65",
        "name": "user:{id}:profile"
      }
    }
  ]
}
```

Example, `GET /api/analysis/expiry`. Reply: HTTP 200 (`$.freed.within` cut to the first 2 of 4; `$.rows` cut to the first 2 of 11; `$.rows[*].ttl_hist` cut to the first 2 of 7; `$.ttl_labels` cut to the first 2 of 7)

```json
{
  "empty": null,
  "freed": {
    "later": 0,
    "never": 5640,
    "never_share": 0.5708502024291497,
    "total": 9880,
    "within": [
      {
        "bytes": 4240,
        "label": "1 h"
      },
      {
        "bytes": 4240,
        "label": "1 d"
      }
    ]
  },
  "freed_off": null,
  "report": {
    "complete": true,
    "previous": false,
    "sampled_keys": 74,
    "sizes_available": true,
    "stopped": false,
    "taken_at_unix": 1794745502
  },
  "rows": [
    {
      "command": "redis-cli --scan --pattern \"events:log\"",
      "est_bytes": 4736,
      "est_keys": 1,
      "explain": [
        "All 1 keys never expire and the pattern has no earlier snapshot to compare with.",
        "This looks intentional."
      ],
      "no_ttl": 1,
      "no_ttl_ratio": 1.0,
      "pattern": {
        "copy": "events:log",
        "id": "6576656e74733a6c6f67",
        "name": "events:log"
      },
      "sampled_keys": 1,
      "status": "persistent · stable",
      "ttl_hist": [
        1,
        0
      ],
      "warning": false
    },
    {
      "command": "redis-cli --scan --pattern \"lock:job:*\"",
      "est_bytes": 280,
      "est_keys": 5,
      "explain": [
        "All 5 keys never expire and the pattern has no earlier snapshot to compare with.",
        "This looks intentional."
      ],
      "no_ttl": 5,
      "no_ttl_ratio": 1.0,
      "pattern": {
        "copy": "lock:job:{id}",
        "id": "6c6f636b3a6a6f623a7b69647d",
        "name": "lock:job:{id}"
      },
      "sampled_keys": 5,
      "status": "persistent · stable",
      "ttl_hist": [
        5,
        0
      ],
      "warning": false
    }
  ],
  "ttl_labels": [
    "none",
    "<1m"
  ]
}
```

Example, `GET /api/analysis/changes`. Reply: HTTP 200

```json
{
  "report": {
    "complete": true,
    "previous": false,
    "sampled_keys": 74,
    "sizes_available": true,
    "stopped": false,
    "taken_at_unix": 1794745502
  },
  "status": "none",
  "text": "No earlier snapshot for local. Save one; the next analysis compares against it."
}
```

Example, `GET /api/analysis/changes`. Reply: HTTP 200 (`$.rows` cut to the first 2 of 12)

```json
{
  "after_taken_at_unix": 1794745503,
  "after_utc": "2026-11-15 12:25:03 UTC",
  "before_taken_at_unix": 1794745502,
  "before_utc": "2026-11-15 12:25:02 UTC",
  "empty": null,
  "note": null,
  "report": {
    "complete": true,
    "previous": false,
    "sampled_keys": 75,
    "sizes_available": true,
    "stopped": false,
    "taken_at_unix": 1794745503
  },
  "rows": [
    {
      "bytes_after": 72,
      "bytes_before": null,
      "change": {
        "alarming": true,
        "cause": "new",
        "cause_words": "new",
        "delta_bytes": 72,
        "delta_keys": 1
      },
      "keys_after": 1,
      "keys_before": null,
      "partial": false,
      "pattern": {
        "copy": "cart:new",
        "id": "636172743a6e6577",
        "name": "cart:new"
      },
      "trend": null
    },
    {
      "bytes_after": 80,
      "bytes_before": 80,
      "change": {
        "alarming": false,
        "cause": "unchanged",
        "cause_words": "unchanged",
        "delta_bytes": 0,
        "delta_keys": 0
      },
      "keys_after": 1,
      "keys_before": 1,
      "partial": false,
      "pattern": {
        "copy": "board:scores",
        "id": "626f6172643a73636f726573",
        "name": "board:scores"
      },
      "trend": null
    }
  ],
  "status": "diff",
  "trend_points": null,
  "used_memory_after": 2018832,
  "used_memory_before": 1944104
}
```

Example, `GET /api/analysis/hot_keys`. Reply: HTTP 200 (`$.rows` cut to the first 2 of 50)

```json
{
  "empty": null,
  "hot_big_bytes": 10485760,
  "hot_freq": 100,
  "report": {
    "complete": true,
    "previous": false,
    "sampled_keys": 74,
    "sizes_available": true,
    "stopped": false,
    "taken_at_unix": 1794745502
  },
  "rows": [
    {
      "hot_and_big": false,
      "key": {
        "bytes": 72,
        "copy": "str:plain",
        "freq": 9,
        "id": "7374723a706c61696e",
        "name": "str:plain",
        "node": null,
        "ttl": "never",
        "ttl_secs": null,
        "type": "string"
      },
      "endpoint": null,
      "node": null,
      "pattern": {
        "copy": "str:plain",
        "id": "7374723a706c61696e",
        "name": "str:plain"
      }
    },
    {
      "hot_and_big": false,
      "key": {
        "bytes": 80,
        "copy": "queue:jobs",
        "freq": 7,
        "id": "71756575653a6a6f6273",
        "name": "queue:jobs",
        "node": null,
        "ttl": "never",
        "ttl_secs": null,
        "type": "list"
      },
      "endpoint": null,
      "node": null,
      "pattern": {
        "copy": "queue:jobs",
        "id": "71756575653a6a6f6273",
        "name": "queue:jobs"
      }
    }
  ]
}
```

Example, `GET /api/analysis/nodes`. Reply: HTTP 200

```json
{
  "off": "Not a cluster: this analysis read one standalone server."
}
```

Example, `GET /api/analysis/slots`. Reply: HTTP 200

```json
{
  "off": "Not a cluster: this is one standalone server."
}
```

Example, `GET /api/analysis/nodes`. Reply: HTTP 200 (`$.rows` cut to the first 1 of 3; `$.rows[*].slots[*]` cut to the first 1 of 2) [cluster redis]

```json
{
  "empty": null,
  "report": {
    "complete": true,
    "previous": false,
    "sampled_keys": 120,
    "sizes_available": true,
    "stopped": false,
    "taken_at_unix": 1794745506
  },
  "rows": [
    {
      "complete": true,
      "endpoint": "127.0.0.1:49152",
      "id": "db60446aa32515e6de67dd00619ee413492b6997",
      "index": 0,
      "keys": 37,
      "memory": 3036424,
      "note": null,
      "patterns": 10,
      "primary_id": "db60446aa32515e6de67dd00619ee413492b6997",
      "role": "primary",
      "sampled_keys": 37,
      "sizes_available": true,
      "slot_count": 5461,
      "slots": [
        [
          0
        ]
      ],
      "status": "scanned",
      "version": "7.4.12"
    }
  ]
}
```

Example, `GET /api/analysis/slots`. Reply: HTTP 200 [cluster redis]

```json
{
  "off": "CLUSTER SLOT-STATS needs Redis 8.2 or Valkey 8.0; 127.0.0.1:49152 runs Redis 7.4.12"
}
```

Example, `GET /api/analysis/slots`. Reply: HTTP 200 (`$.hottest` cut to the first 2 of 20; `$.ranges` cut to the first 2 of 3) [cluster valkey]

```json
{
  "cpu_and_network_zero": false,
  "empty": null,
  "hottest": [
    {
      "cpu_usec": null,
      "endpoint": "127.0.0.1:49358",
      "keys": 1,
      "memory": null,
      "net_in": null,
      "net_out": null,
      "node": 0,
      "sampled_keys": [
        {
          "key": {
            "copy": "item:18",
            "id": "6974656d3a3138",
            "name": "item:18"
          },
          "kinds": "big",
          "pattern": {
            "copy": "item:{id}",
            "id": "6974656d3a7b69647d",
            "name": "item:{id}"
          }
        }
      ],
      "slot": 687
    },
    {
      "cpu_usec": null,
      "endpoint": "127.0.0.1:49358",
      "keys": 1,
      "memory": null,
      "net_in": null,
      "net_out": null,
      "node": 0,
      "sampled_keys": [],
      "slot": 796
    }
  ],
  "memory_column": false,
  "note": null,
  "order": "keys",
  "ranges": [
    {
      "cpu_usec": null,
      "end": 5460,
      "endpoint": "127.0.0.1:49358",
      "error": null,
      "keys": 37,
      "memory": null,
      "net_in": null,
      "net_out": null,
      "node": 0,
      "start": 0,
      "status": "done"
    },
    {
      "cpu_usec": null,
      "end": 10922,
      "endpoint": "127.0.0.1:49359",
      "error": null,
      "keys": 44,
      "memory": null,
      "net_in": null,
      "net_out": null,
      "node": 1,
      "start": 5461,
      "status": "done"
    }
  ]
}
```

No `hot_keys` off reply was captured live (the test servers use `allkeys-lfu`); with
another policy it is `{"off": ...}` with the TUI's sentence, checked by the router test
`hot_keys_degrade_in_the_tuis_words` in `src/web/tests.rs`.

### `POST /api/snapshots`, `GET /api/snapshots`, `GET /api/snapshots/compare`

Snapshots are the TUI's: the same directory (the snapshot directory of the TUI and
`scan --save`, one subdirectory per connection), the same writer and so the same file,
which holds no credential. They are named by their file name (`20261115T122502Z.json`,
UTC).

- `POST /api/snapshots` saves the current report, as the TUI saves one: 201 `{file,
  taken_at_unix, taken_at_utc, sampled_keys, text}`, and Overview's `last_analysis`
  follows. 409, in the TUI's words, for no report, a running analysis, partial results or
  a report stopped early
  (`{"error":"This analysis was stopped early, so its results are partial and are not saved. Analyze again to save one."}`).
  500 `{"error":"The snapshot could not be saved: the snapshot directory could not be written."}`
  when the file can't be written (the file system's text, which names paths, is never
  passed on).
- Snapshots of every database of a profile go to the same directory, as in the TUI: after
  a database switch, Changes and `compare` set the new database's analysis against the
  other database's snapshots.
- `GET /api/snapshots`: the connection's snapshots, newest first (`file`, `taken_at_unix`,
  `taken_at_utc`, `current`: the report the panels show), `profile`, `can_save`,
  `save_off` (why not) and `saved` (the current report is saved). 500
  `{"error":"The snapshots could not be listed: the snapshot directory could not be read."}`
  when the directory can't be read.
- `GET /api/snapshots/compare?a=<file|current>&b=<file|current>`: the newer of the two
  against the older, as the Changes panel shows it (the `diff` body above without
  `status` and `report`, with `before_sampled_keys` and `after_sampled_keys`). A name is a
  file name exactly as `GET /api/snapshots` gives it, or `current`; anything else is 400
  before any file is opened, a file of another connection or none is 404, the same name
  twice is 422, a file that is not a snapshot is 422, `current` without a finished report
  (none yet, or an analysis running) is 409
  `{"error":"No analysis yet: analyze the keys first."}`, a directory that can't be read
  is 500 (the listing's sentence above). A missing `a` or `b` is 400 ("Conventions").

Example, `POST /api/snapshots {}`. Reply: HTTP 201

```json
{
  "file": "20261115T122502Z.json",
  "sampled_keys": 74,
  "taken_at_unix": 1794745502,
  "taken_at_utc": "2026-11-15 12:25:02 UTC",
  "text": "Saved snapshot 20261115T122502Z.json"
}
```

Example, `GET /api/snapshots`. Reply: HTTP 200

```json
{
  "can_save": true,
  "profile": "local",
  "save_off": null,
  "saved": true,
  "snapshots": [
    {
      "current": true,
      "file": "20261115T122503Z.json",
      "taken_at_unix": 1794745503,
      "taken_at_utc": "2026-11-15 12:25:03 UTC"
    },
    {
      "current": false,
      "file": "20261115T122502Z.json",
      "taken_at_unix": 1794745502,
      "taken_at_utc": "2026-11-15 12:25:02 UTC"
    }
  ]
}
```

Example, `GET /api/snapshots/compare?a=20261115T122502Z.json&b=20261115T122503Z.json`. Reply: HTTP 200 (`$.rows` cut to the first 2 of 12)

```json
{
  "after_sampled_keys": 75,
  "after_taken_at_unix": 1794745503,
  "after_utc": "2026-11-15 12:25:03 UTC",
  "before_sampled_keys": 74,
  "before_taken_at_unix": 1794745502,
  "before_utc": "2026-11-15 12:25:02 UTC",
  "empty": null,
  "note": null,
  "rows": [
    {
      "bytes_after": 72,
      "bytes_before": null,
      "change": {
        "alarming": true,
        "cause": "new",
        "cause_words": "new",
        "delta_bytes": 72,
        "delta_keys": 1
      },
      "keys_after": 1,
      "keys_before": null,
      "partial": false,
      "pattern": {
        "copy": "cart:new",
        "id": "636172743a6e6577",
        "name": "cart:new"
      },
      "trend": null
    },
    {
      "bytes_after": 80,
      "bytes_before": 80,
      "change": {
        "alarming": false,
        "cause": "unchanged",
        "cause_words": "unchanged",
        "delta_bytes": 0,
        "delta_keys": 0
      },
      "keys_after": 1,
      "keys_before": 1,
      "partial": false,
      "pattern": {
        "copy": "board:scores",
        "id": "626f6172643a73636f726573",
        "name": "board:scores"
      },
      "trend": null
    }
  ],
  "trend_points": null,
  "used_memory_after": 2018832,
  "used_memory_before": 1944104
}
```

## Edits and bulk actions

Every write Browse can make in the TUI (spec "Edits on a key's value (P3)", "Bulk actions
(P3)"): the open value's edits, a new key, the key's TTL, rename and delete, and a bulk
expire or delete over a pattern's keys. Each one is the TUI's own write: the dashboard
builds the TUI's `WriteOp` from what the open value or the pattern holds, and it goes
through the one write gate (spec "Write permissions") as every other write does. That is:
decided, then asked about with a 409 `confirm` when the gate asks, a permit given once it
is answered, then sent through the live layer. Nothing is built from the request alone,
and no route sends a command of its own.

The rules every route here keeps:

- **A read-only profile is refused first**, with the gate's words, before anything else is
  looked at, and nothing is sent, `SCAN` included:
  422 `{"error":"Not sent: ro is read-only. Turn writes on in Connections."}`. The
  profile's `read_only` and `env` are read again from the config file before each write
  and each answer, so writes turned off in another window stop the next one.
- **Prod asks.** An edit asks yes or no. Deleting a key, a field, an element, a member or
  an entry needs the key's name typed. A bulk action needs the pattern typed. On every
  other profile an edit is sent at once, a bulk action asks yes or no, and a delete asks
  yes or no unless the `skip_non_prod_confirm` setting is on (the TUI's table).
- **An answer is for the run it was asked in.** After a database switch, an analysis
  started or a reconnect, the answer is refused: 409
  `{"error":"Not sent: the connection changed since the question was asked."}`. An edit
  is also decided only for the run its value was read in: one that started while the
  request was on its way gets the same 409, and nothing is sent.
- **No edit while a bulk action counts or runs**, as the TUI's bulk dialog takes every key
  until it ends: 409 `{"error":"A bulk action is running: wait for it to end, or stop it first."}`. A key added or written behind the walk
  would otherwise outlive "every key of the pattern". An edit asked before the bulk action
  started and answered while it counts or runs gets the same 409, and nothing is sent: the
  check is made where the write is decided and again where its answer is granted.
- **A command the server refused for this user is off** for the rest of the session: the
  first refusal is `"Not written: the server refused HSET for this user."` (422); after
  that it is refused before anything is sent, `"Not sent: the server refused HSET for
  this user."` (422), and `state.refused` lists it (a `state` event says so at once).
  Neither the username nor the password is in any reply, event or log.
- **Some edits need `WATCH`, `MULTI` and `EXEC` too.** A string edit (`SET … KEEPTTL`), a
  list element's edit or delete (`LSET`, `LREM`) and a new key of a type other than a
  string are sent in `MULTI`/`EXEC` after `WATCH`; a set member's edit in `MULTI`/`EXEC`.
  A user with `+set` but without `+watch`, `+multi` or `+exec` can't make them. A refused
  `WATCH` is `"Not written: the server refused WATCH for this user."` (422), and it turns
  the edit off by the edit's own command, as the TUI does: `state.refused` lists `SET`
  (`LSET`, …), and the next try is `"Not sent: the server refused SET for this user."`.
- **One write at a time per value.** A second one while the first has not answered is
  409 `{"error":"The last write has not answered yet."}`; the body's `writing` is `true`
  meanwhile.
- **Values are never logged.** The request bodies print no value in `Debug`, and the
  replies show values only where `GET /api/value` shows them.

### The open value's edits

Each route names the value with `view` (from `GET /api/value`). The routes that pick an
item also give `rev`: the `rev` of the value body the item's `index` was read from. Each
reread of the value after a write adds one to `rev` (every write but the key's TTL, which
reads the key's row only and leaves `rev` as it is); an older `rev` is refused, so an
index never points at another item than the one the page showed. The value stays
`writing` until it was read again, and a page of it still on its way when the write
answers (another tab's `GET /api/value/more`) lands first: the reread waits for it, so
`rev` always moves.

| Route | Body | Sends |
|---|---|---|
| `POST /api/value/edit` | `{view, rev, index, value}` (`index` 0 for a string) | a string: `SET … KEEPTTL`, in `MULTI`/`EXEC` after `WATCH`, only while it is the string loaded. A hash field: `HSET`. A list element: `LSET`, only while it is the element loaded. A set member: `SREM` and `SADD` in `MULTI`/`EXEC`. A sorted set member: its score, `ZADD … XX` (`value` is the score as text). A RedisJSON document: `JSON.SET … $`. A stream entry is not edited (422) |
| `POST /api/value/add` | `{view, field?, value, score?}` | a hash: `HSETNX field value`, never over a field. A list: `RPUSH value`. A set: `SADD value`. A sorted set: `ZADD … NX score value`. A stream: `XADD * field value` |
| `POST /api/value/ttl` | `{view, ttl, field?, rev?}` | the key: `EXPIRE` or, for `-` (`persist`, `none`), `PERSIST`. A hash field (`field` is its index, `rev` is needed with it): `HEXPIRE` or `HPERSIST`, where field TTLs are read (Redis 7.4 and later, Valkey 9.0 and later) |
| `POST /api/value/rename` | `{view, name}` | `RENAMENX`: it never overwrites a key. On a cluster both names must be in the same slot |
| `POST /api/value/remove` | `{view, rev, index}` | a hash field: `HDEL`. A set member: `SREM`. A sorted set member: `ZREM`. A stream entry: `XDEL`. A list element: `LSET` to a marker then `LREM` of the marker, only while it is the element loaded |
| `POST /api/value/delete` | `{view}` | `UNLINK` |

- **`value`, `field`**: text as typed (`"42"`), or bytes as hex (`{"hex": "00ff"}`). An odd
  or non-hex `hex` is 400
  `{"error":"a value given as hex must be an even number of hex digits"}`.
- **`ttl`**: seconds (`90`), or a number with `s`, `m`, `h`, `d` or `w`, several in a row
  (`1h30m`); `-`, `persist` or `none` removes it. 0 is refused:
  422 `{"error":"A TTL of 0 would delete the key: delete the key instead."}`. Anything
  else is 422 `{"error":"Type a TTL such as 90s, 10m, 24h or 7d, or - to remove it."}`;
  what was typed is never echoed.
- **`score`**: a number, `inf` or `-inf`;
  otherwise 422 `{"error":"Type the score as a number, such as 1.5, or inf or -inf."}`.
- Unknown fields in a body are 400 ("Conventions").

Refusals before anything is sent (besides the read-only one above):

| Status | When | `error` |
|---|---|---|
| 404 | `view` is not open (another value replaced it, or the session changed) | `That value is no longer open (a newer one replaced it, or the session changed): open it again.` |
| 409 | `rev` is not the value's `rev` now | `The value was read again since the page showed it: look at it again, then try once more.` |
| 409 | the value is still loading | `Still loading the value: wait for it.` |
| 409 | another write of this value has not answered, or it is being read again after one | `The last write has not answered yet.` |
| 409 | a bulk action counts or runs | `A bulk action is running: wait for it to end, or stop it first.` |
| 409 | the session started another run (another database, an analysis) or connection after the edit was made from the value | `Not sent: the connection changed since the question was asked.` |
| 409 | no live connection | `not connected: connect first, or the live connection is closed` |
| 422 | the key no longer exists | `The key no longer exists.` |
| 422 | `index` is past what is loaded | `There is no such item in what is loaded.` |
| 422 | a stream entry edited | `A stream entry can't be edited: add an entry, or delete this one.` |
| 422 | adding to a string or a document | `Adding works on a hash, list, set, sorted set or stream: edit this value instead.` |
| 422 | removing from a string or a document | `Removing works on a field, element, member or entry: delete the key instead.` |
| 422 | a field TTL on another type than a hash | `Only a hash field has a TTL of its own.` |
| 422 | a field TTL where the server has none | `Field TTLs need Redis 7.4 or later, or Valkey 9.0 or later.` |
| 422 | a rename to an empty name, or across slots on a cluster | `Type the new name.`; `Not sent: the keys are in different slots (CROSSSLOT), …` |

**The reply** of a write that was sent (200, the same for the answer of a question):

| Field | What |
|---|---|
| `sent` | `true`: the command was sent. `false` only for an edit that changes nothing (`"message":"Not changed, so nothing was sent."`) |
| `done` | whether it did what was asked: `false` when the value changed since it was loaded, a field or member exists already, the new name is taken, the key is gone |
| `message` | what happened, in the TUI's words without its keys: `Saved.`, `Added.`, `Deleted.`, `Deleted <key>.`, `Renamed to <name>.`, `TTL set to 1h 30m.`, `TTL removed: the key no longer expires.`, `The key had no TTL.`, `Field TTL set.`, `Not written: the value changed since it was loaded. It is read again now.`, `Not added: the field exists already. Edit it instead.`, `Not added: the member exists already.`, `Not renamed: <name> exists already, and a rename never overwrites a key.`, `Not set: the key no longer exists.`, `Nothing deleted: it was gone already.` |
| `reply` | the server's answer, as `POST /api/cmdline` gives one (`{"kind":"ok"}`, `{"kind":"int","value":1}`, `{"kind":"changed"}`, `{"kind":"exists"}`) |
| `key` | the key's name (the new one after a rename), as "Key and pattern names" says |
| `closed` | `true` when the key was deleted: the value is closed (its `view` is 404 from then on) and the key is gone from every key list |
| `value` | the value read again from its first page (the body of `GET /api/value`, `rev` one higher), or `null` once closed. The key's TTL alone does not reread the value (a field's does) |
| `row` | the key's row read again (TTL, size, length, encoding), as the key lists now show it; `null` once closed |

A failure the server gave is 422 `{"error":"Not written: <why>."}`. A live connection
closed meanwhile is 409. A cancelled question is 200 from `DELETE /api/confirm/<id>`.

In the examples after the first, `value` and `row` are cut short.

Example, `POST /api/value/edit {"index":0,"rev":0,"value":"world","view":1}`. Reply: HTTP 200

```json
{
  "closed": false,
  "done": true,
  "key": {
    "copy": "str:a",
    "id": "7374723a61",
    "name": "str:a"
  },
  "message": "Saved.",
  "reply": {
    "kind": "ok"
  },
  "row": {
    "bytes": null,
    "copy": "str:a",
    "encoding": "embstr",
    "freq": null,
    "id": "7374723a61",
    "length": 5,
    "length_unit": "bytes",
    "name": "str:a",
    "node": null,
    "ttl": "1d 03h",
    "ttl_secs": 99999,
    "type": "string"
  },
  "sent": true,
  "value": {
    "capped": false,
    "cluster": false,
    "command": "GETRANGE",
    "cut": null,
    "done": true,
    "error": null,
    "field_ttls": true,
    "field_ttls_off": null,
    "gone": false,
    "items": [],
    "key": {
      "copy": "str:a",
      "id": "7374723a61",
      "name": "str:a"
    },
    "kind": "string",
    "length": 5,
    "loaded": 1,
    "loading": false,
    "max_items": 10000,
    "message": null,
    "meta": {
      "bytes": null,
      "copy": "str:a",
      "encoding": "embstr",
      "freq": null,
      "id": "7374723a61",
      "length": 5,
      "length_unit": "bytes",
      "name": "str:a",
      "node": null,
      "ttl": "1d 03h",
      "ttl_secs": 99999,
      "type": "string"
    },
    "module": null,
    "more": false,
    "notice": "The value is loaded.",
    "pages": 1,
    "pattern": null,
    "refused": [],
    "rev": 1,
    "type": "string",
    "value": {
      "bytes": 5,
      "compression": null,
      "copy": "world",
      "copy_refused": null,
      "detected": "text",
      "format": "text",
      "formats": [
        "text",
        "hex",
        "JSON",
        "MessagePack",
        "PHP",
        "Pickle",
        "Protobuf",
        "Java"
      ],
      "header": "value · text · 5 bytes · auto-detected",
      "lines": [
        "world"
      ],
      "note": null,
      "picked": null,
      "truncated": false
    },
    "view": 1,
    "writing": false
  }
}
```

The value changed behind the page's back: not written, and read again.

Example, `POST /api/value/edit {"rev":1,"value":"mine","view":1}`. Reply: HTTP 200

```json
{
  "closed": false,
  "done": false,
  "key": {
    "copy": "str:a",
    "id": "7374723a61",
    "name": "str:a"
  },
  "message": "Not written: the value changed since it was loaded. It is read again now.",
  "reply": {
    "kind": "changed"
  },
  "row": {
    "name": "str:a",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "string"
  },
  "sent": true,
  "value": {
    "...": "the body of GET /api/value",
    "rev": 2,
    "view": 1
  }
}
```

Example, `POST /api/value/edit {"rev":1,"value":"mine","view":1}`. Reply: HTTP 409

```json
{
  "error": "The value was read again since the page showed it: look at it again, then try once more."
}
```

Example, `POST /api/value/add {"field":"age","value":"42","view":2}`. Reply: HTTP 200

```json
{
  "closed": false,
  "done": true,
  "key": {
    "copy": "user:1",
    "id": "757365723a31",
    "name": "user:1"
  },
  "message": "Added.",
  "reply": {
    "kind": "int",
    "value": 1
  },
  "row": {
    "name": "user:1",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "hash"
  },
  "sent": true,
  "value": {
    "...": "the body of GET /api/value",
    "rev": 2,
    "view": 2
  }
}
```

Example, `POST /api/value/add {"field":"age","value":"7","view":2}`. Reply: HTTP 200

```json
{
  "closed": false,
  "done": false,
  "key": {
    "copy": "user:1",
    "id": "757365723a31",
    "name": "user:1"
  },
  "message": "Not added: the field exists already. Edit it instead.",
  "reply": {
    "kind": "int",
    "value": 0
  },
  "row": {
    "name": "user:1",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "hash"
  },
  "sent": true,
  "value": {
    "...": "the body of GET /api/value",
    "rev": 2,
    "view": 2
  }
}
```

On every profile a delete asks.

Example, `POST /api/value/remove {"index":1,"rev":2,"view":2}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "b92d295ebb1139de93c82a4b3007b04c",
    "kind": "ask",
    "text": "Send HDEL user:1 mail to local? This can't be undone."
  }
}
```

Example, `POST /api/confirm/<id> {}`. Reply: HTTP 200

```json
{
  "closed": false,
  "done": true,
  "key": {
    "copy": "user:1",
    "id": "757365723a31",
    "name": "user:1"
  },
  "message": "Deleted.",
  "reply": {
    "kind": "int",
    "value": 1
  },
  "row": {
    "name": "user:1",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "hash"
  },
  "sent": true,
  "value": {
    "...": "the body of GET /api/value",
    "rev": 3,
    "view": 2
  }
}
```

Example, `POST /api/value/ttl {"field":1,"rev":3,"ttl":"1h","view":2}`. Reply: HTTP 200

```json
{
  "closed": false,
  "done": true,
  "key": {
    "copy": "user:1",
    "id": "757365723a31",
    "name": "user:1"
  },
  "message": "Field TTL set.",
  "reply": {
    "kind": "int",
    "value": 1
  },
  "row": {
    "name": "user:1",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "hash"
  },
  "sent": true,
  "value": {
    "...": "the body of GET /api/value",
    "rev": 4,
    "view": 2
  }
}
```

Example, `POST /api/value/ttl {"ttl":"-","view":2}`. Reply: HTTP 200

```json
{
  "closed": false,
  "done": true,
  "key": {
    "copy": "user:1",
    "id": "757365723a31",
    "name": "user:1"
  },
  "message": "TTL removed: the key no longer expires.",
  "reply": {
    "kind": "int",
    "value": 1
  },
  "row": {
    "name": "user:1",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "hash"
  },
  "sent": true,
  "value": {
    "...": "the body of GET /api/value",
    "rev": 4,
    "view": 2
  }
}
```

Example, `POST /api/value/ttl {"ttl":"0","view":2}`. Reply: HTTP 422

```json
{
  "error": "A TTL of 0 would delete the key: delete the key instead."
}
```

Example, `POST /api/value/edit {"index":0,"rev":0,"value":"x","view":6}`. Reply: HTTP 422

```json
{
  "error": "A stream entry can't be edited: add an entry, or delete this one."
}
```

Example, `POST /api/value/rename {"name":"taken:1","view":7}`. Reply: HTTP 200

```json
{
  "closed": false,
  "done": false,
  "key": {
    "copy": "tags:1",
    "id": "746167733a31",
    "name": "tags:1"
  },
  "message": "Not renamed: taken:1 exists already, and a rename never overwrites a key.",
  "reply": {
    "kind": "int",
    "value": 0
  },
  "row": {
    "name": "tags:1",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "set"
  },
  "sent": true,
  "value": {
    "...": "the body of GET /api/value",
    "rev": 0,
    "view": 7
  }
}
```

Example, `POST /api/value/rename {"name":"tags:2","view":7}`. Reply: HTTP 200

```json
{
  "closed": false,
  "done": true,
  "key": {
    "copy": "tags:2",
    "id": "746167733a32",
    "name": "tags:2"
  },
  "message": "Renamed to tags:2.",
  "reply": {
    "kind": "int",
    "value": 1
  },
  "row": {
    "name": "tags:2",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "set"
  },
  "sent": true,
  "value": {
    "...": "the body of GET /api/value",
    "rev": 0,
    "view": 7
  }
}
```

Example, `POST /api/value/delete {"view":8}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "44256ebe5f8c20e2940d68b48da3c199",
    "kind": "ask",
    "text": "Send UNLINK rank:1 to local? This can't be undone."
  }
}
```

Example, `POST /api/confirm/<id> {}`. Reply: HTTP 200

```json
{
  "closed": true,
  "done": true,
  "key": {
    "copy": "rank:1",
    "id": "72616e6b3a31",
    "name": "rank:1"
  },
  "message": "Deleted rank:1.",
  "reply": {
    "kind": "int",
    "value": 1
  },
  "row": null,
  "sent": true,
  "value": null
}
```

On prod an edit asks yes or no, and a delete needs the key typed:

Example, `POST /api/value/edit {"rev":0,"value":"prod-edit","view":1}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "f70bf2d3ba03ad359879438e425921d1",
    "kind": "ask",
    "text": "Send SET str:a prod-edit KEEPTTL  (in MULTI/EXEC after WATCH, only if unchanged) to prod1?"
  }
}
```

Example, `POST /api/value/delete {"view":1}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": "str:a",
    "id": "d4c5243a2451656816a56e9e186ccc3a",
    "kind": "type",
    "text": "Type str:a to send UNLINK str:a to prod1. This can't be undone."
  }
}
```

Example, `POST /api/confirm/<id> {"typed":"prod1"}`. Reply: HTTP 422

```json
{
  "error": "Not sent: the name typed did not match."
}
```

Example, `POST /api/confirm/<id> {"typed":"str:a"}`. Reply: HTTP 200

```json
{
  "closed": true,
  "done": true,
  "key": {
    "copy": "str:a",
    "id": "7374723a61",
    "name": "str:a"
  },
  "message": "Deleted str:a.",
  "reply": {
    "kind": "int",
    "value": 1
  },
  "row": null,
  "sent": true,
  "value": null
}
```

An answer after a database switch:

Example, `POST /api/confirm/<id> {}`. Reply: HTTP 409

```json
{
  "error": "Not sent: the connection changed since the question was asked."
}
```

### `POST /api/keys/add`

The TUI's `N`: a key with its first item, only when it does not exist. A string is
`SET … NX`. The other types go in `MULTI`/`EXEC` after `WATCH` and `EXISTS`, so a key
written by someone else meanwhile is not overwritten.

Body: `{list?, name, type, field?, value, score?}`.

- `type` is one of `string`, `hash`, `list`, `set`, `zset` or `stream`; anything else is
  422 `{"error":"the type must be one of string, hash, list, set, zset or stream"}`.
- `field` is a hash's first field or a stream entry's field. `value` is the value,
  element or member. `score` is a sorted set's first member's score.
- `list`, when given, is the key list it is added from (`GET /api/keys` or
  `/api/find`). The key is listed there once added. A list that is gone is 404
  `{"error":"That key list is gone (a newer one replaced it, or the session changed): open it again."}`.

The reply is the write reply above. Once added, `value` is the new key opened (its
first page read) and `row` its row (`node` on a cluster: the primary that owns its slot).
`row.length` is in `length_unit`, as on a key list: a string's bytes (5 for `hello`), and
1 for the other types (`fields`, `items`, `members`, `entries`). `bytes` is the memory
`MEMORY USAGE` reports, not the length: `null` until the key list reads it. The value's
`pattern` is the pattern the key belongs to, as `GET /api/value` gives it (the analysis
report's, else the pattern of the key list it was added from when the key infers to it,
else `null`); a find's text is never a pattern.
A key that exists is not overwritten: `done: false`, `"Not added: <key> exists already,
and adding a key never overwrites one."`. One written meanwhile gives `done: false`,
`"Not added: <key> was written by someone else meanwhile."`.

Example, `POST /api/keys/add {"list":1,"name":"new:hello","type":"string","value":"hello"}`. Reply: HTTP 200 (`$.value` cut to some of its fields)

```json
{
  "closed": false,
  "done": true,
  "key": {
    "copy": "new:hello",
    "id": "6e65773a68656c6c6f",
    "name": "new:hello"
  },
  "message": "Added new:hello.",
  "reply": {
    "kind": "ok"
  },
  "row": {
    "bytes": null,
    "copy": "new:hello",
    "encoding": null,
    "freq": null,
    "id": "6e65773a68656c6c6f",
    "length": 5,
    "length_unit": "bytes",
    "name": "new:hello",
    "node": null,
    "ttl": "never",
    "ttl_secs": null,
    "type": "string"
  },
  "sent": true,
  "value": {
    "...": "the body of GET /api/value",
    "kind": "string",
    "length": 5,
    "meta": {
      "bytes": null,
      "length": 5,
      "length_unit": "bytes",
      "...": "the same fields as row"
    },
    "pattern": null,
    "rev": 0,
    "type": "string",
    "view": 15
  }
}
```

Example, `POST /api/keys/add {"name":"taken:1","type":"string","value":"over"}`. Reply: HTTP 200

```json
{
  "closed": false,
  "done": false,
  "key": {
    "copy": "taken:1",
    "id": "74616b656e3a31",
    "name": "taken:1"
  },
  "message": "Not added: taken:1 exists already, and adding a key never overwrites one.",
  "reply": {
    "kind": "exists"
  },
  "row": null,
  "sent": true,
  "value": null
}
```

Example, `POST /api/keys/add {"list":999,"name":"taken:1","type":"string","value":"over"}`. Reply: HTTP 404

```json
{
  "error": "That key list is gone (a newer one replaced it, or the session changed): open it again."
}
```

### Bulk actions

A bulk expire or delete over **one pattern's keys** (Browse › Keys, a pattern row): the
TUI's `x` and `D` on a pattern. There is no bulk action over a Find result. It is the
TUI's run, step for step:

1. `POST /api/bulk` checks it can run, then starts the **dry run**. The dry run walks the
   pattern's glob with `SCAN MATCH … COUNT`, reads each key's type and TTL, and counts:
   - the keys of the pattern
   - those with a TTL already
   - the keys the glob matched but another pattern owns (they are left alone)
   - the keys it could not read

   Nothing is written during the dry run.
2. Once counted, `POST /api/bulk/run` asks. Prod needs the pattern typed; any other
   profile asks yes or no. The question gives the dry run's numbers and what is sent.
   When nothing would change, the dry run ends saying so and nothing is asked.
3. Answered yes, the run starts. Batches of 500 keys, each read again by the walk, so a
   key that left the pattern or got a TTL meanwhile is skipped:
   - **expire**: `EXPIRE <key> <secs> NX` on the keys without a TTL. This needs Redis 7.0
     or later, or any Valkey.
   - **delete**: `UNLINK <key>`.

   Each batch is booked on the command budget. Each key a batch sends a command for is
   written to a **key file** before the batch goes, so an expire can be undone with
   `PERSIST` and a delete is on record. It lists the keys sent, those whose command then
   failed or was refused included (a batch refused with `NOPERM` leaves its keys listed
   and none changed); `run.changed` says how many changed. On a
   cluster each primary is walked in turn, in slot order, with no redirect followed;
   each line of the key file is the node's address, a tab, then the key.
4. `DELETE /api/bulk` stops it. A dry run stops at once and nothing was sent. A run with
   a batch on its way stops once that batch answers; a run between two batches, or
   answered but before its first batch, stops at once. No batch is sent after a stop.

Before every batch after the first, the profile is read again from the config file. A
profile turned read-only stops the run (`"Stopped: <name> is now read-only. …"`), and so
would a new run of the session (`"Stopped: the connection changed. …"`), which the
dashboard refuses while a bulk action runs (below). A batch the server
refuses with `NOPERM` stops the run, and that command is off for the session.

One bulk action per connection: a new one replaces one that ended or waits for its
question. While one is counting
or running, `POST /api/bulk` is 409 `{"error":"A bulk action is running already: wait for it to end, or stop it first."}`.
Meanwhile nothing else may change the session or the keys, as the TUI's bulk dialog takes
every key until it ends: a database switch (`POST /api/db`), an analysis
(`POST /api/analysis`), a connect or a disconnect, and every key edit ("The open value's
edits", `POST /api/keys/add`) are 409 `{"error":"A bulk action is running: wait for it to end, or stop it first."}`. A dry run whose session
started another run anyway ends with
`"The dry run stopped: the connection changed. Nothing was sent."` (`error: true`).

**Routes.** Every reply, and every `bulk` event, is `{"bulk": <body> | null}`:

- **`GET /api/bulk`**: the connection's bulk action, or `null`.
- **`POST /api/bulk {pattern, action, ttl?}`**: 202. `pattern` is the pattern's `id`
  ("Key and pattern names"). `action` is `delete` or `expire`; anything else is 400
  `{"error":"the action must be expire or delete"}`. `ttl` is for `expire` only, and
  defaults to `24h`. It takes seconds, or a number with `s`, `m`, `h` or `d`. A TTL that
  is not a duration is 422 `{"error":"not a duration: use 90s, 10m, 24h or 7d"}` (what
  was typed is never echoed); 0 is 422 `{"error":"the TTL must be at least 1s"}`. The
  refusals, in the order they are checked:
  - the gate: a read-only profile (422), or a command already refused for this user
    (422)
  - another bulk action counting or running (409)
  - a replica, or a cluster whose slots are not known (422, the TUI's words)
  - an expire on Redis before 7.0 (422, `"… needs Redis 7.0 or later …"`)
  - no live connection (409)
- **`POST /api/bulk/run {bulk}`**: `bulk` is the body's `id`. It answers 409 with a
  `confirm`, and the answer is 202 with the running body. The refusals:
  - 404 `That bulk action is no longer open: start it again from its pattern.`
  - 409 `The dry run has not finished: wait for its count.`
  - 409 `That bulk action has started already.`
  - 422 `Nothing to run: the dry run found nothing to change.`
  - 409 `Not sent: the connection changed since the question was asked.` (another run
    since it was counted)

  A wrong typed name is 422 and keeps the bulk action counted, to be asked again.
  Cancelling the question (`DELETE /api/confirm/<id>`) ends the bulk action:
  `"lines": ["Cancelled: nothing was sent."]`. The answer
  (`POST /api/confirm/<id>`) is 410
  `That bulk action is no longer open: start it again from its pattern.` when the bulk
  action ended meanwhile (stopped, or its other question cancelled) or another replaced
  it, and 409 `That bulk action has started already.` when another answer started it.
- **`DELETE /api/bulk`**: 200 with the body and a `text`:
  - a dry run: stopped, `Dry run stopped. Nothing was sent.`
  - counted, not run: closed, `Bulk action cancelled. Nothing was sent.`
  - a run with a batch on its way: still `running`, `"stopping": true`,
    `Stopping after the batch on its way.`; done once that batch answered
    (`Stopped after the batch: …`), with no batch after it.
  - a run between two batches, or before its first: done at once,
    `Stopped: no batch was on its way, and no more is sent.`, its lines
    `Stopped after the batch: …` or `Stopped before the first batch: nothing was sent.`
  - an ended one: cleared, `{"bulk": null}`.

**The body:**

| Field | What |
|---|---|
| `id` | names it for `POST /api/bulk/run` |
| `pattern`, `glob` | the pattern ("Key and pattern names") and the glob `SCAN MATCH` walks |
| `action`, `ttl`, `ttl_secs`, `title` | `delete` or `expire`; an expire's TTL (`2h`) and in seconds; the title (`Delete every key of the pattern`, `Set a TTL on every key of the pattern`) |
| `profile`, `prod` | the profile it runs on, and whether it is a prod one (the pattern is typed) |
| `stage` | `counting`, `counted`, `running`, `done` |
| `count` | the dry run: `matched` (keys of the pattern), `with_ttl`, `other_pattern` (matched the glob, left alone), `unreadable`, `to_change` |
| `summary` | the dry run in sentences, the TUI's, once the count is complete (empty while counting, and after a dry run stopped part way) |
| `nodes`, `at` | a cluster: each primary walked, `{node, endpoint, matched}`, and the index in `nodes` the walk is on (`[]` and `null` on one server) |
| `run` | once it runs: `seen`, `sent`, `changed`, `skipped` (a TTL already, or gone), `failed`, `unreadable`, `moved` (in a slot that moved: left alone), `file` (the key file), `stopping` |
| `lines` | once done: what happened, the TUI's lines without its keys. `Done: deleted 1,200 keys of cart:{id}.` / `Stopped after the batch: …` / `Stopped before the first batch: nothing was sent.` / `Stopped: <why>. Before that, …`, then the skipped, unreadable, moved and failed counts, `Key names: <file>` and, for an expire, `PERSIST on those keys undoes it.` |
| `error` | `true` when it stopped on a failure |

The key files are the TUI's: in the `bulk` directory of redis-lens's data directory, one
subdirectory per profile, named `<UTC time>-<action>.txt`. They hold key names only,
never a value or a credential. In the examples below their directory is shortened to
`<data dir>`.

Example, `POST /api/bulk {"action":"expire","pattern":"636172743a7b69647d","ttl":"2h"}`. Reply: HTTP 202

```json
{
  "bulk": {
    "action": "expire",
    "at": null,
    "count": {
      "matched": 0,
      "other_pattern": 0,
      "to_change": 0,
      "unreadable": 0,
      "with_ttl": 0
    },
    "error": false,
    "glob": "cart:*",
    "id": 5,
    "lines": [],
    "nodes": [],
    "pattern": {
      "copy": "cart:{id}",
      "id": "636172743a7b69647d",
      "name": "cart:{id}"
    },
    "prod": false,
    "profile": "local",
    "run": null,
    "stage": "counting",
    "summary": [],
    "title": "Set a TTL on every key of the pattern",
    "ttl": "2h",
    "ttl_secs": 7200
  }
}
```

Event `bulk`:

```json
{
  "bulk": {
    "action": "expire",
    "at": null,
    "count": {
      "matched": 1200,
      "other_pattern": 20,
      "to_change": 1150,
      "unreadable": 0,
      "with_ttl": 50
    },
    "error": false,
    "glob": "cart:*",
    "id": 5,
    "lines": [],
    "nodes": [],
    "pattern": {
      "copy": "cart:{id}",
      "id": "636172743a7b69647d",
      "name": "cart:{id}"
    },
    "prod": false,
    "profile": "local",
    "run": null,
    "stage": "counted",
    "summary": [
      "Dry run: 1,200 keys of the pattern, 50 with a TTL already.",
      "1,150 keys without a TTL get 2h; the others are left alone.",
      "20 other keys matched the glob but belong to other patterns: left alone."
    ],
    "title": "Set a TTL on every key of the pattern",
    "ttl": "2h",
    "ttl_secs": 7200
  }
}
```

Example, `POST /api/bulk/run {"bulk":5}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "878bc337aeddbf929ccc9d3e3ee3cd28",
    "kind": "ask",
    "text": "Set a TTL on every key of the pattern cart:{id} on local? Dry run: 1,200 keys of the pattern, 50 with a TTL already. 1,150 keys without a TTL get 2h; the others are left alone. 20 other keys matched the glob but belong to other patterns: left alone. PERSIST on the keys in the key file undoes it. Sends: EXPIRE <key> 7200 NX  for each key of SCAN MATCH cart:*, 500 at a time."
  }
}
```

Example, `POST /api/confirm/<id> {}`. Reply: HTTP 202

```json
{
  "bulk": {
    "action": "expire",
    "at": null,
    "count": {
      "matched": 1200,
      "other_pattern": 20,
      "to_change": 1150,
      "unreadable": 0,
      "with_ttl": 50
    },
    "error": false,
    "glob": "cart:*",
    "id": 5,
    "lines": [],
    "nodes": [],
    "pattern": {
      "copy": "cart:{id}",
      "id": "636172743a7b69647d",
      "name": "cart:{id}"
    },
    "prod": false,
    "profile": "local",
    "run": {
      "changed": 0,
      "failed": 0,
      "file": null,
      "moved": 0,
      "seen": 0,
      "sent": 0,
      "skipped": 0,
      "stopping": false,
      "unreadable": 0
    },
    "stage": "running",
    "summary": [
      "Dry run: 1,200 keys of the pattern, 50 with a TTL already.",
      "1,150 keys without a TTL get 2h; the others are left alone.",
      "20 other keys matched the glob but belong to other patterns: left alone."
    ],
    "title": "Set a TTL on every key of the pattern",
    "ttl": "2h",
    "ttl_secs": 7200
  }
}
```

Event `bulk`:

```json
{
  "bulk": {
    "action": "expire",
    "at": null,
    "count": {
      "matched": 1200,
      "other_pattern": 20,
      "to_change": 1150,
      "unreadable": 0,
      "with_ttl": 50
    },
    "error": false,
    "glob": "cart:*",
    "id": 5,
    "lines": [
      "Done: set a TTL of 2h on 1,150 keys of cart:{id}.",
      "50 keys left alone: a TTL already, or gone.",
      "Key names: <data dir>/bulk/local-249f1fb6f3a6/20261115T145622Z-expire.txt",
      "PERSIST on those keys undoes it."
    ],
    "nodes": [],
    "pattern": {
      "copy": "cart:{id}",
      "id": "636172743a7b69647d",
      "name": "cart:{id}"
    },
    "prod": false,
    "profile": "local",
    "run": {
      "changed": 1150,
      "failed": 0,
      "file": "<data dir>/bulk/local-249f1fb6f3a6/20261115T145622Z-expire.txt",
      "moved": 0,
      "seen": 1200,
      "sent": 1150,
      "skipped": 50,
      "stopping": false,
      "unreadable": 0
    },
    "stage": "done",
    "summary": [
      "Dry run: 1,200 keys of the pattern, 50 with a TTL already.",
      "1,150 keys without a TTL get 2h; the others are left alone.",
      "20 other keys matched the glob but belong to other patterns: left alone."
    ],
    "title": "Set a TTL on every key of the pattern",
    "ttl": "2h",
    "ttl_secs": 7200
  }
}
```

Event `bulk`:

```json
{
  "bulk": {
    "action": "expire",
    "at": null,
    "count": {
      "matched": 1200,
      "other_pattern": 20,
      "to_change": 0,
      "unreadable": 0,
      "with_ttl": 1200
    },
    "error": false,
    "glob": "cart:*",
    "id": 6,
    "lines": [
      "Every key of cart:{id} (1,200) has a TTL already. Nothing was sent."
    ],
    "nodes": [],
    "pattern": {
      "copy": "cart:{id}",
      "id": "636172743a7b69647d",
      "name": "cart:{id}"
    },
    "prod": false,
    "profile": "local",
    "run": null,
    "stage": "done",
    "summary": [
      "Dry run: 1,200 keys of the pattern, 1,200 with a TTL already.",
      "0 keys without a TTL get 24h; the others are left alone.",
      "20 other keys matched the glob but belong to other patterns: left alone."
    ],
    "title": "Set a TTL on every key of the pattern",
    "ttl": "24h",
    "ttl_secs": 86400
  }
}
```

Example, `POST /api/bulk/run {"bulk":6}`. Reply: HTTP 422

```json
{
  "error": "Nothing to run: the dry run found nothing to change."
}
```

A delete, its question cancelled:

Example, `POST /api/bulk/run {"bulk":7}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "95bdef0b8a9bf6c7dec65ed19b389217",
    "kind": "ask",
    "text": "Delete every key of the pattern cart:{id} on local? Dry run: 1,200 keys of the pattern will be deleted with UNLINK. 20 other keys matched the glob but belong to other patterns: left alone. This can't be undone. Sends: UNLINK <key>  for each key of SCAN MATCH cart:*, 500 at a time."
  }
}
```

Example, `DELETE /api/confirm/<id>`. Reply: HTTP 200

```json
{
  "cancelled": true,
  "text": "Cancelled: nothing was sent."
}
```

Event `bulk`:

```json
{
  "bulk": {
    "action": "delete",
    "at": null,
    "count": {
      "matched": 1200,
      "other_pattern": 20,
      "to_change": 1200,
      "unreadable": 0,
      "with_ttl": 1200
    },
    "error": false,
    "glob": "cart:*",
    "id": 7,
    "lines": [
      "Cancelled: nothing was sent."
    ],
    "nodes": [],
    "pattern": {
      "copy": "cart:{id}",
      "id": "636172743a7b69647d",
      "name": "cart:{id}"
    },
    "prod": false,
    "profile": "local",
    "run": null,
    "stage": "done",
    "summary": [
      "Dry run: 1,200 keys of the pattern will be deleted with UNLINK.",
      "20 other keys matched the glob but belong to other patterns: left alone."
    ],
    "title": "Delete every key of the pattern",
    "ttl": null,
    "ttl_secs": null
  }
}
```

A dry run stopped:

Example, `DELETE /api/bulk`. Reply: HTTP 200

```json
{
  "bulk": {
    "action": "delete",
    "at": null,
    "count": {
      "matched": 0,
      "other_pattern": 0,
      "to_change": 0,
      "unreadable": 0,
      "with_ttl": 0
    },
    "error": false,
    "glob": "cart:*",
    "id": 8,
    "lines": [
      "Dry run stopped. Nothing was sent."
    ],
    "nodes": [],
    "pattern": {
      "copy": "cart:{id}",
      "id": "636172743a7b69647d",
      "name": "cart:{id}"
    },
    "prod": false,
    "profile": "local",
    "run": null,
    "stage": "done",
    "summary": [],
    "title": "Delete every key of the pattern",
    "ttl": null,
    "ttl_secs": null
  },
  "text": "Dry run stopped. Nothing was sent."
}
```

A run stopped while its first batch was on its way (on a stage profile, whose command
budget spaces the batches): the reply says it stops after that batch, and the `bulk`
event after it says so:

Example, `DELETE /api/bulk`. Reply: HTTP 200

```json
{
  "bulk": {
    "action": "delete",
    "at": null,
    "count": {
      "matched": 4000,
      "other_pattern": 0,
      "to_change": 4000,
      "unreadable": 0,
      "with_ttl": 0
    },
    "error": false,
    "glob": "job:*",
    "id": 10,
    "lines": [],
    "nodes": [],
    "pattern": {
      "copy": "job:{id}",
      "id": "6a6f623a7b69647d",
      "name": "job:{id}"
    },
    "prod": false,
    "profile": "stage1",
    "run": {
      "changed": 0,
      "failed": 0,
      "file": null,
      "moved": 0,
      "seen": 0,
      "sent": 0,
      "skipped": 0,
      "stopping": true,
      "unreadable": 0
    },
    "stage": "running",
    "summary": [
      "Dry run: 4,000 keys of the pattern will be deleted with UNLINK."
    ],
    "title": "Delete every key of the pattern",
    "ttl": null,
    "ttl_secs": null
  },
  "text": "Stopping after the batch on its way."
}
```

Event `bulk`:

```json
{
  "bulk": {
    "action": "delete",
    "at": null,
    "count": {
      "matched": 4000,
      "other_pattern": 0,
      "to_change": 4000,
      "unreadable": 0,
      "with_ttl": 0
    },
    "error": false,
    "glob": "job:*",
    "id": 10,
    "lines": [
      "Stopped after the batch: deleted 500 keys of job:{id}.",
      "Key names: <data dir>/bulk/stage1-5948f06dc12d/20261115T145623Z-delete.txt"
    ],
    "nodes": [],
    "pattern": {
      "copy": "job:{id}",
      "id": "6a6f623a7b69647d",
      "name": "job:{id}"
    },
    "prod": false,
    "profile": "stage1",
    "run": {
      "changed": 500,
      "failed": 0,
      "file": "<data dir>/bulk/stage1-5948f06dc12d/20261115T145623Z-delete.txt",
      "moved": 0,
      "seen": 500,
      "sent": 500,
      "skipped": 0,
      "stopping": true,
      "unreadable": 0
    },
    "stage": "done",
    "summary": [
      "Dry run: 4,000 keys of the pattern will be deleted with UNLINK."
    ],
    "title": "Delete every key of the pattern",
    "ttl": null,
    "ttl_secs": null
  }
}
```

On prod the pattern is typed:

Example, `POST /api/bulk/run {"bulk":11}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": "job:{id}",
    "id": "6f238e3011d4fcecd98be68ff794e1c7",
    "kind": "type",
    "text": "Type job:{id} to start: Delete every key of the pattern job:{id} on prod1. Dry run: 3,500 keys of the pattern will be deleted with UNLINK. This can't be undone. Sends: UNLINK <key>  for each key of SCAN MATCH job:*, 500 at a time."
  }
}
```

Example, `POST /api/confirm/<id> {"typed":"prod1"}`. Reply: HTTP 422

```json
{
  "error": "Not sent: the name typed did not match."
}
```

On a cluster, each primary in turn:

Event `bulk`:

```json
{
  "bulk": {
    "action": "delete",
    "at": 2,
    "count": {
      "matched": 300,
      "other_pattern": 0,
      "to_change": 300,
      "unreadable": 0,
      "with_ttl": 0
    },
    "error": false,
    "glob": "item:*",
    "id": 1,
    "lines": [],
    "nodes": [
      {
        "endpoint": "127.0.0.1:52168",
        "matched": 98,
        "node": 0
      },
      {
        "endpoint": "127.0.0.1:52169",
        "matched": 107,
        "node": 1
      },
      {
        "endpoint": "127.0.0.1:52170",
        "matched": 95,
        "node": 2
      }
    ],
    "pattern": {
      "copy": "item:{id}",
      "id": "6974656d3a7b69647d",
      "name": "item:{id}"
    },
    "prod": false,
    "profile": "cl",
    "run": null,
    "stage": "counted",
    "summary": [
      "Dry run: 300 keys of the pattern will be deleted with UNLINK."
    ],
    "title": "Delete every key of the pattern",
    "ttl": null,
    "ttl_secs": null
  }
}
```

Example, `POST /api/bulk/run {"bulk":1}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "21800052ba1540477c2b954e5cca84a6",
    "kind": "ask",
    "text": "Delete every key of the pattern item:{id} on cl? Dry run: 300 keys of the pattern will be deleted with UNLINK. It runs on each of the 3 primaries in turn, in slot order. This can't be undone. Sends: UNLINK <key>  for each key of SCAN MATCH item:*, 500 at a time."
  }
}
```

Event `bulk`:

```json
{
  "bulk": {
    "action": "delete",
    "at": 2,
    "count": {
      "matched": 300,
      "other_pattern": 0,
      "to_change": 300,
      "unreadable": 0,
      "with_ttl": 0
    },
    "error": false,
    "glob": "item:*",
    "id": 1,
    "lines": [
      "Done: deleted 300 keys of item:{id}.",
      "Key names, each after its node and a tab: <data dir>/bulk/cl-08a25a07b54a/20261115T145619Z-delete.txt"
    ],
    "nodes": [
      {
        "endpoint": "127.0.0.1:52168",
        "matched": 98,
        "node": 0
      },
      {
        "endpoint": "127.0.0.1:52169",
        "matched": 107,
        "node": 1
      },
      {
        "endpoint": "127.0.0.1:52170",
        "matched": 95,
        "node": 2
      }
    ],
    "pattern": {
      "copy": "item:{id}",
      "id": "6974656d3a7b69647d",
      "name": "item:{id}"
    },
    "prod": false,
    "profile": "cl",
    "run": {
      "changed": 300,
      "failed": 0,
      "file": "<data dir>/bulk/cl-08a25a07b54a/20261115T145619Z-delete.txt",
      "moved": 0,
      "seen": 300,
      "sent": 300,
      "skipped": 0,
      "stopping": false,
      "unreadable": 0
    },
    "stage": "done",
    "summary": [
      "Dry run: 300 keys of the pattern will be deleted with UNLINK."
    ],
    "title": "Delete every key of the pattern",
    "ttl": null,
    "ttl_secs": null
  }
}
```

## Browse › Streams, Browse › Search and Hot keys from Monitor

Phase 4: the TUI's Streams and Search panels as Browse's other two panels (the page's
`#/browse/streams` and `#/browse/search`), and Hot keys ranked from a Monitor run when
the analysis has no LFU counters. Each route reads or writes only what the TUI's panel
does: the commands, their limits and their words are the TUI's (`src/tui/views/streams.rs`,
`src/tui/views/search.rs`, `src/tui/views/server.rs`), built by `src/live/`, never in
`src/web/`. Every write goes through the one write gate: `writes::decide`, then the
question (409 `{"confirm":…}`, answered with `POST /api/confirm/{id}`), then the grant and
`Hub::write`. A read-only profile is refused before anything is asked or sent
(422, the gate's words), and so is any of these writes while a bulk action counts or runs
(409 `{"error":"A bulk action is running: wait for it to end, or stop it first."}`, at the
answer too, whether it was asked before the bulk action started or not). A Search write is
for the node it was made on: another node picked before it is decided, or before its
question is answered, refuses it with 409
`{"error":"Not sent: the connection changed since the question was asked."}`, and nothing
is sent. A read whose reply comes back on another run (a database switch, an analysis) or,
for Search, another node than it was asked on keeps nothing and answers the same 409.

A feature that is off answers HTTP 200 `{"off": "<why>"}` in the TUI's words, as the
other panels do. Names (streams, groups, consumers, indexes, keys) come as
`{name, id, copy}` (see "Key and pattern names"), and a request names them by `id`.

There is no new event. A Monitor run's ranking follows the `monitor` event of kind
`closed` (see `POST /api/server/monitor`): the page reads
`GET /api/analysis/hot_keys/monitor` again then.

The examples are replies of the real server in `tests/web_p4_it.rs` (Redis 8.10 and
valkey-bundle 9.1 for Search; Redis 7.4, Valkey 8.1 and a three-shard cluster for
Streams and Hot keys; a profile named `dev`, and `ro` for a read-only one).


### `GET /api/streams[?refresh=1|now=1]`

The streams the analysis found (the stream keys among its sampled keys, at most 50), each
read with `XINFO STREAM`, `XINFO GROUPS` and `XINFO CONSUMERS` (and `MEMORY USAGE`) on the
node that owns it: on a cluster the keys are grouped by owner and each node is asked for
its own, at once. A list read under 2 s ago is answered again without asking the server,
so two tabs polling every 5 s (as the TUI polls) cost one read. `refresh=1` reads now and
asks again for what was refused before (the TUI's `r`). `now=1` reads now too but still
leaves out what was refused: the page's read after a write when no pending list of that
stream is shown, as the TUI's poll after a write (with one shown, the page reads only
`GET /api/streams/pending`, which reads the stream's row too). The page polls every 5 s
while the panel is shown and the entries are not open.

- 409 `{"error":"not connected: connect first, or the live connection is closed"}` without
  a ready live connection.
- 409 `{"error":"Not sent: the connection changed since the question was asked."}` when a
  new run started (a database switch, an analysis) while it was read: nothing of that read
  is kept.
- `{"off": …}` when the connect's permission check refused `XINFO`, or when the server
  refuses or does not know `XINFO STREAM`: `"The server refused XINFO for this user, so
  this view is off."`, or `"XINFO is not available: <why>."`.
- Before an analysis: `status: "no_report"`, the TUI's `text`, `analysis_running`, no rows.
  The page offers to analyze.
- None found: `status: "none"`, `"text": "The analysis found no streams."`, `"hint": "It
  samples keys, so it may miss some; a Deep analysis samples more."`.
- Else `status: "ready"`: `cluster`, `rows` in the analysis's order, `found` (how many the
  analysis found), `shown`, `note` (`"first 50 of 61 streams"`, or why the last read failed
  and the previous one is shown), `error`, and `writes`: why the writes are off for this
  profile (the gate's words), or `null`. The page leaves the write buttons out then.

A row: `key`, `node` and `endpoint` (on a cluster: the owner it was read on), `length`,
`bytes` (`null` when `MEMORY USAGE` is refused), `first_id`, `last_id`, `oldest_age_ms` and
`oldest` (the TUI's short age), `entries_added`, `group_count`, `lag` (the largest group
lag; `null` when unknown), `pending` (all groups), `verdict` (the first of `stuck`,
`lagging`, `no readers`, `unbounded`, else `ok`), `verdicts` (each with the TUI's reason),
`left_out` (what this user may not read), `groups` (`null` when `XINFO GROUPS` is refused,
with `groups_text`). A group: `name`, `consumers`, `pending`, `last_delivered_id`, `lag`,
`lagging` (1,000 or more), `readers` (`null` when `XINFO CONSUMERS` is refused): each
consumer's `name`, `pending`, `idle_ms`/`idle`, `inactive_ms`/`inactive` (Redis 7.2 and
later) and `stuck` (pending entries and idle 30 minutes or more); and `claim`, what the
TUI's claim form starts with from the group: the most recently active consumer, 60000 ms,
100 entries. The page opens the same form from a consumer's row and a pending entry's row,
as the TUI does: that consumer's idle time (in whole seconds), and the most recently active
of the group's other consumers to take them.
A stream gone since the analysis is `{key, node, endpoint, gone: true, text: "The key is
gone, or no longer a stream."}`.

Example, before an analysis, `GET /api/streams`. Reply: HTTP 200

```json
{
  "analysis_running": false,
  "rows": [],
  "status": "no_report",
  "text": "Streams are found by an analysis: it samples the keys and lists the streams among them."
}
```

Example, a read-only profile (`ro`), `GET /api/streams`. Reply: HTTP 200 (`$.rows` cut to 2 of 3)

```json
{
  "cluster": false,
  "error": null,
  "found": 3,
  "note": null,
  "rows": [
    {
      "bytes": 9704,
      "endpoint": null,
      "entries_added": 6,
      "first_id": "1794764710058-0",
      "gone": false,
      "group_count": 1,
      "groups": [
        {
          "claim": {
            "count": 100,
            "min_idle_ms": 60000,
            "to": {
              "copy": "w1",
              "id": "7731",
              "name": "w1"
            }
          },
          "consumers": 2,
          "lag": 2,
          "lagging": false,
          "last_delivered_id": "1794764710059-2",
          "name": {
            "copy": "billing",
            "id": "62696c6c696e67",
            "name": "billing"
          },
          "pending": 4,
          "readers": [
            {
              "idle": "0s",
              "idle_ms": 218,
              "inactive": "0s",
              "inactive_ms": 218,
              "name": {
                "copy": "w1",
                "id": "7731",
                "name": "w1"
              },
              "pending": 3,
              "stuck": false
            },
            {
              "idle": "0s",
              "idle_ms": 218,
              "inactive": "0s",
              "inactive_ms": 218,
              "name": {
                "copy": "w2",
                "id": "7732",
                "name": "w2"
              },
              "pending": 1,
              "stuck": false
            }
          ]
        }
      ],
      "groups_text": null,
      "key": {
        "copy": "p4s:orders",
        "id": "7034733a6f7264657273",
        "name": "p4s:orders"
      },
      "lag": 2,
      "last_id": "1794764710210-0",
      "left_out": null,
      "length": 6,
      "node": null,
      "oldest": "0s",
      "oldest_age_ms": 219,
      "pending": 4,
      "verdict": "ok",
      "verdicts": []
    },
    {
      "bytes": 8536,
      "endpoint": null,
      "entries_added": 250,
      "first_id": "1794764710061-0",
      "gone": false,
      "group_count": 0,
      "groups": [],
      "groups_text": "No consumer groups: nothing reads this stream.",
      "key": {
        "copy": "p4s:big",
        "id": "7034733a626967",
        "name": "p4s:big"
      },
      "lag": null,
      "last_id": "1794764710127-3",
      "left_out": null,
      "length": 250,
      "node": null,
      "oldest": "0s",
      "oldest_age_ms": 216,
      "pending": 0,
      "verdict": "no readers",
      "verdicts": [
        {
          "label": "no readers",
          "text": "No readers: no consumer group reads this stream."
        }
      ]
    }
  ],
  "shown": 3,
  "status": "ready",
  "writes": "Not sent: ro is read-only. Turn writes on in Connections."
}
```

Example, a cluster (Valkey), `GET /api/streams?refresh=1`. Reply: HTTP 200 (`$.rows` cut to 1 of 5)

```json
{
  "cluster": true,
  "error": null,
  "found": 5,
  "note": null,
  "rows": [
    {
      "bytes": 4642,
      "endpoint": "127.0.0.1:50806",
      "entries_added": 3,
      "first_id": "1794764705906-0",
      "gone": false,
      "group_count": 1,
      "groups": [
        {
          "claim": {
            "count": 100,
            "min_idle_ms": 60000,
            "to": {
              "copy": "c1",
              "id": "6331",
              "name": "c1"
            }
          },
          "consumers": 1,
          "lag": 1,
          "lagging": false,
          "last_delivered_id": "1794764705906-1",
          "name": {
            "copy": "g",
            "id": "67",
            "name": "g"
          },
          "pending": 2,
          "readers": [
            {
              "idle": "0s",
              "idle_ms": 86,
              "inactive": "0s",
              "inactive_ms": 86,
              "name": {
                "copy": "c1",
                "id": "6331",
                "name": "c1"
              },
              "pending": 2,
              "stuck": false
            }
          ]
        }
      ],
      "groups_text": null,
      "key": {
        "copy": "p4c:0",
        "id": "7034633a30",
        "name": "p4c:0"
      },
      "lag": 1,
      "last_id": "1794764705906-2",
      "left_out": null,
      "length": 3,
      "node": 1,
      "oldest": "0s",
      "oldest_age_ms": 87,
      "pending": 2,
      "verdict": "ok",
      "verdicts": []
    }
  ],
  "shown": 5,
  "status": "ready",
  "writes": null
}
```

Example, a user without `XINFO GROUPS`, `GET /api/streams`. Reply: HTTP 200 (`$.rows` cut to 1 of 3)

```json
{
  "cluster": false,
  "error": null,
  "found": 3,
  "note": null,
  "rows": [
    {
      "bytes": 9948,
      "endpoint": null,
      "entries_added": 5,
      "first_id": "1794764706306-0",
      "gone": false,
      "group_count": null,
      "groups": null,
      "groups_text": "XINFO GROUPS is not available for this user.",
      "key": {
        "copy": "p4s:orders",
        "id": "7034733a6f7264657273",
        "name": "p4s:orders"
      },
      "lag": null,
      "last_id": "1794764706307-2",
      "left_out": "Left out: the server refused XINFO GROUPS for this user.",
      "length": 5,
      "node": null,
      "oldest": "0s",
      "oldest_age_ms": 203,
      "pending": null,
      "verdict": "ok",
      "verdicts": []
    }
  ],
  "shown": 3,
  "status": "ready",
  "writes": null
}
```

Example, a user without `XINFO`, `GET /api/streams`. Reply: HTTP 200

```json
{
  "off": "The server refused XINFO for this user, so this view is off."
}
```


### `GET /api/streams/pending?key=&group=[&consumer=]`

A group's pending entries, `XPENDING <key> <group> - + 100 [<consumer>]` (the TUI's
`p`, at most 100), on the stream's owner, with the stream's row read again. `key`,
`group` and `consumer` are the ids of the list. The stream and the group must be in the
list (404 `"That stream is not in the list: read the streams again, or analyze again to
update it."`, `"That group is not one of the stream's: read the streams again."`).
The entries listed are the ones `POST /api/streams/ack` takes. A stream gone meanwhile is
`{"pending": null, "row": {…gone…}, "text": "The key is gone, or no longer a stream."}`;
`{"off": …}` as for the list; 409 `"Not sent: the connection changed since the question was
asked."` when a new run started while it was read (nothing is kept).

Example, `GET /api/streams/pending?key=7034733a6f7264657273&group=62696c6c696e67&consumer=7731`. Reply: HTTP 200 (`$.row` left out)

```json
{
  "pending": {
    "consumer": {
      "copy": "w1",
      "id": "7731",
      "name": "w1"
    },
    "entries": [
      {
        "consumer": {
          "copy": "w1",
          "id": "7731",
          "name": "w1"
        },
        "deliveries": 1,
        "id": "1794764710058-0",
        "idle": "0s",
        "idle_ms": 145
      },
      {
        "consumer": {
          "copy": "w1",
          "id": "7731",
          "name": "w1"
        },
        "deliveries": 1,
        "id": "1794764710059-0",
        "idle": "0s",
        "idle_ms": 145
      },
      {
        "consumer": {
          "copy": "w1",
          "id": "7731",
          "name": "w1"
        },
        "deliveries": 1,
        "id": "1794764710059-1",
        "idle": "0s",
        "idle_ms": 145
      }
    ],
    "error": null,
    "group": {
      "copy": "billing",
      "id": "62696c6c696e67",
      "name": "billing"
    },
    "key": {
      "copy": "p4s:orders",
      "id": "7034733a6f7264657273",
      "name": "p4s:orders"
    },
    "max": 100
  },
  "row": "…the row, as in the list…"
}
```


### `POST /api/streams/ack`, `POST /api/streams/claim`, `POST /api/streams/trim`

The TUI's three stream writes, and no other. Each is checked in the TUI's order: the
gate's refusal for a read-only profile (422), a bulk action running (409), the live
connection (409), the view off (422, its words), then the stream (and group) in the list
(404), then the form as the TUI's form checks it (422, its words). Then the gate: off
`prod` XACK and XAUTOCLAIM are sent at once and XTRIM (destructive) asks; on `prod` each
asks, and XTRIM has the stream's name typed. Each is sent to the stream's owner. The
reply is `{"sent": true, "reply": …}`: `{"kind":"int","value":n}` for XACK (1, or 0 when
the entry was not pending any more) and XTRIM (entries removed),
`{"kind":"claimed","ids":[…],"deleted":n}` for XAUTOCLAIM. The page says it in the TUI's
words (`Acknowledged <id>.`, `<id> was not pending any more.`, `Claimed 3 entries for w2.`
and `… 1 entry no longer in the stream left the pending list.`, `Trimmed 200 entries.`,
`Trimmed 0 entries: with ~ the server removes whole blocks only.`) and reads the list again.

- `ack` `{key, group, id}`: `XACK key group id`. `id` must be an entry ID (422 `"That is
  not a stream entry ID, such as 1700000000000-0."`) of the pending list last listed for
  that stream and group (422 `"That entry is not in the pending list shown: list the
  pending entries again."`).
- `claim` `{key, group, to, min_idle_ms, count}` as typed: `XAUTOCLAIM key group to
  min-idle 0-0 COUNT count`. `"Type the consumer that takes the entries."`, `"The min
  idle time is a whole number of milliseconds."`, `"The count is a whole number, 1 or
  more."`.
- `trim` `{key, by, value}`: `by` is `MAXLEN` or `MINID` (else 400 `"trim by MAXLEN or
  MINID"`); `XTRIM key MAXLEN ~ n` or `XTRIM key MINID ~ id`, approximate as the TUI's
  (`"Approximate (~): the server may keep a few more."`). `"MAXLEN takes how many entries to
  keep."`, `"MINID takes an entry ID, such as 1700000000000-0."`.

Example, `POST /api/streams/ack {"group":"62696c6c696e67","id":"1794764710058-0","key":"7034733a6f7264657273"}`. Reply: HTTP 200

```json
{
  "reply": {
    "kind": "int",
    "value": 1
  },
  "sent": true
}
```

Example, `POST /api/streams/ack {"group":"62696c6c696e67","id":"1-1","key":"7034733a6f7264657273"}`. Reply: HTTP 422

```json
{
  "error": "That entry is not in the pending list shown: list the pending entries again."
}
```

Example, `POST /api/streams/claim {"count":"100","group":"62696c6c696e67","key":"7034733a6f7264657273","min_idle_ms":"0","to":"w2"}`. Reply: HTTP 200

```json
{
  "reply": {
    "deleted": 0,
    "ids": [
      "1794764710059-0",
      "1794764710059-1",
      "1794764710059-2"
    ],
    "kind": "claimed"
  },
  "sent": true
}
```

Example, `POST /api/streams/trim {"by":"MAXLEN","key":"7034733a626967","value":"10"}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "fc4f0277580b9e927e40ee756d876365",
    "kind": "ask",
    "text": "Send XTRIM p4s:big MAXLEN ~ 10 to dev? This can't be undone."
  }
}
```

Example, its answer, `POST /api/confirm/fc4f0277580b9e927e40ee756d876365`. Reply: HTTP 200

```json
{
  "reply": {
    "kind": "int",
    "value": 200
  },
  "sent": true
}
```

Example, a read-only profile, `POST /api/streams/trim {"by":"MAXLEN","key":"7034733a626967","value":"10"}`. Reply: HTTP 422

```json
{
  "error": "Not sent: ro is read-only. Turn writes on in Connections."
}
```


### `GET /api/search[?refresh=1]`

The indexes of the picked node (on a cluster: `POST /api/node`), from `FT._LIST`, each
with `FT.INFO` (the TUI's limit: at most 50 indexes). Read on each request, as the TUI reads
the list when its view is shown: the page asks when the panel is shown and after each of
its writes, and does not poll. A module found missing, or `FT._LIST` refused, is not asked
again until `refresh=1`, which reads now and asks again for what was refused (the TUI's
`r`). A reply that comes back on another run or another node than it was asked on keeps
nothing and answers 409 `"Not sent: the connection changed since the question was asked."`.

- `{"off": …, "no_module": true, "detail": …}` without the module: the permission check
  skipped `FT._LIST`, or the server does not know it. `{"off": …, "no_module": false,
  "detail": null}` when it is refused for this user (`"The server refused FT._LIST for this
  user, so this view is off."`).
- Else `module` (`Redis Search` or `valkey-search`), `node` and `node_title` on a cluster,
  `indexes`, `total`, `first_of` (`"first 50 of 140 indexes"`), `notes`, `empty`
  (`{title, text}` when there is none), `error` (the last read failed; the previous one is
  shown), `cmd_off` (why FT.SEARCH, FT.EXPLAIN or FT.PROFILE is off: refused or unknown,
  found on use), `busy` (the command running, named as in its messages: `"FT.SEARCH"`,
  `"FT.EXPLAIN"` or `"FT.PROFILE"`), `last_query` (and `last_query_copy`, `null` when
  escaping changed it; the page's query box starts from it), `results` (the last results' first page) and `writes` (why
  writes are off, or `null`).

An index: `name`, `info` (`false` when `FT.INFO` was refused: only the name is known, with
`info_text`), `on` (`HASH` or `JSON`), `prefixes`, `docs`, `bytes` (`null` when the
module does not report it), `indexing`, `progress` and `progress_text`, `building`,
`failures`, `left_out`, `schema` (`name`, `path` when it is not the name, `type`, `flags`)
and `fields_text`.

Example, no module (Redis 7.4), `GET /api/search`. Reply: HTTP 200

```json
{
  "detail": "FT._LIST is unknown, so there is no Redis Search (built into Redis 8) or valkey-search to list indexes from.",
  "no_module": true,
  "off": "FT._LIST is not available: the Search module is not loaded."
}
```

Example, Redis 8, no index yet, `GET /api/search`. Reply: HTTP 200

```json
{
  "busy": null,
  "cmd_off": {
    "explain": null,
    "profile": null,
    "search": null
  },
  "empty": {
    "text": "Redis Search is loaded; nothing was created with FT.CREATE yet.",
    "title": "The module has no indexes."
  },
  "error": null,
  "first_of": null,
  "indexes": [],
  "last_query": null,
  "last_query_copy": null,
  "module": "Redis Search",
  "node": null,
  "node_title": null,
  "notes": [],
  "results": null,
  "total": 0,
  "writes": null
}
```

Example, valkey-search, after FT.EXPLAIN and FT.PROFILE were found missing, `GET /api/search`. Reply: HTTP 200 (`$.results.docs` cut to 1)

```json
{
  "busy": null,
  "cmd_off": {
    "explain": "The server does not support FT.EXPLAIN.",
    "profile": "The server does not support FT.PROFILE.",
    "search": null
  },
  "empty": null,
  "error": null,
  "first_of": null,
  "indexes": [
    {
      "building": false,
      "bytes": null,
      "docs": 3,
      "failures": 0,
      "fields_text": null,
      "indexing": false,
      "info": true,
      "info_text": null,
      "left_out": "left out: size (valkey-search does not report it)",
      "name": {
        "copy": "p4idx",
        "id": "7034696478",
        "name": "p4idx"
      },
      "on": "HASH",
      "prefixes": [
        "p4p:"
      ],
      "progress": 1.0,
      "progress_text": "100%",
      "schema": [
        {
          "flags": [],
          "name": "price",
          "path": null,
          "type": "NUMERIC"
        },
        {
          "flags": [],
          "name": "title",
          "path": null,
          "type": "TEXT"
        }
      ]
    }
  ],
  "last_query": "@title:red",
  "last_query_copy": "@title:red",
  "module": "valkey-search",
  "node": null,
  "node_title": null,
  "notes": [],
  "results": {
    "cmd": "search",
    "count": "2 matches",
    "docs": [
      {
        "field_count": 2,
        "fields": [
          {
            "name": "price",
            "value": "20"
          },
          {
            "name": "title",
            "value": "red shoe"
          }
        ],
        "index": 0,
        "key": {
          "copy": "p4p:2",
          "id": "7034703a32",
          "name": "p4p:2"
        }
      }
    ],
    "empty": null,
    "from": 0,
    "index": {
      "copy": "p4idx",
      "id": "7034696478",
      "name": "p4idx"
    },
    "kept": 2,
    "max": 50,
    "page": 10,
    "query": "@title:red",
    "run": 3,
    "took": "<1 ms",
    "total": 2
  },
  "total": 1,
  "writes": null
}
```


### `POST /api/search/query`

Body `{index, cmd, query?}`: `index` is an index's id from the list (404 `"That index is
not in the list: read the indexes again."`), `cmd` is `search`, `explain` or `profile`
(else 400). As the TUI sends them, on the picked node:

- `search`: `FT.SEARCH <index> <query> LIMIT 0 50`, the query as typed (a newline is a
  space, other control characters are dropped, empty is `*`; more than 1,000 characters is
  422 `"A query is at most 1,000 characters."`). It becomes the last query.
- `explain`, `profile`: `FT.EXPLAIN <index> <last query>`, `FT.PROFILE <index> SEARCH
  QUERY <last query> LIMIT 0 50`. They take no `query` (422 `"FT.EXPLAIN and FT.PROFILE
  use the query last run: send no query."`) and need one run first (422 `"Run a query
  first: FT.EXPLAIN uses the query last run."`).

One at a time: 409 `"FT.SEARCH is still running: wait for its answer."`. No
`FT.AGGREGATE` (the TUI has none). The reply is `{cmd, results, text}`: `results` the first
page (as `GET /api/search/results`) for `search` and `profile`, `text` (`{lines, empty}`)
for `explain` and `profile`. A command the server refuses or does not know answers
`{"off": …}` and stays off (in `cmd_off`) until `refresh=1`. A reply that comes back after
another node was picked or a new run started keeps nothing (no results, no command off)
and answers 409 `"Not sent: the connection changed since the question was asked."`; the
query slot is freed only by the query that took it. Any other failure is 422 with
the module's message about the query (`"FT.SEARCH failed: Syntax error at offset 7 near
price."`), or `"FT.SEARCH failed: the server could not run the query."` when that message
would not be shown.

Example, `POST /api/search/query {"cmd":"search","index":"7034696478","query":"@price:[0 100]"}`. Reply: HTTP 200 (`$.results.docs` cut to 2 of 3)

```json
{
  "cmd": "search",
  "results": {
    "cmd": "search",
    "count": "3 matches",
    "docs": [
      {
        "field_count": 2,
        "fields": [
          {
            "name": "title",
            "value": "red shoe"
          },
          {
            "name": "price",
            "value": "10"
          }
        ],
        "index": 0,
        "key": {
          "copy": "p4p:1",
          "id": "7034703a31",
          "name": "p4p:1"
        }
      },
      {
        "field_count": 2,
        "fields": [
          {
            "name": "title",
            "value": "red shoe"
          },
          {
            "name": "price",
            "value": "20"
          }
        ],
        "index": 1,
        "key": {
          "copy": "p4p:2",
          "id": "7034703a32",
          "name": "p4p:2"
        }
      }
    ],
    "empty": null,
    "from": 0,
    "index": {
      "copy": "p4idx",
      "id": "7034696478",
      "name": "p4idx"
    },
    "kept": 3,
    "max": 50,
    "page": 10,
    "query": "@price:[0 100]",
    "run": 1,
    "took": "1 ms",
    "total": 3
  },
  "text": null
}
```

Example, `POST /api/search/query {"cmd":"explain","index":"7034696478"}`. Reply: HTTP 200

```json
{
  "cmd": "explain",
  "index": {
    "copy": "p4idx",
    "id": "7034696478",
    "name": "p4idx"
  },
  "query": "@title:red",
  "results": null,
  "text": {
    "empty": null,
    "lines": [
      "@title:UNION {",
      "  @title:red",
      "  @title:+red(expanded)",
      "}"
    ]
  }
}
```

Example, `POST /api/search/query {"cmd":"profile","index":"7034696478"}`. Reply: HTTP 200 (`$.results.docs` cut to 1, `$.text.lines` to 6)

```json
{
  "cmd": "profile",
  "index": {
    "copy": "p4idx",
    "id": "7034696478",
    "name": "p4idx"
  },
  "query": "@title:red",
  "results": {
    "cmd": "profile",
    "count": "2 matches",
    "docs": [
      {
        "field_count": 2,
        "fields": [
          {
            "name": "title",
            "value": "red shoe"
          },
          {
            "name": "price",
            "value": "10"
          }
        ],
        "index": 0,
        "key": {
          "copy": "p4p:1",
          "id": "7034703a31",
          "name": "p4p:1"
        }
      }
    ],
    "empty": null,
    "from": 0,
    "index": {
      "copy": "p4idx",
      "id": "7034696478",
      "name": "p4idx"
    },
    "kept": 2,
    "max": 50,
    "page": 10,
    "query": "@title:red",
    "run": 4,
    "took": "1 ms",
    "total": 2
  },
  "text": {
    "empty": null,
    "lines": [
      "Shards",
      "  - Total profile time: 0.739417",
      "    Parsing time: 0.095084",
      "    Workers queue time: 0.573125",
      "    Pipeline creation time: 0.037583",
      "    Total GIL time: 0.031877"
    ]
  }
}
```

Example, valkey-search, `POST /api/search/query {"cmd":"explain","index":"7034696478"}`. Reply: HTTP 200

```json
{
  "off": "The server does not support FT.EXPLAIN."
}
```

Example, `POST /api/search/query {"cmd":"search","index":"7034696478","query":"@price:["}`. Reply: HTTP 422

```json
{
  "error": "FT.SEARCH failed: Syntax error at offset 7 near price."
}
```


### `GET /api/search/results?run=&from=`, `GET /api/search/field?run=&doc=&field=[&format=&width=]`

A page of 10 of the last results (`run` is `results.run`), from `from`; and one field of
one result whole, drawn in a format as a value's item is (`GET /api/value/item`). Results
of an earlier run are 409 `"Those results are not the last query's: run it again."`; a
`doc` or `field` that is not there is 404 `"There is no such result, or no such field in
it."`. A result: `index`, `key`, `field_count`, `fields` (`name`, and `value`, a preview).
`total` is what the module counted, `kept` how many it returned (at most `max`, 50).

Example, `GET /api/search/results?run=1&from=0`. Reply: HTTP 200 (`$.docs` cut to 1 of 3)

```json
{
  "cmd": "search",
  "count": "3 matches",
  "docs": [
    {
      "field_count": 2,
      "fields": [
        {
          "name": "title",
          "value": "red shoe"
        },
        {
          "name": "price",
          "value": "10"
        }
      ],
      "index": 0,
      "key": {
        "copy": "p4p:1",
        "id": "7034703a31",
        "name": "p4p:1"
      }
    }
  ],
  "empty": null,
  "from": 0,
  "index": {
    "copy": "p4idx",
    "id": "7034696478",
    "name": "p4idx"
  },
  "kept": 3,
  "max": 50,
  "page": 10,
  "query": "@price:[0 100]",
  "run": 1,
  "took": "1 ms",
  "total": 3
}
```

Example, `GET /api/search/field?run=1&doc=2&field=0`. Reply: HTTP 200

```json
{
  "doc": 2,
  "field": 0,
  "key": {
    "copy": "p4p:3",
    "id": "7034703a33",
    "name": "p4p:3"
  },
  "name": "title",
  "run": 1,
  "shown": {
    "bytes": 8,
    "compression": null,
    "copy": "blue hat",
    "copy_refused": null,
    "detected": "text",
    "format": "text",
    "formats": [
      "text",
      "hex",
      "JSON",
      "MessagePack",
      "PHP",
      "Pickle",
      "Protobuf",
      "Java"
    ],
    "header": "value · text · 8 bytes · auto-detected",
    "lines": [
      "blue hat"
    ],
    "note": null,
    "picked": null,
    "truncated": false
  }
}
```


### `POST /api/search/create`, `POST /api/search/drop`

The TUI's `C` and `D`, through the write gate, on the picked node: the node whose list
the index was checked against. Another node picked before the write is decided, or before
its question is answered, refuses it with 409 `"Not sent: the connection changed since the
question was asked."`, and nothing is sent to either node.

- `create` `{name, on, prefixes, fields}` as typed: `on` is `HASH` or `JSON` (else 400);
  `prefixes` separated by spaces or commas (empty: every key); `fields` as `name TYPE, …`,
  on JSON `$.path AS alias TYPE`. Checked as the TUI's form checks it (`"Type the index's
  name."`, `"An index name has no spaces."`, `"A field's type is TEXT, TAG, NUMERIC or GEO,
  not WORDS."`, `"Add at least one field, such as title TEXT."`, …), then `FT.CREATE`. An
  edit: sent at once off `prod`.
- `drop` `{index}`: an index of the list, `FT.DROPINDEX <index>` without `DD`: its
  documents stay. Destructive: asked, and on `prod` the index's name is typed.

Without the module both are 422 `"Not available: the server has no Search module, so
indexes can't be created or dropped."`. The reply is `{"sent": true, "reply":
{"kind":"ok"}}`; the page says `Created index <name>.` or `Dropped index <name>. Its
documents stay.` and reads the list again.

Example, `POST /api/search/create {"fields":"title TEXT, price NUMERIC","name":"p4idx","on":"HASH","prefixes":"p4p:"}`. Reply: HTTP 200

```json
{
  "reply": {
    "kind": "ok"
  },
  "sent": true
}
```

Example, `POST /api/search/create {"fields":"title TEXT","name":"p4 idx","on":"HASH","prefixes":""}`. Reply: HTTP 422

```json
{
  "error": "An index name has no spaces."
}
```

Example, `POST /api/search/drop {"index":"7034696478"}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "bee3c691082b85bdea082b47c2d51ed0",
    "kind": "ask",
    "text": "Send FT.DROPINDEX p4idx to dev? This can't be undone."
  }
}
```

Example, its answer, `POST /api/confirm/bee3c691082b85bdea082b47c2d51ed0`. Reply: HTTP 200

```json
{
  "reply": {
    "kind": "ok"
  },
  "sent": true
}
```

Example, no module, `POST /api/search/create {"fields":"title TEXT","name":"p4idx","on":"HASH","prefixes":"p4p:"}`. Reply: HTTP 422

```json
{
  "error": "Not available: the server has no Search module, so indexes can't be created or dropped."
}
```


### `GET /api/analysis/hot_keys/monitor`

Hot keys ranked from the last Monitor run (`POST /api/server/monitor`), for when the
analysis has no hot keys (no LFU policy, or `OBJECT FREQ` refused), as the TUI's Hot keys
shows it. A run that ends keeps its lines, and their keys are found from each command's
key specs: `COMMAND INFO` for the command names the session does not know yet (shared
with Server › Slow log, at most 8 requests, on the node Monitor ran on, through the
command budget; never `COMMAND GETKEYS`, so no logged argument goes back to the server).
They are ranked once that is done. With `COMMAND INFO` refused, the lines of commands
whose specs are unknown count in `unknown`, and `keys_note` says why (`"Key names are off:
<why>."`). 409 `{"error":"No analysis yet: analyze the keys first."}` before an analysis.

`status` is `lfu` (the analysis has hot keys; the panel ranks by them), `none` (no run has
ended in this session; `text` says where to start one), `pending` (the keys are being
placed) or `ready`, with `ranking`: `title` (the TUI's line: run length, lines, when),
`secs`, `lines`, `at_unix`, `unknown` and `unknown_text` (lines whose keys could not be
found), `keys_note` (why the keys were not looked up, when they were not), `node`,
`endpoint`, `rows` (`key`, `count`, `share` of the lines, `named`, `pattern`) and `empty`.

Example, `GET /api/analysis/hot_keys/monitor`. Reply: HTTP 200

```json
{
  "ranking": null,
  "status": "none",
  "text": "Or rank keys by how often Monitor sees them: Server › Monitor."
}
```

Example, just after the run stopped, `GET /api/analysis/hot_keys/monitor`. Reply: HTTP 200

```json
{
  "ranking": null,
  "status": "pending",
  "text": "Ranking the keys of the last Monitor run…"
}
```

Example, `GET /api/analysis/hot_keys/monitor`. Reply: HTTP 200

```json
{
  "ranking": {
    "at_unix": 1794764707,
    "empty": null,
    "endpoint": null,
    "keys_note": null,
    "lines": 10,
    "node": 0,
    "rows": [
      {
        "count": 6,
        "key": {
          "copy": "p4h:a",
          "id": "7034683a61",
          "name": "p4h:a"
        },
        "named": "p4h:a · named 6 times in 10 lines",
        "pattern": {
          "copy": "p4h:a",
          "id": "7034683a61",
          "name": "p4h:a"
        },
        "share": 0.6
      },
      {
        "count": 4,
        "key": {
          "copy": "p4h:b",
          "id": "7034683a62",
          "name": "p4h:b"
        },
        "named": "p4h:b · named 4 times in 10 lines",
        "pattern": {
          "copy": "p4h:b",
          "id": "7034683a62",
          "name": "p4h:b"
        },
        "share": 0.4
      },
      {
        "count": 1,
        "key": {
          "copy": "p4h:c",
          "id": "7034683a63",
          "name": "p4h:c"
        },
        "named": "p4h:c · named once in 10 lines",
        "pattern": {
          "copy": "p4h:c",
          "id": "7034683a63",
          "name": "p4h:c"
        },
        "share": 0.1
      }
    ],
    "secs": 0,
    "title": "from Monitor, 0 s, 10 lines, at 17:45 UTC",
    "unknown": 0,
    "unknown_text": null
  },
  "status": "ready",
  "text": "Ranked by how often the last Monitor run named each key, because the analysis has no LFU counters: a short sample, not the server's own counters."
}
```
