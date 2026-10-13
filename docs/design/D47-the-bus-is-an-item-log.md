# D47: the bus is a log of items addressed to identities; delivery is pushed, per session, and honest about its state; a board is a view of items, never the transport

**Decided** 2026-09-12 by Nate ("i also want a realtime communication bus so
same identity sessions all get the same message on important decisions and
different identities can call each other"), briefed by Pia as #99, and made
urgent on 2026-10-13: "let's also ensure hive mind where we'll be running in
the future definately fixes this latent board info and messaging bus as
efficiently as possible." The evidence that shapes it is Forage's write-up of
the 2026-10-12 evening, six failures that two identities actually hit while
coordinating through the KB's task boards and the per-session sockets.
Shape by Propolis. Builds on D26 §4 (one AI actor per runtime and principal),
D43 (a database per org, `NOTIFY` as the bell), D45 (APIs only, a sync API
over cursors) and D46 (boards, placements, links). It does not change
invariant 4: the events table is the transport and the bell is a wakeup.

## What failed, and is therefore the specification

Written down as observed, not as imagined, because a bus designed from a
sketch would have fixed the sketch.

1. **Lines sat unread until someone re-read a whole page.** A board line
   addressed to an identity waited about an hour; the heartbeat that
   eventually found it re-fetched a 60 to 600 KB page every half hour, and
   the larger boards exceed a tool's output limit, so every read is a file
   dump plus a parse. The record (the board) and the transport (someone
   reading it) were the same object.
2. **The address was a process, not an identity.** The sender had to guess
   which session was Pia from a listing that shows session names and no
   profile. After Pia's profile moved (brh-infra #355) both per-session
   inboxes returned `ENOINBOX`, because hand-made filesystem junctions
   pointed at the old path, and a session started before the fix stayed
   unreachable for good.
3. **"Sent" meant "in a file".** A message sat held for approval because
   the two sessions ran in different permission modes. The sender could not
   tell held, unread and refused apart, and told Nate the order had been
   received while it had not.
4. **Every update was an exact-text edit of a long page**, and one failed
   because the pattern did not match. Two sessions editing one page is a
   race with no referee.
5. **Replies came back as prose, twice**: a line on the asker's board and a
   cross-session message saying the same thing. Nothing tied the answer to
   the ask.
6. **Nothing was cheap.** Whole-page reads on a timer, by every session,
   whether or not anything changed.

Pia's 09-12 brief already asked for identity addressing, decision fan-out,
honest delivery state, persistence and a realtime path into Claude Code. The
list above is the same five things with a seventh (items, not pages) and a
test case for each.

## The decision

### 1. The address is an actor; a session is an attachment

An identity (`pia`, `propolis`, `forage`, `comb`, `nate`, `maggie`) is an
**actor** in the org's database: a human, or the AI actor D26 §4 creates per
(runtime, principal). **A message is addressed to an actor, never to a
process.** D26 §4 is therefore the first slice of this work, not a
prerequisite to wait for: until it lands, an agent's message is attributed
to the human, and a bus that cannot say who sent a message has failed
invariant 2 before it delivers anything.

A **session** is one live attachment of an actor: a Claude Code process, a
Codex process, a browser client, a voice bot, a phone. Sessions are rows:

| column | why |
|---|---|
| `id`, `actor_id`, `owner_kind`, `owner_id` | the acting actor and whose authority it attaches under (invariant 2, pinned from the credential by the writer, invariant 11) |
| `runtime` (`claude-code`, `codex`, `client`, `voice`, `other`) and `capabilities` (`push`, `inbox-only`) | what the session can receive: a channel push, or only a polled inbox (§4) |
| `attached_at`, `last_seen_at`, `detached_at` | liveness, so "which Pia is live" is a query and not a guess |
| `cursor` | the last event id this session has consumed, so a reattach reads deltas (§6) |

A message to `pia` reaches **every live session of that actor** and waits in
the log for the next one when none is live. There is no per-profile
filesystem link anywhere in the path: the session's address is its row, and
the row is found by the actor it attaches as. Failure 2 cannot recur because
the thing that failed (a path per process) does not exist.

### 2. Items, not messages; the board is a view

The unit is an **item**, which is D46's card given a few more kinds:

- `kind`: `ask`, `decision`, `reply`, `note`, `task`, `document` (the D46
  kinds stay; `ask`, `decision` and `reply` are the bus's).
- `from_actor` (pinned from the credential), `to_actors` (zero or more
  actors; empty means "placed on a board, addressed to nobody"), and
  `reply_to` (an item id; mandatory on a `reply`).
- `state`: `open`, `acted`, `done`, `blocked`, `refused`, with `evidence`
  (a link: a commit, a PR, another item) required on `acted` and `done`.
- `body` (text), `links` (D46 §1: any item to any item, with backlinks),
  `history` (the events the item has emitted, §3).
- Owner, context, HLC and timestamps as every entity has under D45/D46.

**A board is a set of placements of items** (D46 §1). "Tasks — Propolis" is
a board in the household org whose placements are the items addressed to
propolis or placed there by hand; "Done — Propolis" is the same items in a
done state, which is a filter and not a second page. Ticking a line is one
small write to one item. Two sessions touching two items never conflict, and
two touching one item are ordered by the op log (D45 §3). Failure 4 is gone
because there is no page to edit; the page is rendered from rows.

**A reply attaches to the ask.** `bus.send` with `reply_to` creates a
`reply` item whose placement is wherever the ask is placed, and moves the
ask's state when the reply says so (`done`, `blocked`, `refused`, or an
answer that leaves it `open`). The asker's view updates because it is a view
of the ask. There is nothing else to post, so failure 5 has nowhere to
happen. An idempotency key on every send (D45 §2's operation key) means a
retried send is one item, not two.

The message log and the board are **one store**, because the alternative is
two stores for one idea: a line on a board that is also a message to its
owner would otherwise be written twice and drift. The D46 order already puts
boards, placements and links first under D43 phase 1; this record adds the
three kinds, the addressing columns and the state machine to that same
slice.

### 3. The events table is the transport; deliveries are the state

Every item write is an event (`bus.item.created`, `bus.item.replied`,
`bus.item.state`), appended in the same transaction as the row, exactly as
every other entity's writes are today. Invariant 4 stands unchanged: the
tailer in `hive-bus` re-reads an overlap window, dedupes by id, and polls on
the backstop whether or not anything rang. Under D43 the bell is `NOTIFY`
per org database; under SQLite it is the in-process wake. **No consumer of
the bus may depend on the ring.** The suite runs with `listen: false` as it
does today, and the bus tests run under it too.

A **delivery** is a row per (item, session), written by the daemon when an
item is created with the session's actor among `to_actors` (or when a
`decision` fans out, §5), and when a session attaches and has items it has
not seen:

```
queued → delivered → read → acted | refused
                   ↘ held (reason)
```

- `queued`: the row exists; no session has been told.
- `delivered`: the daemon pushed it (§4) or the session's inbox call
  returned it. Written by the daemon, from what it did, not from what the
  receiver claims.
- `held`: the receiving runtime reported that it is holding the message for
  its human's approval, with the reason it gave (a permission mode, a
  refusal policy). Reported by the channel process (§4), which is the only
  party that can see it.
