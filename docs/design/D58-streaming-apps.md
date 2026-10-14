# D58: realtime streaming apps: an app route may be a stream, dispatched as a raw HTTP exchange over the per-install socket with bodies flowing both ways, ranges and long-lived responses passed through, WebSocket upgrades proxied, root-path routes on the per-install origin for clients that expect them; the envelope becomes headers and the daemon stays the only front door

**Decided** 2026-10-14 by Nate, relayed by Pia from Forage: "and have the
ability to add realtime streaming apps." Forage's reading, taken: byte-range
and HLS serving, long-lived streams, SSE and WebSocket at media throughput,
and root-path routes for apps whose clients expect them (Jellyfin's
`/System/Info`, `/Items`). nectar is the reference consumer (D56). Shape by
Propolis. Builds on D53 (an app is a process; the host dispatches a tool
call, a route, a subscription delivery or a cron tick as one HTTP request
per invocation over the per-install unix socket with `{trust, data}` in
and out), D44 (app UI on a per-install origin; the shell never runs an
app's markup), D45 (APIs only; SSE for the event feed), D40 and invariant
3 (blobs by reference), invariant 12 (trust structural in every response),
invariant 13 (the daemon reachable over a unix socket) and D55 (host
installs, the host profile). **Amends D53 §1** (the dispatch shape, for
routes marked as streams) and **D44** (a root-path claim on the per-install
origin).

## What was checked before deciding

- **Today a route is a buffered JSON exchange.** `hive-httpapi`'s app
  routes build a `RouteCall` whose body is a `Vec<u8>`, dispatch it, and
  answer `application/json` (`crates/hive-httpapi/src/apps.rs`,
  `crates/hive-surfaces`). Nothing streams; a response is assembled whole,
  which is right for a tool call and wrong for a two-hour file.
- **nectar's streaming surface** (`ts/server/src/routes/streaming.ts` and
  `routes/jellyfin/`): the server decides direct play, direct stream or
  transcode; serves files with `Range` requests; serves HLS manifests and
  segments from a durable ledger (`live_hls_*`); reports playback over
  long-lived connections; and answers Jellyfin's paths at the root of its
  origin (`/System/Info`, `/Items`, `/Users/AuthenticateByName`). No
  WebSocket in nectar's Bun server today; SSE is used.
- **The daemon has SSE** (`hive-sse`, the event feed and the chat stream)
  and no WebSocket dependency in the workspace. axum carries both.
- **D53's sandbox gives an app no network.** The only way bytes reach a
  client is through the daemon, which is the point (invariant 1 at the
  door, the egress allowlist, one origin per install). So "streaming" has
  to mean the daemon streaming on the app's behalf, not the app opening a
  port.

## The decision

### 1. A route may be a stream, and a stream is a raw HTTP exchange over the socket

- The manifest's `routes` gain `stream: true`. A stream route is
  dispatched to the app's process over the same per-install unix socket
  (D53 §1) as **a raw HTTP/1.1 exchange**: method, path, query and headers
  forwarded, the request body streamed in as it arrives, the response
  status, headers and body streamed out as the app produces them, with no
  buffering beyond the socket's. A tool call, a subscription delivery and
  a cron tick keep D53's JSON exchange.
- **The envelope becomes headers.** `{trust, data}` cannot wrap a byte
  stream, so for a stream route the daemon sets `X-Hive-Trust`,
  `X-Hive-Actor`, `X-Hive-Principal` and `X-Hive-Install` on the request
  into the app (the facts the credential pins, invariant 2, resolved by
  the daemon and never by the app) and reads `X-Hive-Trust` on the
  response, defaulting to `untrusted` when the app sets nothing (invariant
  12: trust is structural, and an absent claim is the low one). The
  daemon strips every `X-Hive-*` header a client sends before forwarding,
  so an app cannot be told facts by the client.
- **Ranges pass through.** `Range` in, `Content-Range`, `Accept-Ranges`
  and `206` out, untouched by the daemon; the app decides what a range
  means for its content. A blob served through a stream route is read by
  the app through `hive.blobs` by reference (invariant 3) and streamed; the
  daemon never hands a client a blob path or a hash.
- **Long-lived responses are allowed and bounded.** A stream route may
  hold a response open (an HLS segment being written, an SSE feed, a
  playback-report channel) for as long as the client stays, under a
  per-install **concurrency cap** and an **idle timeout** from the host
  profile (D55 §1) with platform defaults, and a deadline per request
  that an app may raise in the manifest up to a platform ceiling. A
  stream that exceeds its cap is refused with the message; an idle one is
  closed; both are events. The supervisor's rule from D53 §4 stands: a
  hung app costs its own container.
- **Throughput.** The daemon copies bytes between the client and the
  socket, which is one copy and no parse, the same cost the egress proxy
  already pays for a harness run. The measured number is an open item
  below, and a zero-copy path for a blob on the same node is a later
  optimisation, not a reason to let an app open a port.

### 2. WebSocket upgrades are proxied

- A stream route may accept an `Upgrade: websocket` request. The daemon
  completes the upgrade with the client and opens the same upgrade to the
  app over the per-install socket, then pumps frames both ways without
  reading them. The app sees a WebSocket on its socket exactly as if it
  were listening on a port; the client sees the daemon's origin.
