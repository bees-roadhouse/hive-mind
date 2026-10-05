# D44: an app's HTML never runs as the shell; app UI is framed on its own origin, sanitized when it is not, and every mutating route checks a capability token instead of `HX-Request`

**Decided** 2026-10-04: Pia's recommendation, endorsed by Nate the same
evening ("tell them to start working on your input and changes"). Shape by
Propolis. Amends D32 §1 (htmx for the client) without reversing it: htmx
stays right for the host shell, and this record is about the one place it is
wrong, which is markup the host did not write.

## The hole

D32 says apps contribute UI as HTML fragments that htmx swaps into the
shell. htmx does not need script to act: its behaviour is in attributes.
A fragment carrying

```html
<div hx-post="/ui/conversations/new" hx-trigger="load" hx-vals='{"title":"x"}'></div>
```

makes htmx, already loaded by the shell, send a same-origin request the
moment it is swapped in, with the person's `SameSite=Strict` session
cookie (same-site, so the cookie goes) and with `HX-Request: true` set
by htmx itself. `crates/hive-httpapi/src/ui.rs` uses that header as its
CSRF check (`require_htmx`), so the check passes. The CSP does not help:
`script-src 'self'` blocks inline script, and htmx is `'self'`. The app
acts as the person, in the shell, on any route the shell has.

That is invariant 9 broken by construction: untrusted content (an app's
output) reaching instruction position (attributes the shell's interpreter
executes).

**Not exploitable today**, and checked rather than assumed: app routes
(`/apps/{app}/...`, `crates/hive-httpapi/src/apps.rs`) answer
`application/json` only, and no shell template interpolates app output as
markup. The point of deciding now is that the first fragment path is the
one that opens it, so the rule has to exist before that path does.

One correction to the brief, so nobody builds on the wrong word: the
"stubbed sanitizer" in `hive-sandbox` (`Deps::default()` in `main.rs`) is
invariant 12's `hive_sanitize` **trust** capability, which raises a
value's trust level under audit. It has nothing to do with HTML. The HTML
sanitizer below is new, and is a different thing with a different name
(`hive-webui::fragment`), so the two are never confused again.

## The decision

### 1. The shell's own mutating routes check a capability token, not a header

`require_htmx` goes. Every mutating route under `/ui/` requires an
`X-Hive-Cap` header whose value is

```
HMAC-SHA256(boot key, session id || method || route template)
```

minted into the page the shell renders, for exactly the routes that page
offers (`hx-headers` on the element that makes the call). The key is the
daemon's boot key; the session id is the cookie's server-side id; the
route is the template (`/ui/conversations/{id}/messages`), not the
concrete path, so one token serves a list. A token minted for one route
does not open another (invariant 14: the key carries every dimension the
decision depends on: who, which session, which verb, which route). What
it omits is the concrete id in the path, and that is safe because the
route handler still runs the predicate on that id through `Chat`; the
token proves the request came from a page the shell rendered for this
session, and the predicate still decides access.

The token is not a secret from the person's own browser, and it doesn't
need to be one: it is a secret from **other markup**, which is the
property that matters. A fragment from an app cannot learn it, because
§2 and §3 keep app markup out of the shell's document.

### 2. App UI renders in a sandboxed frame on an origin of its own

The default and preferred form. An install's UI is served from
`https://i-<install digest>.<apps domain>/`, never from the shell's
origin, and shown in

```html
<iframe sandbox="allow-scripts allow-forms" src="https://i-…/…?t=<frame token>">
```

- **A separate origin per install**, not one apps origin for all. An app
  that could script a sibling app's frame would be the D33 defect again,
  one layer up (invariant 14: the origin is a key, and it has to carry the
  install).
- **No `allow-same-origin`** in the sandbox. The frame is an opaque origin
  even to its own host, so it cannot read cookies or storage there either.
- **The frame token** is a short-lived, single-use token minted by the
  shell for (session, install), exchanged by the app origin for a
  frame-scoped credential that reaches **that install's routes and
  nothing else**. The shell's session cookie is never sent to an app
  origin and never valid there.
