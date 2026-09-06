# The surfaces: MCP tools and app routes

A manifest declares tools and routes; the registry derives the surface an
install exposes; `crates/hive-surfaces` serves it. Two endpoints, one router,
so both are reachable over the unix socket as well as the port (invariant 13).

## `POST /mcp`

Stateless JSON-RPC 2.0. No session, no server-to-client stream, no GET. The
credential is the platform's, presented the way every request presents it
(header, cookie or query), and a missing one gets the one 401 body.

| method | answer |
|---|---|
| `initialize` | protocol version, `tools` capability, server name and version |
| `notifications/initialized` (no id) | `202 Accepted`, empty |
| `ping` | `{}` |
| `tools/list` | every tool of every active install in the caller's scope that the predicate allows, sorted, named `<app>.<tool>` |
| `tools/call` | `{content: [{type: text}], structuredContent?, isError, _meta: {trust, taintedBy}}` |

Two rules from `hive-mcp` show on the wire:

- **List and call agree by construction.** Both walk the same candidate set and
  ask the same predicate, so a tool that is listed can be called and one that
  cannot be called is not listed. A grant revoked between the two bites on the
  call.
- **Not yours and does not exist are one answer**: `-32602` with "unknown tool".
  A tool that ran and said no is a *result* with `isError: true`, which is how
  MCP separates "your call failed" from "the protocol failed".

`_meta.trust` carries the invocation's taint (invariant 12). A client feeding a
result back to a model can see what it is feeding.

## `/apps/{app}/...`

Any method. The path after the app is matched against the install's derived
routes: exact segments first, `{name}` captures second, so `/notes/recent` beats
`/notes/{id}` whatever the declaration order. The host owns the `/apps/` prefix
(D2.3); a manifest cannot mount over a platform endpoint.

- A generated CRUD route maps to the data layer: the `{id}` from the path,
  `limit` and `cursor` from the query string, the document from the body.
  Collection grants are checked inside the data layer, nowhere else.
- A guest route hands the function one JSON object: `method`, `path`, `params`,
  `query`, `body` (parsed when it is JSON, a string when it is not).
- The answer is JSON with `x-hive-trust: trusted|untrusted`.

Every miss is `404 {"error":"not_found"}`: unknown app, unknown route, method
the manifest did not mount, or not yours. `400` is the caller's mistake on a
route they may call; `502 {"error":"app_failed"}` is the app's own failure, with
its message.

## Who may call what

Both surfaces go through the store's predicate and nothing else.

- **Candidates** are every active install the principal owns or holds any live
  grant on. This set is deliberately not a permission check; the predicate is.
- **Tools** use `tool_access_reason` (D18.1): an install grant with no tool
  allowlist means the whole tool set; with one, exactly those tools.
- **Routes** use `route_access_reason` (migration 0004), the same rule with
  routes named `"<METHOD> <path template>"`. A route grant opens that route and
  nothing else; a tool grant opens nothing on the HTTP side.
- The owner of an install reaches everything on it without a grant row.

## The module bytes

The dispatcher reads wasm by content address. The address comes from the build
row of an install the predicate just authorised, never from a caller, and the
type that reads it is private to the surfaces crate. That is why a hash is still
not a read capability here (invariant 3).

## Tests

`crates/hive-surfaces/tests/surfaces.rs` installs the reference guest the way
the daemon would and asks both surfaces as the owner, a stranger, a tool
grantee and a route grantee, then disables the install and asks again.
`crates/hive-httpapi/tests/mcp.rs` covers the wire shapes with fakes.
`crates/hive-sandbox/tests/mcp_socket.rs` speaks JSON-RPC over the unix socket.
