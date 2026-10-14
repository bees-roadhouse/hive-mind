# D57: identity is a kit interface: a host app delegates its people, sign-in, sessions and tokens to the kit over one `identity` surface; Keycloak is the household's issuer behind D50's front door and the built-in issuer serves a standalone host; a client that cannot do OIDC signs in with a per-device app password issued after a real sign-in, never with the account password, so the password grant stays refused and the second factor stays mandatory

**Decided** 2026-10-14 by Nate, relayed by Pia from Forage: "we need
hive-identity to be a thing then too we provide as a standard interface."
And, in session: "oh are apps identities in our identity system? nectar is
going to use this framework as you probably already understand.. we could
just use hive-mind's identity system as a resource instead.. which will be
connected to our keycloak obviously." Shape by Propolis. Builds on D50
(OIDC is the only way a human signs in; the built-in issuer; a mandatory
second factor with no setting to turn it off; the identity token exchanged
once for a device credential; step-up; AI actors never sign in), D53 (an
app's install credential), D55 (a host install, its profile, the scoped
tokens a host issues to plugins), D56 (nectar as the reference consumer),
D43 §1 (the registry's `identities` and `credential_index`), D45 §5
(bounded offline sessions) and invariant 2. **Amends D55 §1** in one
place: the "scoped tokens the host issues to plugins" become a kit call,
because a host minting tokens of its own would be a second credential
kind. Amends nothing in D50; §4 below is written so it does not have to.

## What was checked before deciding

- **Apps are already identities.** Every actor that acts is an actor row:
  human, AI, org, and since D55 an app acting for the org or person that
  installed it. An app never signs in; it holds an install credential
  minted when its container starts and revoked when it stops (D53 §1),
  and every row it writes carries the app as author and the installer as
  owner (invariant 2). Nothing here changes that; it is the fact the
  question was asking about.
- **nectar carries its own identity tables today** (`api/migrations`, read
  2026-10-14): `users`, `user_identities`, `sessions`, `api_keys`,
  `oidc_auth_codes`, beside `user_library_access` and `user_preferences`.
  Its Jellyfin-compatible routes (`ts/server/src/routes/jellyfin/`) accept
  `MediaBrowser Token="…"` in the `Authorization` or `X-Emby-Authorization`
  header, answer `/Users/AuthenticateByName` with a username and password,
  and report Quick Connect as disabled.
- **D50 §2 refuses the resource-owner password grant** on the built-in
  issuer and **§3 makes the second factor mandatory with no off-switch**.
  A Jellyfin client sends a username and a password and nothing else, so
  "nectar delegates sign-in to the kit" cannot mean "the kit accepts a
  password"; §4 is the resolution and it keeps both rules.
- **D50 §2 already has the device authorization grant** (RFC 8628) for a
  CLI or a channel in a terminal. A television or a set-top client is the
  same shape.
- **Keycloak** is the household's identity server (D32 §6 named it in
  passing; D50 §1 lists it first among an org's own providers). A Keycloak
  realm publishes a discovery document, issues `amr`/`acr`, and supports
  the device grant, so it fits D50 §1 and §3 without a special case.

## The decision

### 1. One `identity` surface, on the API every client uses

A host install (D55 §1) reaches the kit's identities the way it reaches
its storage: through the one API, under its install credential, with
`Guard` deciding (invariant 1). The surface, all of it existing machinery
given a host-facing shape:

| call | what | who may |
|---|---|---|
| `identity.whoami` | the credential's actor, principal, identity, issuer, the factors that stood behind it, its device label and expiry | any credential, about itself |
| `identity.members` | the people in the org and their org roles (D43's `org_members`) | a credential in the org; names only, no contact details without a grant |
| `identity.sessions` / `identity.sessions.revoke` | the device credentials of the current person, and revocation (D50 §4's devices page, as an API) | the person, or an org admin for a member |
| `identity.sign_in.start` | begins D50 §1's authorization code flow for the host's client: the kit is the relying party, the host supplies only a return address on its per-install origin; the flow ends at the kit's `/credentials` and the host's client receives a **device credential pinned to the host's org actor**, never an ID token | the host install, on behalf of a client it is serving |
| `identity.device.start` / `identity.device.poll` | D50 §2's device grant for a client without a browser: a code shown on the client, approved on an enrolled device with a second factor, a device credential handed back | the host install |
| `identity.app_passwords.*` | §4 | the person, after step-up |
| `identity.credentials.scope` | a credential narrowed to one plugin install and a grant set, derived from the caller's, for a plugin to call back into the host (D55 §1's scoped tokens, moved here) | the host install, for its own plugins |
| `identity.step_up` | D50 §4's fresh second factor, as a call the host's client makes before an action the host marks sensitive | the person |

- **No new credential kind.** Everything the surface hands out is a D50
  §4 device credential or an install credential: a bearer token whose
  hash is a row pinning `(actor, principal)` in the org's database,
  revocable there. A host never mints a token of its own, which is the
  amendment to D55 §1.
- **No host reads another host's credentials**, and no host reads a
  credential's hash at all. `identity.sessions` returns labels, issuers,
  factors and dates; the token exists only in the client that received it.
- **Authorization data stays the host's.** Which libraries a person may
  see is nectar's `user_library_access`, keyed on the kit's actor id, and
  nectar decides it; the kit decides who the person is and which org they
  are in. The line is D55's: identity is the kit's, domain is the host's.

### 2. Keycloak is the household's issuer; the built-in issuer serves a standalone host

- **The household's instance registers Keycloak** as the org's identity
  provider exactly as D50 §1 says: issuer URL, client id, the client
  secret as a D48 vault entry, the claims mapping, and the MFA policy of
  D50 §3 (the realm's `amr` or `acr`, or `asserted-by-policy` on the
  record). nectar's users become identities on that registration, matched
  by `(issuer, subject)`; nectar's `users` table goes with the migration
  nectar schedules (D56 §1's slices), its rows mapped once by e-mail or
  username to Keycloak subjects by an admin, never guessed.
- **A standalone host** (D55 §1's sidecar form, someone's self-hosted
  nectar) signs in through the kit's built-in issuer (D50 §2) and sees no
  difference: one sign-in code path, as D50 promised, which is exactly why
  the host delegates rather than carrying a login of its own.
- **An org that requires its issuer** (D50 §6) applies to the host's
  clients as to any other: a sign-in from the wrong issuer is refused with
  the reason naming the one it wants.

### 3. The host's clients see the kit, not a login form

- The host's own client (nectar's `web-solid`) calls
  `identity.sign_in.start`, follows the authorization URL to the issuer,
  and returns to the kit, which hands it a device credential for the host's
  org. The client stores that credential and nothing else; the ID token
  never reaches the host.
- The host's API accepts the kit's credential on its routes by resolving it
  through `identity.whoami`, cached for the credential's lifetime and
  invalidated by the kit's revocation event (D47) on the bus, so a revoked
  device stops within one event delivery rather than at a cache's expiry.
- A plugin (D55 §2) calling back into the host presents the scoped
  credential `identity.credentials.scope` produced; the host resolves it
  the same way and sees the plugin's install and the grant set, never the
  person's full standing.

### 4. A client that cannot do OIDC uses an app password; the account password is never accepted

The Jellyfin-compatible surface is the case that would break D50, so it
gets its own rule rather than an exception:

- **An app password** is a device credential (D50 §4) in a form a legacy
  client can carry: a random secret shown once, created by the person on
  the host's devices page **after step-up** (D50 §4), labelled with the
  client it is for, expiring on the person's schedule, revocable like any
  session, listed with every other session. It pins the person's actor and
  principal like every credential; what the client does with it is bounded
  by the person's grants, exactly as a browser session is.
- **`/Users/AuthenticateByName` takes the username and an app password.**
  The host calls `identity.app_passwords.verify`; a match returns the
  device credential the app password is, which the host returns as the
  `MediaBrowser` token. The account password, the one Keycloak or the
  built-in issuer knows, is **never** accepted on that route: a person who
  types it gets a refusal naming app passwords and where to make one. The
  issuer's password grant stays refused (D50 §2) because nothing here
  calls it.
- **Why this keeps D50 §3.** The second factor gates the *issuance* of the
  app password (step-up on an enrolled, signed-in device), not each use of
  it, which is the same shape as a device credential surviving for the D45
  §5 session lifetime after one sign-in. A stolen app password is a stolen
  session: revocable, listed, scoped to the person's grants, and never a
  way to enrol a new device or pass step-up.
- **Quick Connect** on the Jellyfin surface maps onto `identity.device.*`:
  the client shows a code, the person approves it on a device where they
  are signed in, and the client receives a device credential. nectar's
  `/QuickConnect/Enabled` answers true once this lands.
- **`MediaBrowser Token="…"`** on every later request is the kit's
  credential, resolved as §3 says.

### 5. AI actors, apps and the surface

- An AI actor cannot call `identity.sign_in.start`, `device.*`,
  `app_passwords.*` or `step_up` (D50 §5: it has no identity and no
  factors); `whoami` answers for it, as for any credential.
- An app's install credential may call `credentials.scope` for its own
  plugins and `whoami`; `members` and `sessions` need a grant the person
  or org makes to the install, absent is deny.
- A host's profile (D55 §1) lists `identity` among its surfaces; a host
  with it off gets the refusal at the route.

## Acceptance scenarios, checked in before the code

1. **A host sign-in ends in the kit's credential.** Through a test issuer,
   `identity.sign_in.start` from a host install leads to a device
   credential pinned to the host's org actor; the host's client never
   receives the ID token (asserted from the client's record of what it
   stored).
2. **Keycloak's MFA evidence is required.** A test issuer configured as an
   external provider returning a token without `amr`/`acr` is refused with
   the message D50 §3 names; with `amr: ["mfa"]` it succeeds; with the
   `asserted-by-policy` declaration the sign-in succeeds and the audit row
   says so.
3. **The account password is refused on the legacy route.** With a
   person's issuer password in hand, `app_passwords.verify` refuses with
   the message naming app passwords; `AuthenticateByName` through the
   test host returns the same refusal.
4. **An app password is a session.** Creating one requires step-up
   (refused without it, with D50 §4's message); it appears in
   `identity.sessions` with its label; verifying it yields a credential
   whose `whoami` pins the person; revoking it makes the next verify the
   one 401; it cannot be used to create another, to enrol a device or to
   pass step-up.
5. **Device grant for a client without a browser.** `device.start` yields
   a code; `device.poll` is pending until an enrolled device approves with
   a second factor; then it yields a credential; an unapproved code expires
   with the message.
6. **A scoped credential is narrower, never wider.** `credentials.scope`
   from a host's install credential for a plugin install yields a
   credential that reaches the named grant set and nothing else; scoping to
   a grant the host itself lacks is refused with the grant message.
7. **Revocation reaches the host within one event.** Revoking a session
   through the kit emits the event; the test host's cached `whoami` is
   invalidated on delivery and its next request with that credential is
   refused.
8. **AI actors have no front door.** An AI credential calling
   `sign_in.start`, `device.start`, `app_passwords.create` or `step_up` is
   refused with the message D50 §5 implies; `whoami` answers.

## Order

1. D50's slices as D50 orders them (the built-in issuer, factors, the
   exchange at `/credentials`), since this surface is a shape over them.
2. **`identity.whoami`, `members`, `sessions`, `credentials.scope`** over
   what exists. Scenarios 6, 8.
3. **`sign_in.start` and the host return** (scenario 1), with Keycloak
   registered on the household's instance by Pia and the test issuer in
   CI (scenario 2).
4. **`device.*`** (scenario 5) and **`app_passwords.*`** (scenarios 3, 4).
5. **The revocation event and the host's cache** (scenario 7).
6. nectar's `users`, `user_identities`, `sessions`, `api_keys` and
   `oidc_auth_codes` migrate onto it in nectar's own slices (D56 §1);
   `AuthenticateByName` and Quick Connect switch when `app_passwords` and
   `device.*` exist.

Behind D43 slice 3's remaining crates and the D53 runtime, as everything
in D55 and D56 is.

## What lost

- *A host keeping its own users and sessions beside the kit's.* Two
  identity systems for one person, two places to revoke, and the second
  one without a second factor.
- *A password grant for legacy clients.* D50 §2 refuses it and §3 would
  have no meaning if it existed; app passwords give the legacy client a
  secret that is a session, not the account.
- *A per-host MFA exemption.* D50 §3 has no off-switch and this record
  does not add one; a client that cannot pass a second factor gets a
  secret issued after one.
- *Host-minted tokens for plugins* (D55 §1 as written). A second
  credential kind; `credentials.scope` is the same kind, narrowed.
- *The kit deciding library access.* Domain authorization is the host's
  (D55); the kit says who, the host says what.
- *Accepting the ID token on the host's routes.* D50 §4's reasons hold for
  a host as for the daemon's own API.

## Open

- The exact `members` payload a host may see without a grant (names and
  roles assumed; anything else behind a grant).
- App password lifetime defaults and whether an org may cap them.
- Whether `identity.sign_in.start` returns a URL the host's client
  redirects to, or the kit serves the sign-in page itself on the per-
  install origin; the first is assumed, the second is what a host without
  a client of its own would want.
- Mapping nectar's existing users to Keycloak subjects: an admin tool in
  nectar's migration slice, not designed here.
- Keycloak's own configuration (realm, client, MFA policy) is Pia's, on the
  cluster, when D50's slices reach it.