- **The shell talks to the frame by `postMessage`**, with the origin
  checked on both sides, and a protocol of a few message kinds (resize,
  navigate within the install, ask the shell to open something). Anything
  else is dropped.

This needs a wildcard DNS name and certificate for the apps domain, which
is an infra ask (below).

### 3. Where a frame is not available, an app fragment is sanitized and wrapped

A single-box install with no wildcard name, and compact surfaces (a card
on a dashboard) where a frame is too heavy, get inline fragments, under
three rules that all hold at once:

1. **The sanitizer is an allowlist** (`hive-webui::fragment`, built on an
   HTML5 parser, not a regex): structural and text elements only; no
   `script`, `style`, `iframe`, `object`, `embed`, `base`, `form`,
   `meta`, `link`; no attribute beginning `on`, `hx-` or `data-hx-`, no
   `style`, no `srcdoc`; `href` and `src` only as relative paths inside the
   install's own prefix, or `https:` to a host the install's egress grant
   names. Anything else is dropped, not escaped, and every drop is
   counted so a test can see it.
2. **Behaviour comes back only through a host-generated wrapper.** An app
   declares interactive points in its manifest (a route, a verb, a target)
   and marks them in its fragment with a plain `data-hive-action="<name>"`;
   the host emits the `hx-*` attributes for each declared action, bound to
   `/apps/<install>/…` and carrying a capability token for that route (§1,
   with the install as one more dimension). An action the manifest does
   not declare renders as inert markup.
3. **The swapped container carries `hx-disable`**, and the wrapper's own
   attributes sit on elements the host creates outside it. htmx does not
   process descendants of `hx-disable`, so anything the sanitizer missed
   is still inert. Defence in depth, measured as such: the test suite
   removes each layer in turn and asserts the other still holds (CLAUDE.md,
   *Check what the mutation removed*).

The rendered fragment is `untrusted` (invariants 9 and 12) whatever the app
says, and the shell never puts it anywhere instruction-shaped: not in a
title the agent reads, and not in a tool description.

### 4. htmx is configured for the shell, not for strangers

`htmx.config.allowEval = false`, `allowScriptTags = false`,
`selfRequestsOnly = true`, set in the shell's script before any swap. None
of these is the fix; each one shrinks what a mistake in §3 could do.

## Order

1. **§1 and §4, now.** Small, and they close the CSRF path for the shell's
   own routes whether or not app UI ever ships. Their tests: a request with
   `HX-Request: true` and no token is refused; a token for one route is
   refused on another; a token for one session is refused on another.
2. **§3's sanitizer** with an adversarial fixture set (every `hx-*` form,
   `on*` in every casing, `javascript:` and `data:` URLs, namespace and
   entity tricks), asserting **which** rule dropped each one.
3. **§2's frames**, when the apps domain exists.
4. **The first app fragment path**, only after 1 and 2 have landed. A PR
   that renders app markup into the shell before then is a bug.

## What lost

- *Trust `HX-Request` plus `SameSite=Strict`.* The hole above: it defends
  against other sites, and the attacker here is on our own page.
- *Strip `hx-*` and keep everything else.* A denylist against an
  interpreter whose attribute set grows each release; the allowlist is the
  only form that does not need to know htmx's future.
- *One shared apps origin.* Apps could script each other.
- *Shadow DOM for isolation.* It isolates styles, not behaviour or
  origin: the shadow content shares the shell's origin, cookies and
  script, so it changes what an app's markup looks like and not what it
  can do.
- *Abandon htmx.* The shell's own markup is ours, and D32's reasons for
  it still hold.

## Infra (asks to Pia, not done here)

- A wildcard DNS name and certificate for the apps domain
  (`*.apps.trh.beesroadhouse.com` or whatever she prefers), routed to the
  daemon, for §2.

## Open

- Whether a compact surface (§3) ever earns script, or stays declarative
  forever (assumed: declarative).
- The `postMessage` protocol's exact message set, decided with the first
  framed app.
