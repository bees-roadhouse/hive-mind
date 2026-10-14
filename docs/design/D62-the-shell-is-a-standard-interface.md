# D62: the shell is a standard interface apps build against: named surfaces (navigation, identity, notifications, search, settings) that an app fills with declared views the shell renders in the house look, or with a fully custom UI mounted full-bleed on its own origin inside the shell's chrome; an app may run standalone with its own UI or integrated inside the shell, and the same manifest says which

**Decided** 2026-10-14 by Nate, relayed by Pia from Forage: "nectar also
needs a standalone mode or "hive-mind" integrated mode.. one is its own UI
basically and the other one expands out and embeds itself as an App in
hive-minds built-in interface.. also message prop, the hive-user-iterface
needs to be a standard interface to build against.. while also completely
support custom ones entirely". Shape by Propolis. Builds on D45 (one
Solid.js client, APIs-only daemon), D44 (app UI on a per-install origin in
a sandboxed frame; the shell never runs an app's markup), D55 §3 (slots: a
declared schema or a frame, validated at install), D57 (identity as a kit
interface), D47 (bus items as notifications), D61 (the `resource` schema
element) and the Brand & Visual Identity Standard (KB page 2451: charcoal
and gold, the `--brh-*` tokens, dark only). **Generalises D55 §3** from "a
slot inside a host's page" to the shell's own surfaces and whole views, and
**amends D55 §1 and §5**: a host may be a standalone host or an integrated
app, chosen per install.

One premise corrected on the way: the client this record maps onto is
D45's Solid.js shell. The Slint native client assigned on 2026-10-10 was
deferred by Nate the same day ("keep solid.js for now") and D45 stands; if
a native shell ever comes, it renders the same declared views and the
custom-UI path would need a webview, which is one more reason declared
views are the default.

## What was checked before deciding

- **D44 already gives every install an origin** and a frame on it, with a
  token exchange at `/.hive/…`, and says the shell never runs an app's
  markup. D55 §3 already has two slot kinds, `schema` and `frame`, and a
  validator at install. Nothing here needs a third mechanism; the shell
  has to say which of its surfaces take which kind.
- **nectar's client is a complete Solid.js application** (`web-solid`)
  with its own navigation, its own settings and its own identity screens;
  today it is served by nectar's Bun server at nectar's URL. Nate wants it
  to keep working that way and also to live inside the kit's shell.
- **The brand standard** is one page and one token set; every Roadhouse
  surface follows it, and the shell is where it is enforced for declared
  views. A custom UI inside a frame keeps its own look, which D44 already
  permits, and the chrome around it is the shell's.

## The decision

### 1. The shell's surfaces are named, and an app fills them

The shell (D45's client) exposes **surfaces**, each a named place an app
may contribute to, each with the kinds it accepts:

| surface | what it is | kinds |
|---|---|---|
| `nav` | an entry in the shell's navigation: name, icon, route | declaration only |
| `view` | a whole page under the app's nav entry | `schema` or `frame` |
| `panel` | a region inside a shell page (an item's side panel, a dashboard tile) | `schema` or `frame` (D55 §3's slot, unchanged) |
| `action` | a verb offered on an item or a list the shell owns | `schema` |
| `search` | results the app contributes to the shell's search | `schema` |
| `notification` | an item on the bus addressed to the person, rendered by the shell | the bus item (D47), no app markup |
| `settings` | the app's section of the shell's settings, including its AI resources (D61) | `schema` |

- The manifest's `ui` block (D55 §3) names the surface, the kind, and the
  tool or route behind it; `hive-manifest` validates the surface and the
  kind at install (D24: decided at install) and refuses with the message
  otherwise. A host (D55) may add surfaces of its own to this table for
  its plugins, under its own names, and the validator knows the union.
- **A declared view** is the vocabulary D55 §3 names, extended to a page:
  layout (stack, split, grid), text, list, table, card, form, action,
  link, the D61 `resource` element, and an `embed` that places a `frame`
  inside a declared page. The shell renders it in the house look from the
  `--brh-*` tokens, offline-capable through D45's replica, and every
  `action` is a tool call into the app's process. The vocabulary is
  versioned; an app declares the version it wrote against and the shell
  renders every version it knows.
- **A custom UI** is a `frame` on a `view`: the app's own routes serve its
  HTML on its per-install origin (D44), the shell mounts it **full-bleed**
  under the app's nav entry with the shell's chrome around it (the nav,
  the identity menu, the notification tray), and the two talk through
  `postMessage` with a declared schema: the shell tells the frame the
  theme tokens, the person's display name and the current route; the
  frame asks the shell to navigate, to show a notification, or to open the
  settings. Nothing else crosses. The custom UI keeps its own look inside
  the frame; D44's reasons for the frame stand (the shell's session,
  cookie and origin are not the frame's).
- **What the shell promises every app**, declared or custom: a nav entry,
  the identity (D57's `whoami` through the shell, never the credential
  itself to a frame), notifications on the bus, events, the theme tokens,
  and the settings section. **What no app gets**: the shell's session or
  its origin, a pixel outside its frame or its surface, another app's
  surfaces, or markup that runs as the shell.

### 2. Standalone or integrated, per install

An app's manifest may offer both modes; the install chooses:

- **Standalone**: the app serves its own UI at its own URL, from its own
  routes on its per-install origin (or, for a host running without the
  kit's shell, its own server, which is D55 §1's host form). The kit's
  shell is not involved; identity still comes through D57 if the app is a
  kit app, and the app's own sign-in screen is D57's `sign_in.start`.
- **Integrated**: the app is installed with a nav entry and its `ui`
  block, and renders inside the shell under §1. Its custom UI, if it has
  one, is the full-bleed frame; its declared views fill the surfaces it
  named; its notifications arrive in the shell's tray.
- **Both at once** is allowed and is nectar's case: the same install
  reachable at nectar's URL standalone and under the shell's nav entry
  integrated, with one identity and one data store behind both.

### 3. nectar

- **Standalone** is today: `web-solid` at nectar's URL, served by nectar
  (D55 §1's host form, or its routes on the per-install origin once it is
  a kit app per D56). Identity moves to D57 in nectar's own slices.
- **Integrated**: nectar registers a nav entry (`Media`), a `view` of kind
  `frame` that mounts `web-solid` full-bleed, `search` and `notification`
  surfaces fed from its library and its events, and a `settings` section
  that includes its AI resources (D61) and its plugin slots (D55 §3).
  `web-solid` learns the `postMessage` schema (theme, identity, route,
  navigate) and nothing else changes in it.
- The two modes share the install, the store and the identity; a person
  opening nectar either way is the same person with the same libraries.

### 4. The house look

Declared views are rendered by the shell, so they follow the brand page
by construction: the shell's stylesheet is the only one that touches
them, and it is the page's tokens. A custom UI inside its frame receives
the tokens through the `postMessage` schema and may adopt them; nectar's
client already follows the page (its own 2026-10-13 palette change), so
the seam is invisible there. A custom UI that ignores the tokens is
allowed and looks like itself inside the shell's chrome, which is what
"completely support custom ones entirely" means.

## Acceptance scenarios, checked in before the code

1. **A declared view renders in the shell.** The test app's `view` of kind
   `schema` appears under its nav entry, every element in the vocabulary
   renders, an `action` reaches the app's tool through the daemon, and no
   stylesheet but the shell's applies (asserted by the e2e suite reading
   computed styles against the token values).