- The same cap, idle timeout and events as §1; the credential is checked
  once, at the upgrade, and the daemon closes the socket on revocation
  (the D47 event, as D57 §3 uses it).
- Nothing today needs it (nectar uses SSE); it is here because "realtime
  streaming apps" without it would be a promise with a hole, and the
  proxy is the same machinery as §1 with the bodies replaced by frames.

### 3. Root-path routes on the per-install origin

- D44 gives every install an origin of its own. A host install (D55 §1)
  may **claim the root of that origin**: its manifest declares `routes`
  with paths like `/System/Info` and `/Items/{id}`, and the daemon serves
  them at the root of the install's origin rather than under a prefix, so
  a client built for an existing API (a Jellyfin client against nectar)
  reaches the paths it expects. The claim is per install and per origin,
  so two hosts never contend for a path (invariant 14: the origin carries
  the install).
- The daemon reserves its own paths on every origin (`/.hive/…` for the
  frame token exchange, health and the credential routes) and refuses a
  manifest that claims one, with the message naming the path.
- An app with no root claim keeps today's shape: its routes under the
  prefix D44 gives them.

### 4. What the daemon still does on every streamed request

- **Resolve the credential** (the `MediaBrowser` token of D57 §4, a
  cookie, a bearer) to actor and principal through `Guard`, before a byte
  reaches the app; a request with none gets the one 401. A stream route
  may be declared `public` only under #95's rule when that record exists;
  until then there is no anonymous stream.
- **Set the trust headers in, read the one out** (§1).
- **Count, cap and time out** (§1), and emit the events.
- **Never inspect the body.** The daemon does not parse an HLS manifest
  or a video frame; what it enforces is who, how many and for how long.

## Acceptance scenarios, checked in before the code

1. **A stream route streams.** A checked-in test app answers a stream
   route with a body produced in chunks over two seconds; the client
   receives the first chunk before the last is produced (asserted by
   timing against a buffered control route), and the whole body matches.
2. **Ranges pass through.** The test app serves a 10 MB blob by reference
   through a stream route; `Range: bytes=1000-1999` yields `206`, a
   `Content-Range` the app set, and exactly those bytes.
3. **The envelope is headers, and the client cannot forge them.** The app
   records the `X-Hive-*` headers it received; they match the credential
   the test presented; a client request carrying `X-Hive-Actor` is
   forwarded without it; a response with no `X-Hive-Trust` is delivered
   with `untrusted` and the test asserts the header the client saw.
4. **Caps and timeouts bite with their messages.** The (N+1)th concurrent
   stream on an install capped at N is refused with the message; a stream
   idle past the timeout is closed and the event names it; a hung handler
   is cut at its deadline (D53 scenario 6, on a stream).
5. **A WebSocket round-trips.** The test app echoes frames over an upgraded
   stream route; the client sends ten frames and receives ten; revoking
   the credential mid-session closes the socket within one event delivery.
6. **Root paths resolve per origin.** Two host installs each claim
   `/System/Info`; a request to each origin reaches its own install (asserted
   by the install id in the response); a manifest claiming `/.hive/health`
   is refused at install with the message naming the path.
7. **No anonymous stream.** A stream route requested with no credential
   gets the one 401 and the app's socket sees nothing (asserted by the
   app's request log being empty).
8. **nectar's playback through the kit.** nectar's checked-in parity
   fixture plays a file (decide, direct play with ranges, an HLS manifest
   and its segments) through the daemon's stream routes and gets the same
   bytes as from its own server; run in nectar's CI against a hive-mind
   image.

## Order

1. **The raw exchange over the per-install socket** for `stream: true`,
   with the headers, the strip, ranges, and the default trust. Scenarios
   1, 2, 3, 7. After the D53 runtime exists, since there is no process to
   dispatch into before it.
2. **Caps, idle timeouts and events.** Scenario 4.
3. **Root-path claims** on the per-install origin and the reserved paths.
   Scenario 6.
4. **WebSocket proxying.** Scenario 5.
5. **nectar's streaming slice** on nectar's side (D56 §1). Scenario 8.

## What lost

- *Apps opening a port.* The sandbox has no network on purpose (D53 §1):
  one front door, one credential check, one origin per install.
- *Buffering streams in the daemon.* A two-hour file assembled in memory
  is neither streaming nor safe.
- *Wrapping streamed bytes in JSON.* The envelope's job is trust and
  identity; headers carry both without touching the body.
- *The daemon serving blobs directly on an app's behalf by hash.* A hash
  is not a reference (invariant 3); the app serves through `hive.blobs`
  and the daemon streams what the app sends.
- *Deferring WebSocket.* It is the same proxy with frames instead of
  bodies, and leaving it out would make "realtime" a word.

## Open

- Throughput and added latency of the socket proxy for a 50 Mbit/s HLS
  session and a 4K direct play, measured on the cluster before nectar's
  slice is called done; the zero-copy path for same-node blobs if the
  number is bad.
- Default cap, idle timeout and deadline ceiling per install; the host
  profile's fields for them.
- HTTP/2 or HTTP/3 to the client: the daemon's choice, invisible to the
  app, which always sees HTTP/1.1 on its socket.
- Whether a stream route may be `public` (#95), which decides whether a
  shared HLS link can exist at all.
