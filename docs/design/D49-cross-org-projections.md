# D49: cross-org relations read at local speed over a granted projection; the peer's events keep it current through the same sync API devices use; writes stay requests; nothing is ever joined across databases

**Decided** 2026-10-13 by Nate: "plan for cross org.. we need it.. cross user
exists." The question that produced it: "the jsonb data can relate across
users and different orgs/databases efficiently and fast correct?" Within an
org, yes, by construction (D43 §2: one database, typed relation columns, GIN
on the jsonb bodies, RLS in the same plan). Across orgs the honest answer
was no: D43 §4 made every cross-org read a request to the other org's
daemon, which is fine for a link you click and wrong for a board that shows
items from three orgs, a search, or an AI run that needs to relate them.
Shape by Propolis. **Amends D43 §4 and D46 §1**; keeps their rule that a
join across databases never happens and that an item lives in exactly one
context.

## The idea, in one sentence

An org's daemon is a **client** of each org it is linked to, holding a
**predicate-filtered projection** of exactly what that peer has granted it,
kept current by the peer's events, through the same sync API D45 §2 builds
so a device can hold a filtered replica of a person's data. A device and a
peer org are the same shape to the serving side: a credential, a predicate,
a cursor, deltas.

That is why this is a plan and not a rewrite. D45 order step 1 already
builds "changes since a cursor, per entity type, under the credential"; D43
phase 4 already builds the link, the registry-signed link credential and the
federation route. D49 is the two meeting: the federation route serves the
sync API, and the receiving daemon writes what comes back into tables of its
own.

## The decision

### 1. A link carries a projection scope, granted by the serving side

A link (D43 §4) stays a registry row plus a `peer_orgs` row on each side.
What is added: on the **serving** org B, grants to the **remote principal**
(`(peer org A, actor)`) name what A may project, by the same grant kinds
everything else uses: an item, a board (and so its placements), a
collection, an entity type with a filter. Nothing is projected that B's
predicate would not release to that remote principal on a direct read
(invariant 1); the projection is a cache of released rows, not a new read
path.

Revoking the grant deletes the projection on the next delta (B emits the
revocation as a change with `visible: false`) and, as a backstop, on A's
periodic reconcile, which re-reads the scope from a zero cursor and drops
what B no longer returns. Invariant 4's shape: the push is a wakeup, the
reconcile is what makes the state correct if every push is lost.

### 2. The receiving side holds projections, not copies

On A, projected rows land in `peer_items`, `peer_placements`,
`peer_links` (and `peer_<collection>` for a projected collection): the
peer's row **shape** with three added columns, `peer_org`, `remote_id` and
`remote_version` (B's HLC for the row), and with `trust = untrusted` on
every row, always, because B's claim about B's rows is B's (invariant 12,
as D43 §4 already has it).

The D46 §1 rule that an item lives in one context and is never silently
copied **stands**, because a projection is not a copy in that sense:

- A cannot edit it. A write to a projected item is a request to B's daemon
  (§4), and the local row changes when B's event comes back.
- A does not own it. It carries no `owner` of A's, grants on it are B's,
  and A's predicate treats it as readable-by-the-link and nothing more.
- It disappears when B says so (§1), so it cannot drift into being A's.

Said plainly for the client: a projected item renders with its home org
labelled (D46 §5 already labels every pane with its context), and "edit"
on it is a request that may be refused by B.

### 3. Relating is local

With projections in place, the things that were slow become local rows:

- **Links across orgs.** A's `links` table gains a target kind `peer`
  pointing at a `peer_items` row. The link is one of A's rows; opening it is
  an indexed read. B learns of the inbound link because A sends it as a
  request (`link.create` on the federation route) and B records it in its
  own `peer_links`, so **backlinks show on both sides**, each from its own
  table.
- **Boards across orgs.** A placement on an A board of a B item is an A row
  pointing at the projection. The board query is one join, local. Dragging
  a B item onto an A board (D46 §5) is this, and the share warning D46
  gives for private-to-org does not apply: nothing moves, nothing is
  decrypted, the item stays B's.
- **Search across orgs.** Projected text fields get A's generated tsvector
  (D41), so full text over A's view of B's items is A's index. Vectors are
  **re-embedded by A's model** on arrival (D46 §3a's rule: vectors from two
  models are not comparable), stored on the projection row, and deleted
  with it. The cost is one embedding per projected change; the gain is one
  nearest-neighbour query over everything A can see.
- **AI runs.** A hosted run in A relates B's items by reading A's rows, with
  the trust label travelling into the run (invariant 9: still untrusted in
  instruction position).

### 4. Writes stay requests

A write to a B item from A (a reply on a cross-org ask, a field edit, a
state change) is a request to B's daemon with the link credential, exactly
D43 §4. B's writer pins `author_actor` as the remote principal (invariant
11), B's predicate decides, B's triggers run, and the result comes back to
A as an event through the projection. A's client shows the local row
updating when B confirms, and shows a refused write as a refusal, never as
a local change that then reverts. There is no optimistic cross-org write,
because the authority is B's and the client would be lying for a round
trip.

### 5. Freshness, and what the record promises about it

Projections are eventually consistent, with B's events as the push and a
reconcile as the floor:

- **Push**: B's `hive-bus` tailer sees a change to a row in a projected
  scope and delivers it to A over the federation route as a delta (the same
  frame a device receives). Under D43 the bell is `NOTIFY` and the tailer's
  overlap and dedupe already handle late commits.
- **Reconcile**: A re-reads each link's scope from its cursor on a timer,
  and from zero on a schedule, so a lost push is latency and never a stale
  row forever.
