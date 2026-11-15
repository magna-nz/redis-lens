# Dashboard API

The JSON API and event stream the web dashboard's page talks to (`src/web/`). The page in
`src/web/assets/` is the only client; this file is the contract between the two.

Every example reply below was captured from a real run: a test that started the dashboard
with `redis_lens::web::start_with` against a Redis 7 container, a temporary config file and
an in-memory credential store (`MemoryStore`). Ports and times are from that run. Long lists
are cut, and each place that is cut says so.

## Listening and signing in

- The dashboard binds `127.0.0.1:<port>` only (default 7379; port 0 picks a free one).
  A port in use is `StartError::PortInUse`.
- Each run makes a session token: 32 random bytes from the OS, written as 64 hex digits.
  It is only in the URL that `Dashboard::open_url` returns. `Debug`, logs and every reply
  leave it out.
- `GET /auth/<token>` answers `303 See Other` with `Location: /` and
  `Set-Cookie: rl_session=<token>; HttpOnly; SameSite=Strict; Path=/`. A wrong token gets
  401 `{"error":"not signed in"}`.
- `GET /` serves `index.html` to a signed-in browser and `locked.html` to anyone else.
  `GET /assets/<file>` serves the embedded files on a fixed list (404 for anything else).
  `.js` is `text/javascript; charset=utf-8`. Debug builds only: `REDIS_LENS_DASHBOARD_ASSETS=<dir>`
  serves the same names from that directory, so the page can be changed without a rebuild.

## Checks on every request

They run in this order, before any route:

1. `Host` must be `127.0.0.1:<port>` or `localhost:<port>`. Anything else gets 403
   `{"error":"forbidden"}`. This stops DNS rebinding.
2. Any method other than `GET` and `HEAD` must have `Origin: http://127.0.0.1:<port>` (or
   `http://localhost:<port>`), the header `X-Redis-Lens: 1` and
   `Content-Type: application/json`. Anything else gets 403 `{"error":"forbidden"}`.
3. Every `/api/...` route needs the `rl_session` cookie, compared in constant time. Without
   it the reply is 401 `{"error":"not signed in"}`, with no detail.

Every response, errors and assets included, carries:

```
Cache-Control: no-store
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
X-Frame-Options: DENY
Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'
```

The server logs nothing.

Example, `GET /api/state (no cookie)`. Reply: HTTP 401

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
- Errors are `{"error": "<text>"}`.
- Status codes:
  - 200: done.
  - 201: a connection was added.
  - 202: started, with the result arriving as events (connect, subscribe, Monitor).
  - 404: no such connection or route.
  - 409: there is no live connection (`{"error":"not connected: connect first, or the live connection is closed"}`),
    or a confirm is needed (see below).
  - 410: a confirm id that was used, cancelled or expired.
  - 422: refused by the write gate or by a rule (read-only profile, no such node, can't run here).
  - 502: the server failed the request, or did not answer within 60 s.
- When the server or the user lacks a feature (`NOPERM`, a command Valkey or an old Redis
  does not have), the panel's route answers 200 `{"off": "<why>"}` instead of failing.
  This follows the rule "degrade instead of failing".
- Every text that came from the server (keys, client names, channel names, reply values,
  command lines) is escaped with `value::escape` before it goes in a reply.
- Connection URLs are always masked: the user name and password are never in a reply.
  `auth` says which kind of credential the profile uses.
- Command lines from the server (slow log, Monitor) and the user's own lines are masked by
  the TUI's own masking code (`live::command::masked_line`): secret arguments, such as
  those of `AUTH` and `CONFIG SET masterauth`, are shown as `***`.

## Confirms (write questions)

A write that the gate (spec "Write permissions") would ask about in the TUI gets 409 instead:

```json
{"confirm": {"id": "<32 hex>", "kind": "ask" | "type", "text": "<question>", "expect": "<name to type>" | null}}
```

- `kind: "ask"` is a yes/no question. `kind: "type"` needs the profile's name typed (for
  prod profiles), and `expect` holds that name.
