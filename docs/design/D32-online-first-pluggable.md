# D32: online-first with htmx, resources as host capabilities, core entities, the journal as a guest, and a person's own Claude subscription

**Decided** 2026-09-06, by Nate, in one conversation with Pia after #83
landed. Six decisions, recorded together because they were made together and
because they bound each other. The one pick below that Nate did not make
himself (the guest language) is marked as Pia's and open to reversal. Issues:
#84, #85, #86, #87, #88, and #21 updated.

## 1. The client is server-rendered HTML with htmx; online-only for now

Nate: "would it be easier to make this always online for now.. and use htmx
instead?" Yes. The Solid.js shell under `web/` is 562 lines of chat, and apps
have no path to the screen at all: D24's sentence about apps contributing
"HTML fragments the shell mounts" was the whole design. With the shell itself
server-rendered, that sentence becomes the architecture: an app route returns
a fragment, the daemon composes the page, and installing an app rebuilds
nothing. The committed `web/dist`, the `hive-webui` embed of it, the Vite
build and the gate step that rebuilds and diffs it all go away.

What it costs, accepted: D30's voice is a real client loop and becomes a small
JavaScript island; a desktop client later starts from rendered pages rather
than the Solid shell D31 assumed. The house rule that htmx fits admin panels,
dashboards and anything online-only is what this repo now is. Offline is a
later decision, not a lost one.