- **Shown, not implied**: every projected row carries `remote_version` and
  `projected_at`; the client shows a projected item's age when it is older
  than the push should allow, and a link's health (`live`, `lagging`,
  `unreachable`) sits beside the org's health in the registry (D43 §8).

The promise is "a change in B shows in A within the push latency when both
daemons are up, and within one reconcile interval otherwise, and the UI
shows which". It is not "the same instant", and the record says so.

### 6. The personal org and the household are two orgs

A person's own space is their personal org (D43 §1), so a journal entry
linking to a household board item is cross-org. Two things make this
cheap:

- **On the device**, the D45 client already holds a replica of every
  context the person belongs to, so relating a private entry to a household
  item is a read across two local replicas with no server involved.
  Projections are for the **server side**: triggers, server search, hosted
  runs, and members of the household who are not that person.
- **Tier (a) content never projects.** A personal item is sealed on the
  device (D46 §2a); the server cannot read it, so it cannot release it to a
  projection. What a household member sees of a private item is what D46 §1
  already says: nothing, until the person moves it into the org. A link
  from a private item to a household item is the private side's business,
  stored encrypted in the private org, and the household never learns of
  it. That asymmetry is the point of the tier.

### 7. Self-hosted peers and the trust boundary

Nothing here loosens D43 §8. A self-hosted B can lie about its rows; the
projection carries them as untrusted, labelled with B's org, and a reader
who linked to B chose to. The registry, not B's database, says whether the
link exists; the link credential is registry-signed; A never connects to
B's database and B never to A's. A projection makes B's lies faster to
read, not more believed.

### 8. Limits

- A projection scope has a row and byte quota per link, set by the serving
  org, with the fill shown on both sides; a scope over quota stops
  projecting and says so rather than projecting partially in silence (D45
  §2's rule for a device that will not fit).
- The federation route is rate-limited per link (closing D43's open item):
  deltas are batched, and a peer that floods is throttled with the reason
  visible in the link's health.
- A projected collection's documents are read-only on A and are not
  re-indexed by A beyond the text and vector columns B's manifest already
  declared; A does not get to run its own triggers over B's data.

## Acceptance scenarios, checked in before the code

The loopback case (A and B on one daemon, D43 §4) runs on every build, so
all of these run in the ordinary suite on both engines, with the bell
disconnected:

1. **Only what is granted projects.** B grants A one board. A's projection
   holds that board's items and nothing else from B; a B item on no granted
   board never appears, and a test asserts that by counting rows, not by
   asserting the granted ones arrived.
2. **Revocation empties.** B revokes the grant; the rows leave A on the
   next delta, and, with pushes disabled, on the reconcile.
3. **Writes are B's.** A edits a projected item; the local row does not
   change until B's event arrives; a write B refuses leaves A's row as it
   was and surfaces the refusal with B's message.
4. **Backlinks on both sides.** A links an A item to a B item; B's view of
   its item shows the inbound link from A, labelled as A's.
5. **Search is local.** A's full-text and nearest-neighbour queries return
   projected B items, and the test asserts the query made no federation
   request (the instrument: a counter on the route, checked for zero).
6. **Untrusted stays untrusted.** A run in A that reads a projected B row
   carries `untrusted` into its result, and a write made after that read is
   marked (invariant 12).
7. **A key that omits a dimension.** `peer_items` is unique on `(peer_org,
   remote_id)`, never on `remote_id` alone; a test with two peers sharing
   an id proves it, because two peers' ids will collide and the question is
   whether it fails closed (invariant 14).

## Order

Lands as the second half of D43 phase 4 and depends on D45 order step 1:

1. D43 phase 2 and 3 (registry, per-org pools, RLS) and D45 step 1 (the
   sync API over cursors) as already ordered.
2. D43 phase 4 as recorded: links, the registry-signed link credential, the
   federation route, the loopback test.
3. **D49 slice 1**: the federation route serves the sync API under the link
   credential; `peer_items`, `peer_placements`, `peer_links` on both
   engines; the reconcile; scenarios 1, 2, 7.
4. **Slice 2**: push via the peer's tailer; links and placements onto
   projections; backlinks both ways; scenarios 3, 4, 6.
5. **Slice 3**: text and re-embedded vectors on projections; scenario 5.
6. **Slice 4**: quotas, rate limits, link health in the registry and the
   client.

## What lost

- *Joins across databases* (`postgres_fdw`, `dblink`). D43's reasons stand:
  one org's credentials inside another's database, and a join that breaks
  the moment the two are not co-located.
- *A request per cross-org read* (D43 §4 as written). Correct, and too slow
  for anything but a click; kept as the write path and as the fallback read
  when a projection is absent.
- *One shared index across orgs* (a central search database). A third
  store with its own copy of everything and its own access decisions, which
  is a second enforcement point for every org at once. The projection keeps
  each org's view inside its own database under its own predicate.
- *Making the household one org to avoid the problem.* Nate's call on
  2026-10-13 was for cross-org to work, and a person's own space being its
  own org is what tier (a) encryption rests on (D46).
- *Optimistic cross-org writes.* The authority is the peer's; a local change
  that reverts on refusal is a lie with a delay.

## Open

- Whether a projection may itself be projected onward (A projects a B item
  to C). Assumed no: a projection is terminal, and C links to B directly.
- The reconcile intervals and the quota defaults; set when measured.
- Whether B should learn which A members read a projected item. Assumed no
  beyond the link-level audit B already has; a projection is read locally by
  design.