- `POST /api/confirm/<id>` with `{}` (ask) or `{"typed": "<name>"}` (type) answers yes. The
  gate checks again at that point: a different connection, a profile that is now
  read-only, or a wrong typed name means it is not sent.
- `DELETE /api/confirm/<id>` cancels.
- Each id can be used once and expires after 2 minutes. After that both routes answer 410.
  Connecting or disconnecting clears every open confirm.

Example, `PATCH /api/connections/prod-cache {"writes":true}`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": "prod-cache",
    "id": "06edda37fc81534abda0233773a0cce6",
    "kind": "type",
    "text": "prod-cache is a prod profile. Type its name to turn writes on: "
  }
}
```

Example, `POST /api/confirm/<id> {"typed":"prod-cache"}`. Reply: HTTP 200

```json
{
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:61596",
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
    "url": "redis://127.0.0.1:61596/0",
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
The page opens one stream per tab.

| Event | When | Data |
|---|---|---|
| `state` | first on every stream, then on every change of connection, phase, node, tail or Monitor | the same body as `GET /api/state` |
| `figures` | about every second, but only while at least one stream is open and the live connection is ready | `{node, info, rates, shards}` for the picked node, where `shards` is every node's headline on a cluster (refreshed less often) |
| `tail` | pub/sub tail | `{tail, kind: "ready"}`, `{tail, kind: "messages", messages: [...], dropped}`, `{tail, kind: "closed", error}` |
| `monitor` | Monitor run | `{run, kind: "ready", at_unix_ms, max_lines, max_secs}`, `{run, kind: "lines", lines: [...]}`, `{run, kind: "closed", end: "stopped"\|"line_cap"\|"time_up"\|"lost"\|"failed", error}` |
| `notice` | something the user should know that has no panel, such as the live connection closing or a stream that fell behind | `{"text": "..."}` |

A stream that falls more than 1024 events behind skips them and gets a `notice` instead.

Example, `event: state (first on the stream, not connected)`. Event

```json
{
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
  "nodes": [],
  "picked": 0,
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
    "fragmentation": 9.8,
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
    "used_memory": 1543912
  },
  "node": 0,
  "rates": {
    "blocked": 0,
    "clients": 2,
    "cpu_sys": 0.18385597476726265,
    "cpu_user": 0.21549676955619976,
    "evicted": 0.0,
    "expired": 0.0,
    "fragmentation": 9.8,
    "hit_rate": null,
    "net_in": 13.973852588174143,
    "net_out": 5577.563447336936,
    "ops": 0.9981323277267246,
    "used_memory": 1543912
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
| GET | `/api/overview` | figures, permissions, shards, the last analysis |
| GET | `/api/server/clients` | `CLIENT LIST`, grouped |
| GET | `/api/server/slowlog` | `SLOWLOG GET`, masked, plus the slow-log settings |
| POST, DELETE | `/api/server/monitor` | start, stop a Monitor run |
| GET | `/api/pubsub/channels` | active channels and their subscriber counts |
| POST, DELETE | `/api/pubsub/subscribe` | start, stop the live tail |
| POST | `/api/pubsub/publish` | `PUBLISH`/`SPUBLISH` through the write gate |
| POST | `/api/cmdline` | run a typed command line under the TUI's rules |
| GET | `/api/cmdline/commands` | command names for completion |
| GET | `/api/cmdline/help?name=` | a command's help |
| POST, DELETE | `/api/confirm/{id}` | answer or cancel a question |

### `GET /api/state`

`status` is `idle`, `connecting`, `connected` or `failed` (`error` says why). `live` is
`waiting`, `ready` or `closed` (`live_error`). `writes` is true when this connection may
write, and `writes_off` says why not. `refused` lists the commands the permission check
turned off. `nodes` and `picked` describe a cluster (the first primary is picked when the
nodes arrive). `features.entra_id` says whether this build has Entra ID.

Example, `GET /api/state`. Reply: HTTP 200

```json
{
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
  "nodes": [],
  "picked": 0,
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

Example, `GET /api/state (after connecting)`. Event

```json
{
  "connected_at_unix": 1794724960,
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:61596",
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
    "url": "redis://127.0.0.1:61596/0",
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
  "nodes": [],
  "picked": 0,
  "refused": [],
  "sentinel": null,
  "server": {
    "flavor": "redis",
    "keys": 1,
    "lfu": false,
    "maxmemory": 0,
    "mode": "standalone",
    "policy": "noeviction",
    "used_memory": 1469704,
    "version": "7.4.12"
  },
  "status": "connected",
  "tail": null,
  "target": "primary",
  "writes": true,
  "writes_off": null
}
```

### `GET /api/connections`

`connections` holds the saved profiles, and `connected` is the name connected to (or null).
There are no credential fields: `auth` is `none`, `keychain` (a user name or password is saved there), `command`
(a token command) or `entra-id`, and `url` is masked.

Example, `GET /api/connections`. Reply: HTTP 200

```json
{
  "connected": null,
  "connections": [
    {
      "auth": "none",
      "db": 0,
      "endpoint": "127.0.0.1:61596",
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
      "url": "redis://127.0.0.1:61596/0",
      "writes": true
    },
    {
      "auth": "none",
      "db": 0,
      "endpoint": "127.0.0.1:61596",
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
      "url": "redis://127.0.0.1:61596/0",
      "writes": false
    }
  ]
}
```

### `POST /api/connections`

This adds a profile, as `redis-lens conn add` does: the same checks, and credentials go only
to the OS keychain. Body (only `name` and `url` are required; unknown fields get 400):

```json
{"name": "staging", "url": "redis://app-user@10.0.0.5:6379/0", "password": "...",
 "env": "local|dev|stage|prod", "tags": ["team-b"], "writes": true,
 "provider": "elasticache|memorystore|azure|upstash|redis-cloud|aiven",
 "ssh": "user@bastion:22", "ssh_identity": "~/.ssh/id_ed25519", "ssh_known_hosts": "...",
 "sentinel": ["10.0.0.1:26379"], "sentinel_master": "mymaster", "sentinel_tls": false,
 "sentinel_user": "...", "sentinel_password": "...",
 "token_command": ["prog", "arg"], "entra": false, "standalone": false,
 "prefer_replica": false, "force": false}
```

`password` and `sentinel_password` go straight to the keychain. They, and any user name or
password in `url`, are never in a reply or in the config file. A `token_command` is asked
about first (409 `ask`, showing the command). Writes on a `prod` profile need the name typed
(409 `type`). A failure is 422 with the CLI's error text, which has credentials scrubbed.
The reply is 201 `{message, connection}`.

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

Example, `POST /api/connections (token command: asks first)`. Reply: HTTP 409

```json
{
  "confirm": {
    "expect": null,
    "id": "8ab6baa295ca9af125e404b72972fe65",
    "kind": "ask",
    "text": "This profile will run this command at every connect and send what it prints as the password: get-token --scope redis. It is run directly, not through a shell. Save it?"
  }
}
```

### `PATCH /api/connections/{name}`

The body is `{"writes": bool, "tag": [..], "untag": [..], "standalone": bool}` (each part
optional), as in `conn set`. Turning writes on for a prod profile asks for the name to be
typed. When the profile is the one connected to, the session's read-only flag and tags
change at once and a `state` event follows.

Example, `PATCH /api/connections/prod-cache {"writes":false}`. Reply: HTTP 200

```json
{
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:61596",
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
    "url": "redis://127.0.0.1:61596/0",
    "writes": false
  },
  "message": "Writes are off for prod-cache: it is read-only."
}
```

### `DELETE /api/connections/{name}`

This is `conn remove`: it removes the profile and its keychain entries. An unknown name is 404.

Example, `DELETE /api/connections/staging`. Reply: HTTP 200

```json
{
  "message": "Removed staging."
}
```

### `POST /api/connections/{name}/test`

This is `conn test`: it connects once, as the TUI does (keychain entries are read on a blocking thread), and reports the server and the permission check. A failed connect, or a keychain read that fails, is 502 with the masked error. An unknown name is 404.

Example, `POST /api/connections/cache/test`. Reply: HTTP 200 (`$.checks` cut to the first 3 of 6)

```json
{
  "at": "127.0.0.1:61596",
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
    "endpoint": "127.0.0.1:61596",
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
    "url": "redis://127.0.0.1:61596/0",
    "writes": true
  },
  "sentinel": null
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
      "container": "objective_borg",
      "endpoint": "127.0.0.1:61596",
      "image": "redis:7.4-alpine",
      "url": "redis://127.0.0.1:61596/0"
    }
  ]
}
```

### `POST /api/connect`, `POST /api/disconnect`

`{"name": "<profile>"}` or `{"docker": "<container>"}`. The connection is made exactly as the
TUI makes it (same code). It replaces the current connection, its tail, its Monitor and its
open confirms. The reply is 202 with the state; progress arrives as `state` events, and
`status` turns to `connected` or `failed`. All tabs share the one connection.

Example, `POST /api/connect {"name":"cache"}`. Reply: HTTP 202

```json
{
  "connected_at_unix": null,
  "connection": {
    "auth": "none",
    "db": 0,
    "endpoint": "127.0.0.1:61596",
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
    "url": "redis://127.0.0.1:61596/0",
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
  "nodes": [],
  "picked": 0,
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

Example, `POST /api/disconnect`. Reply: HTTP 200

```json
{
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
  "nodes": [],
  "picked": 0,
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
there is one. `server` and `target` describe what was connected to. Live rates come as
`figures` events.

Example, `GET /api/overview`. Reply: HTTP 200 (`$.perms` cut to the first 3 of 41)

```json
{
  "figures": {
    "big_keys": null,
    "dataset_bytes": 512992,
    "gaps": [
      "This server has no INFO keysizes (Redis 8 and later have it), so there is no exact size histogram"
    ],
    "keys": 1,
    "overhead_bytes": 981984,
    "server": {
      "flavor": "redis",
      "keys": 1,
      "lfu": false,
      "maxmemory": 0,
      "mode": "standalone",
      "policy": "noeviction",
      "used_memory": 1469704,
      "version": "7.4.12"
    },
    "ttl_share": 0.0,
    "with_ttl": 0
  },
  "figures_failed": null,
  "last_analysis": null,
  "live": null,
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
    "used_memory": 1469704,
    "version": "7.4.12"
  },
  "shards": [],
  "status": "connected",
  "target": "primary"
}
```

### `GET /api/server/clients`

`CLIENT LIST` on the picked node, grouped as the TUI groups it. Clients the dashboard itself uses are marked `own`. A missing permission gives `{"off": ...}`.

Example, `GET /api/server/clients`. Reply: HTTP 200

```json
{
  "connections": [
    {
      "addr": "192.168.65.1:29502",
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
      "addr": "192.168.65.1:18752",
      "age": 2,
      "cmd": "client|list",
      "db": 0,
      "id": 7,
      "idle": 0,
      "lib": "redis-rs 1.8.1",
      "mem": 23450,
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
      "mem": 45850,
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
      "mem": 45850,
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

`SLOWLOG GET` on the picked node. Each `line` is masked: the capture ran `CONFIG SET masterauth example-secret` first, and its line is `CONFIG SET masterauth ***`. `settings` holds the threshold and length (`settings_off` when `CONFIG GET` is refused), and `keys_off` says why the key list is missing.

Example, `GET /api/server/slowlog`. Reply: HTTP 200 (`$.entries` cut to the first 3 of 71)

```json
{
  "entries": [
    {
      "args_cut": 0,
      "client": "192.168.65.1:29502",
      "client_name": "",
      "command": "CONFIG SET",
      "id": 70,
      "keys": [],
      "line": "CONFIG SET masterauth ***",
      "micros": 7,
      "more_keys": 0,
      "note": null,
      "unix": 1794724962
    },
    {
      "args_cut": 0,
      "client": "192.168.65.1:18752",
      "client_name": "",
      "command": "CLIENT LIST",
      "id": 69,
      "keys": [],
      "line": "CLIENT LIST",
      "micros": 12,
      "more_keys": 0,
      "note": null,
      "unix": 1794724962
    },
    {
      "args_cut": 0,
      "client": "192.168.65.1:18752",
      "client_name": "",
      "command": "INFO",
      "id": 68,
      "keys": [],
      "line": "INFO",
      "micros": 108,
      "more_keys": 0,
      "note": null,
      "unix": 1794724962
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

This starts a Monitor run on the picked node, using the TUI's runner. The same limits apply:
the run stops after `max_lines` lines or `max_secs` seconds, and lines are masked. The reply
is 202 `{run, max_lines, max_secs}`. Lines come as `monitor` events. A run is refused with
422 when the server has no MONITOR (some managed services) or the permission check turned it
off. DELETE stops the run and answers `{"stopped": bool}`.

Example, `POST /api/server/monitor`. Reply: HTTP 202

```json
{
  "max_lines": 10000,
  "max_secs": 30,
  "run": 1
}
```

Example, `event: monitor`. Event

```json
{
  "at_unix_ms": 1794724962129,
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
      "client": "192.168.65.1:29502",
      "cut": 0,
      "db": "0",
      "line": "SET user:2 y",
      "millis": 129,
      "unix": 1794724962
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
      "at_unix_ms": 1794724962243,
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
through the one write gate. A read-only profile gets 422, a prod profile with writes on gets
a 409 `type` question, and a profile with writes on sends at once. The reply is
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

Example, `POST /api/pubsub/publish (read-only prod)`. Reply: HTTP 422

```json
{
  "error": "Not sent: prod-cache is read-only. Writes are turned on with w on 0 Connections."
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
replies}`. `line` is the masked line, and `replies` holds the formatted replies (`kind` is
`text`, `table`, `json` or `raw`).

Example, `POST /api/cmdline {"line":"GET user:1"}`. Reply: HTTP 200

```json
{
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
      "took_ms": 0.418625
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
      "took_ms": 0.5069579999999999
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
      "took_ms": 0.456042
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
      "took_ms": 0.432667
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

Example, `POST /api/cmdline {"line":"SET user:3 z"}`. Reply: HTTP 200

```json
{
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
      "took_ms": 0.517667
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
    "id": "6c7990e755c5769742e27ddecc19241f",
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
    "id": "6c7990e755c5769742e27ddecc19241f",
    "kind": "ask",
    "text": "KEYS is @dangerous: it can slow the server down. Send it? KEYS user:*"
  }
}
```

Example, `POST /api/confirm/<id> (KEYS)`. Reply: HTTP 200 (`$.replies[].json` cut to the first 3 of 5)

```json
{
  "line": "KEYS user:*",
  "node": null,
  "replies": [
    {
      "cut": false,
      "error": false,
      "json": [
        "[",
        "  \"user:2\",",
        "  \"user:1\","
      ],
      "raw": [
        "[array] *3",
        "  [bulk string] $6 \"user:2\"",
        "  [bulk string] $6 \"user:1\"",
        "  [bulk string] $6 \"user:3\""
      ],
      "table": [
        "#  value",
        "1  user:2",
        "2  user:1",
        "3  user:3"
      ],
      "text": [
        "1) \"user:2\"",
        "2) \"user:1\"",
        "3) \"user:3\""
      ],
      "took_ms": 0.530708
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
  "line": "SET user:3 z",
  "node": null,
  "replies": [],
  "route_note": null,
  "sent": 0,
  "status": "note",
  "text": "Not sent: prod-cache is read-only. Writes are turned on with w on 0 Connections.",
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