What stays: the chat stream is the same SSE frames, consumed by htmx's SSE
extension or an island; the e2e specs keep their element ids and behaviour and
are the acceptance test of the rewrite; the CSP stays strict. Guest output is
`untrusted` (invariant 9, 12), so an app fragment is sanitized host-side before
it is composed into a page, never inserted raw. (#84)

## 2. Every resource is a host capability, sockets included; none is a guest's

Nate: "we need the ability to make sockets. take ingress traffic. Role this
off to admins. basically the wasm framework needs the be very pluggable.. all
resources provided." The goal is adopted whole; the shape is fixed by
invariant 5, which is what makes a runaway guest killable and an AI-built app
unable to roam the LAN.

- **Ingress needs nothing new.** The host listens (invariant 13). A manifest
  declares routes, tools and subscriptions and the host calls the guest with
  the payload. #18 mounts them. **Guests never listen**; an app that wants
  inbound traffic gets a route.
- **Egress is a new capability domain, `hive_net`**: `connect`, `send`,
  `recv`, `close` over a connection the host owns, valid for one invocation
  and closed when the call returns or its deadline fires (invariant 7). TLS is
  terminated by the host. Declared in the manifest, **granted by an admin at
  install** (the trust tiers of #19: an AI-built `local` app starts with
  none), and every connection goes through the same allowlist the harness
  egress proxy enforces, keyed per install (invariant 14: the rule travels
  with the dial). Every `recv` is `untrusted`. No UDP, no raw sockets, no
  listeners; those are a different decision if ever wanted. (#85)
- "Rolled off to admins" is therefore not a new mechanism. It is the trust
  tier plus the install grant, which a human writes.

## 3. Core entities, and one app reading another's collection

Nate: "built in entities for the journal is tasks, lists, contacts, decisions.
more can be made. WASM apps should be able to share direct data storage
access too"; his example an Outlook duplicate that talks to Stalwart and keeps
its contacts in the journal's contacts.

Half exists. Every app declares collections per install; every stored document
is also a row in the platform-wide `entities` table with kind, install,
collection, ref, owner, author and trust; `grants.subject_kind` already
includes `collection` and `entity`. What does not exist is the door: a guest's
storage call resolves only its own install's schema.

- **Core kinds** are `entries`, `tasks`, `lists`, `contacts`, `decisions`.
  They live in a **core install per owner**, created with the principal.
  Nate's contacts and Maggie's are different owners; the key stays complete
  (invariant 14). A custom entity is a collection; more kinds by manifest.
- An app's manifest declares what it **uses** and how (read or write). The
  registry derives the collection grants the install needs, and **activating
  the install writes them** on the core install through the existing grants
  seam, by the human who activates (D19). Absence of a grant is deny.
- The storage capability accepts a **qualified collection name**, and the
  resolution and the access decision happen in the ONE predicate in
  `hive-store` (invariants 1 and 11). No handler, no guest, no second code
  path, never SQL across schemas. The writing app is the `author_actor` on the
  row; ownership stays with the principal (invariant 2). (#86)

## 4. The journal is a guest app

Nate: "journal should be a wasm module." It is `apps/journal`, a first-party
guest, not host code. Most of it is declarative: the core collections with
generated CRUD, so the host derives list, get, create, update and delete as
functions, tools and routes. The guest code is the handful of functions that
are not CRUD and the htmx fragments. Its collections are the core kinds of
decision 3, so the journal is where "contacts" lives for the mail app and
anything else. (#21)

## 5. Guest languages: criteria first, Rust now, TypeScript second

Nate: "let's write it in a language that makes sense here. we don't have to
use rust for everything now." The criteria, which outlive the pick:

1. compiles to `wasm32-wasip1` as a reactor, no threads;
2. small module, fast cold start under the instance pool;
3. an SDK exists or is cheap (the Rust one is 442 lines);
4. byte-reproducible builds, because CI rebuilds every committed guest and
   refuses a diff;
5. what an AI writes reliably against a JSON ABI, because the point of the
   project (#27) is AI-built micro apps.

Rust meets 1 to 4 today and the journal is written in it first, because the
SDK exists and the module is thin. **TypeScript is the second guest language**,
via Javy (QuickJS compiled to wasm, WASI preview 1, host imports through its
plugin seam): it wins criterion 5 outright and costs one to two megabytes per
module and an interpreter's cold start, which the compile cache and the warm
pool absorb. AssemblyScript was considered and rejected: TypeScript-shaped but
not TypeScript, which is worse for an AI than either. *(Pia's pick, recorded
for reversal.)* (#88)

## 6. A person's own Claude subscription runs their agents; it is not the login

Nate: "Sign in with claude subscription.. yes they do. and i don't want to use
it for authentication, just to get the token so users can use their
subscription since they're using claude code directly over the app."

`claude setup-token` ("Set up a long-lived authentication token (requires
Claude subscription)", Claude Code 2.1.263 on the fleet host) hands a person a
long-lived token after an OAuth flow in their own browser. The platform's
"connect your Claude account" step takes that token once, stores it in the
vault owned by that principal, and every hosted run for that person is
injected with it as `CLAUDE_CODE_OAUTH_TOKEN` (the variable name is verified
against the CLI version pinned for the harness image before anything relies
on it). One person, one token, never shared across principals; revocation
expires the lease. The platform's own login stays its own; Keycloak over OIDC
is the intended front door, and Anthropic is not an identity provider here.

The prerequisite is the vault `docs/vault.md` describes and nothing implements:
Postgres-backed, entries owned by a principal, versioned, encrypted with a key
the daemon derives at boot and never writes; **leases, not reads**, recorded
against the run and injected as environment into a tmpfs home so a snapshot
cannot capture them; a lease names the actor acting for the principal
(invariant 2). Open before the enrollment step ships: Anthropic's terms for a
consumer subscription driving Claude Code on a shared host. Per person and
never shared is the conservative shape either way. (#87)

## Order

#18 mount the surfaces, then #84 the htmx shell, #86 core entities, #21 the
journal, #85 egress, #20 the browse tool with Kitesurf as its first driver,
#87 the vault and Claude tokens, #88 the TypeScript SDK, then #27 the builder
loop. Recorded on #29.

## What lost

- *Keep the Solid shell and add fragments beside it*: two UI paths to keep
  consistent, for a client that is chat today.
- *Guest-held sockets or listeners*: invariant 5, and the LAN.
- *Direct cross-schema reads between apps*: a second enforcement point.
- *A journal in host code*: the platform would then have one app it cannot
  build itself, which is the opposite of the point.
- *Anthropic as the identity provider*: not offered, and the token is for
  spending the person's own subscription, not for saying who they are.