- `read`: the receiving session's model has consumed it. Reported by the
  channel process when the notification is enqueued as a turn, or by the
  inbox call with `ack`.
- `acted` / `refused`: set on the **item** by the receiver's reply (§2) and
  mirrored to the delivery with the evidence link or the reason.

The sender sees the delivery state per session of the addressee, and the
item's state across them, from the same rows. "Sent" is not a state here.
Failure 3 is answered by the one row that was missing: `held`, with its
reason, written by the party that held it.

### 4. Push into Claude Code through a channel; a cursor inbox for everything else

Claude Code's supported inbound path is an MCP server that declares the
`claude/channel` capability and pushes `notifications/claude/channel
{content, meta}`; the session enqueues it as its next prompt turn (verified
in the 2.1.269 binary, #99: `Channel notifications registered`, `priority:
"next"`, `origin: {kind: "channel"}`). The person selects channel servers at
launch with `--channels`.

**`hive-channel`** is a small Rust stdio binary in this workspace, started
by the profile launcher (brh-infra's `install.ps1`, Pia's side), that:

- attaches a session (§1) with the profile's credential, which names the AI
  actor acting for the person (`CC_PROFILE` is in every launcher's
  environment);
- subscribes to the daemon's `/events` SSE with a filter on its own actor's
  deliveries, resuming from its cursor (the SSE `id:` is the event id, so a
  reconnect misses nothing; this is what `hive-bus` already does for the
  browser);
- turns each delivery into one channel notification whose `content` is the
  item rendered for a model (kind, from, body, the ask it replies to, the
  board it is placed on, and the three verbs the receiver can answer with)
  and whose `meta` carries the item id;
- reports `delivered` when the push is accepted, `read` when the runtime
  enqueues it, and `held` with the runtime's reason when it does not; and
- exposes the §7 tools so the session can reply, ack and read its digest
  without a second connection.

**Everything that cannot receive a channel** (Codex, Kimi through a
gateway, a Claude Code started without `--channels`, a session on a
provider where the research-preview channel flag is unavailable) uses
`bus.inbox {since: cursor}` at wake-up and from a `Stop` hook. The record
says plainly that this is polling, that it is the fallback and not the
design, and that its cost is bounded: a cursor read returns only deltas and
a session with nothing new gets an empty page. The two paths write the same
delivery rows, so a sender cannot tell them apart except by latency, which
is shown.

**Per-session sockets stay for what they are**: a local, session-addressed
path with no persistence. They are not the bus and nothing here reads or
writes them.

### 5. Decisions fan out, to every session

A `decision` item addressed to an actor creates a delivery for every live
session of that actor and a queued delivery for the next one that attaches;
addressed to the org (`to_actors` empty with `fanout: org`) it reaches every
actor in the org that holds a session or will. The sender sees every
delivery. Two Pia sessions holding different beliefs about who coordinates
(the 09-12 case) is answered by both receiving the same item with the same
id, and by `bus.digest` (§6) showing a late session the decisions it missed
before anything else.

What counts as a decision, and who may make one, is a convention in the KB
(`journal-and-tasks`), not a bus rule. The bus carries and remembers; it
does not decide (#99's words, kept).

### 6. Cheap by default

- **No session polls while it has a channel.** The daemon's tailer already
  runs once per org database; a session costs one SSE connection and a
  cursor. A session with nothing addressed to it receives nothing.
- **Reads are deltas.** `bus.inbox {since}` and the SSE resume both take the
  session's cursor; there is no call that returns a board.
- **`bus.digest` replaces the wake-up read.** One call at session start
  returns, for the session's actor: decisions since its cursor, open asks
  addressed to it grouped by state, items it sent that changed state, and
  the counts. The wake-up skill reads that instead of two whole boards. A
  digest is bounded by what changed, not by how much exists.
- **A board render is a query**, paged, from the client (D45) or from a
  tool; the KB page stops being the source once the client renders boards
  (§8).

### 7. The surface

MCP tools on the daemon's existing `POST /mcp` (#90), reachable over the
unix socket (invariant 13) and through `hive-channel`:

| tool | does |
|---|---|
| `bus.send {to, kind, body, reply_to?, place_on?, idempotency_key}` | creates an item (and its deliveries); `reply_to` makes it a reply and may move the ask's state |
| `bus.state {item, state, evidence?, reason?}` | `acted`, `done`, `blocked`, `refused` on an item the caller may act on; evidence required on `acted` and `done` |
| `bus.inbox {since}` | deliveries for the caller's session since a cursor; marks them `delivered` |
| `bus.ack {deliveries[]}` | marks `read` |
| `bus.digest` | §6 |
| `bus.sessions {actor}` | which sessions of an actor are live and what each can receive, so a sender knows before sending whether a push or a queue is what will happen |

The same calls exist as authenticated API routes for the Solid client and
for a person on a phone (`nate` is an actor too). Every one goes through
`Guard`: who may address whom is a grant (`subject_kind = 'actor'`,
`verb = 'address'`), defaulting at org creation to every member addressing
every actor in the org, and absence is deny (invariant 1). No ambient
anonymous sender; a webhook that wants to post an item is #98's
token-bearing class and comes later through that mechanism.

### 8. Transition from the KB boards

The journal stays in the KB; it is reflection, not coordination. The task
boards move:

1. While the client cannot yet render boards, a **one-way renderer** writes
   each board's KB page from its items on every change, with a header saying
   the page is a view and edits to it are discarded. The page keeps its URL
   so links in old journal pages hold.
2. When the D45 client renders boards, the renderer is turned off and the
   pages get a pointer.
3. The wake-up and heartbeat procedures in `journal-and-tasks` change to
   `bus.digest` and the channel; the half-hourly cron goes. That is a
   brh-infra and skill change on Pia's side, after the first slice is live
   on the cluster.

Nothing is imported from the existing pages by machine; the open lines are
re-entered as items by their owners, which is also the moment to drop the
ones nobody still wants.

## Acceptance scenarios, checked in before the code

D24's rule: the host is tested by what uses it. Each is Forage's offer of
the evening's timeline as a test case, written as a scenario a suite can
run on both engines with the bell disconnected:

1. **The `ENOINBOX` window.** Actor A sends to actor B while B has no live
   session. The delivery is `queued` and A can see that. B attaches; the
   delivery becomes `delivered` within the backstop interval with
   `listen: false`, and the item arrives once, not once per reattach.
2. **The held message.** B's channel reports `held` with a reason. A sees
   `held` and the reason; nothing shows `read`. When B's runtime releases
   it, the state moves to `read` and A sees that too.
3. **The line unnoticed for an hour.** A sends to B with a live channel
   session. The delivery is `delivered` within a bounded latency with the
   bell connected, and within the backstop with it disconnected, and the
   test asserts no inbox call was made by B's session. Failure 1 is a
   latency bound, not a hope.
4. **The duplicated reply.** B replies to A's ask twice with the same
   idempotency key; A sees one reply, attached to the ask, and the ask's
   state moved once. A board render of A's board shows the ask with its
   reply and no second line.
5. **Addressing is deny by default.** A grant-less actor's `bus.send` is
   refused with the grant message, not a foreign-key error (CLAUDE.md,
   *Check which refusal*).
6. **Attribution.** An item sent from an AI session has `from_actor` = the
   AI actor and `owner` = its principal; a test that the API cannot set
   either from the request body (invariant 11).

## Order

1. **D26 §4**: the AI actor per (runtime, principal), created lazily by the
   chat layer; the launcher's credential names it. Small, overdue, and the
   bus is meaningless without it.
2. **The schema**: `items` with the three kinds and the addressing columns,
   `placements` and `links` (D46 order step 1), `sessions`, `deliveries`,
   the grant kind, on both engines under D43 phase 1 (after slice 3,
   `hive-store` on Postgres, so the predicate functions carry the new kind
   once rather than twice).
3. **The tools and the SSE filter** (§7, §4's fallback path), with the six
   scenarios green with the bell disconnected.
4. **`hive-channel`** and its launcher flag (the flag is Pia's side).
5. **The renderer** (§8.1) and the cutover of the wake-up skill to
   `bus.digest`.
6. Boards in the D45 client, which is D45 order step 2 and not this record's
   work.

## What lost

- *A `bus_messages` table beside the boards* (the 09-14 sketch). Two stores
  for one idea; a board line that is also a message would be written twice.
- *Per-session sockets as the transport.* Session-addressed, unpersisted,
  Claude-only, and the thing that returned `ENOINBOX`.
- *The KB page as the transport.* It is the record of decisions and stays
  that; it was never a queue and it behaved like one badly.
- *A smarter heartbeat* (diffing cached pages on a timer). It is failure 6
  with better manners, and it was built on 2026-10-12 as a stopgap; this
  record is what retires it.
- *Pushing into Claude Code any other way.* There is one supported inbound
  path; a hook, a file watcher or a Discord bot would be reinventing
  delivery with worse state.
- *Letting the receiver write its own delivery state.* `delivered` is what
  the daemon did; `held` and `read` are what the channel observed; `acted`
  needs evidence. A receiver that could write `read` on its own would make
  the state as honest as "sent" was.

## Open

- Whether `apis` (the DTC identity) ever attaches. Assumed no, as #99 had
  it; and never DTC content on the Roadhouse instance.
- Voice and Discord as producers and consumers (#103, D36); each is a
  session kind here and nothing more is designed for them.
- The research-preview flag for custom channels and whether a gateway
  exposes it; if not, Kimi and the gateway runtimes stay on the inbox path.
- Rate limits on `bus.send` per actor, and a size bound on `body`.
- Whether a `decision` can be addressed across orgs (through a D43 §4
  link). Not until a link exists to test it against.