2. **A custom view is full-bleed and contained.** The test app's `view` of
   kind `frame` fills the content area under the shell's chrome; a
   `postMessage` outside the declared schema is dropped and counted; the
   frame cannot read the shell's cookie (D44's test, re-run under this
   mounting).
3. **Surfaces are validated at install.** A manifest naming a surface the
   shell lacks, or a `frame` on `search`, is refused at install with the
   message naming the surface and kind.
4. **The vocabulary is versioned.** A view declaring a version the shell
   does not know is refused at install with the message; one declaring an
   older version renders.
5. **Standalone and integrated are one install.** The test app opened at
   its own URL and under the shell's nav entry sees the same `whoami` and
   the same rows (asserted through both paths in one test).
6. **Notifications are the shell's.** An item the app puts on the bus for
   the person appears in the shell's tray with the app named; the app
   cannot render a notification of its own markup.
7. **The shell promises the identity, not the credential.** The frame
   receives the person's display name through the schema; the credential
   never appears in any message to the frame (asserted by the test frame
   recording every message it received).
8. **nectar integrated.** nectar's checked-in integrated manifest mounts
   `web-solid` under `Media`, its search results appear in the shell's
   search, and playback works inside the frame through D58's stream
   routes (run in nectar's CI against a hive-mind image).

## Order

1. **The surfaces table and the `ui` block's extension** in
   `hive-manifest`, with the validator (scenario 3, 4), beside D55's slot
   work.
2. **Declared views in the shell**: the vocabulary at version 1, the
   renderer, the `action` path. Scenario 1.
3. **The full-bleed frame and the `postMessage` schema.** Scenarios 2, 7.
4. **Notifications and settings surfaces** over D47 and D61. Scenario 6.
5. **Modes per install.** Scenario 5.
6. **nectar integrated** (D56 §1's slices on nectar's side). Scenario 8.

After the D53 runtime and D55's slot work, which this generalises.

## What lost

- *Apps patching the shell's DOM or routes.* D44's hole; every app's
  markup stays in its frame or in a declared view the shell renders.
- *A custom UI as the only path.* Nate asked for a standard interface to
  build against first; declared views are it, and they work offline and
  look like the house without the app doing anything.
- *A declared view as the only path.* "Completely support custom ones
  entirely": a whole Solid application is a `frame` on a `view`, and that
  is nectar.
- *A second shell for integrated apps.* One shell, one nav, one tray, one
  identity menu; an app adds to it and never replaces it.
- *Designing for a native shell that does not exist.* D45 stands; the
  record maps onto the browser shell and says what a native one would
  inherit.

## Open

- The vocabulary at version 1: written with nectar's and the journal's
  views in front of it, not before.
- Deep links: a shell route into an app's declared view or custom frame,
  and back; wanted for notifications, not designed here.
- Keyboard and focus handling across the shell and a full-bleed frame.
- Whether a host (D55) may restyle declared views for its plugins beyond
  the tokens; assumed no.
