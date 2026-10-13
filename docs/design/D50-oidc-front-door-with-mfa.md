# D50: OIDC is the front door; the daemon is an OIDC issuer of its own when there is no identity server; a second factor is mandatory everywhere; an identity token is exchanged once for a device credential and never used as one

**Decided** 2026-10-13 by Nate: "getting sign into your hive.. OIDC should be
forefront, even local against itself just in case someone doesn't have an
identity server. MFA is a must." Shape by Propolis. Builds on the credential
model that exists (invariant 2: a credential pins `author_actor` and the
owner principal; `hive-httpauth`'s one 401 shape; `/credentials` enrolling a
device from a credential; `/session` moving a bearer into an HttpOnly cookie),
on D29 (accounts are records with a trust domain; the token is never in the
record), D43 §1 (the registry's `identities` and `credential_index`), D45 §5
(bounded offline sessions, revocation on next contact, a passkey wrapping
the local key), D46 §2a (device approval) and D48 (the vault holds the
issuer's keys). D32 §6 said "Keycloak over OIDC" as the intended front door in
passing; this record makes it a decision and widens it.

## What exists, and what is missing

Today a credential is a bearer token whose hash is a row pinning an actor
and a principal. A device gets one by enrolling from a credential it already
holds, a browser swaps one for a cookie, and the very first credential comes
from `HIVE_SANDBOX_BOOTSTRAP_TOKEN` at first boot. There is **no way for a
human to sign in**: no login, no identity provider, no second factor. The
first credential is an environment variable and every later one descends
from it. That is fine for a daemon under test and not for a platform people
sign in to.

What stays: the credential as the thing the platform acts on. Every
predicate, trigger and RLS policy decides on `(actor, principal)` from a
credential row in the org's database, and nothing in this record changes
that. **OIDC is how a human proves who they are; a credential is what the
platform acts on.** The two meet exactly once, at enrollment.

## The decision

### 1. OIDC, and only OIDC, is how a human signs in

The client (D45) signs a person in through an OpenID Connect authorization
code flow with PKCE, against an **issuer**. There is no other login form.
Which issuer:

- **An org's own identity provider**, when it has one: Keycloak, Authentik,
  Entra ID, Google Workspace, anything that publishes a discovery document.
  An org admin registers it (issuer URL, client id, the client secret as a
  D48 vault entry, the claims mapping, and the MFA policy in §3). The
  daemon is a relying party.
- **The daemon's own issuer**, when it has none (§2).

The registry (D43 §1) maps `(issuer, subject)` to an **identity**, and an
identity to its home org and to the human actor in each org it belongs to.
A person may hold several `(issuer, subject)` pairs on one identity (their
org's Entra and the household's built-in issuer), each linked by proving
both at once (§6). An org may require that its members sign in through
**its** issuer, and that requirement is recorded on the org, not assumed.

### 2. The daemon is an OIDC issuer when nobody else is

A household, a single box, a small org: none of these should need to run
Keycloak to sign in. So the daemon carries a **built-in OpenID provider**,
served under the instance's own origin (`/oidc/…`: discovery, JWKS,
authorize, token, userinfo, revocation, device authorization), as a daemon
role (`--serve-idp`, defaulting on like the other roles, D7). It is a real
issuer, not a shortcut: the client talks to it exactly as it talks to
Keycloak, so **one sign-in code path exists** whether the issuer is
built-in or external, and a deployment that later adds an identity server
changes a registration, not the client.

What it implements: the authorization code grant with PKCE (the only
browser grant), the device authorization grant (RFC 8628) for a CLI or a
`hive-channel` sitting in a terminal, refresh tokens with rotation and
reuse detection, `prompt=login` and `max_age` for step-up (§4), discovery
and JWKS with key rotation. What it refuses: the implicit grant, the
resource-owner password grant, any client without PKCE.

Its signing keys are D48 vault entries at the daemon tier, rotated on a
schedule with the previous key kept in the JWKS until every token signed by
it has expired. Its users are the identities in the registry; its first
factor is a password (argon2id, a length floor and a breached-list check,
no composition rules) **or no password at all** when the person enrols a
passkey as their only factor (§3 says what that means for the second
factor). Its account state (locked, must-change, factors enrolled) lives
with the identity in the registry, because sign-in has to work before any
org database is open (D43 §1's reasoning for `identities`).

**First boot changes.** The bootstrap token keeps minting the root
credential for the operator's tooling, but the first *person* no longer
descends from it: first boot writes a one-time enrolment URL to the log,
valid for an hour, and the person who opens it creates the first identity
and enrols their factors (§3) before anything else is reachable. The
bootstrap token stays what it is, an operator's credential for automation,
and is expected to be rotated once a person exists.

### 3. A second factor is mandatory, everywhere, and there is no setting to turn it off

Nate: "MFA is a must." Built as a property of the platform, not a policy an
admin may relax:

- **On the built-in issuer** a sign-in completes only with two factors, or
  with one phishing-resistant factor that is itself two things (a passkey
  bound to a device and unlocked by the device's biometric or PIN counts as
  possession plus inherence, which is what WebAuthn's user verification
  flag asserts, and the issuer requires that flag). In order of preference,
  shown in that order at enrolment: **a passkey** (WebAuthn, platform or
  roaming, user verification required), **TOTP** as the second factor
  beside a password, and **recovery codes**, generated once, shown once,
  stored hashed, each usable once. Never SMS, never e-mail codes: both are
  the factor the attacker already has.
- **Enrolment is before use.** An identity with no second factor cannot
  finish its first sign-in; the flow ends at the factor enrolment screen
  and nowhere else. There is no grace period and no "remind me later".
- **On an external issuer**, the daemon requires evidence that the IdP
  performed MFA for this sign-in: the `amr` claim containing a multi-factor
  method (`mfa`, `hwk`, `otp`, `swk` and the like) or an `acr` value the org
  admin mapped to "multi-factor" at registration. A sign-in whose token
  carries neither is **refused with that reason**, and the person sees
  "your identity provider did not confirm a second factor", not a generic
  failure. An org admin may declare, at registration and on the record, that
  their IdP enforces MFA by policy and emits no claim for it; the daemon
  then logs every such sign-in as `mfa: asserted-by-policy` so the gap is
  visible in the audit rather than invisible in a setting. There is no
  declaration that turns the requirement off.
- **Recovery is a human act.** Losing every factor means an org admin (or,
  for a personal org, the recovery codes) re-enrols the person after
  verifying them out of band; the admin's act is an event naming both. The
  platform never resets a factor on its own, and never through e-mail.

### 4. The identity token is exchanged once; the device credential is what works

After a successful sign-in the client holds an ID token from the issuer. It
is **not** a credential and no API accepts it as one. The client presents
it, once, to `/credentials` (the enrollment route that exists), and the
daemon:

1. verifies the token against the issuer's JWKS (issuer, audience, nonce,
   expiry, and the MFA evidence of §3);
2. resolves `(issuer, subject)` to an identity in the registry, or, on a
   first sign-in through an issuer the identity has not used, creates the
   identity (built-in issuer) or refuses until an admin maps it (external
   issuer, unless the org allows just-in-time creation for its own issuer);
3. finds or creates the **human actor** for that identity in the org;
4. mints a **device credential**: a bearer token (hashed as today) whose row
   pins `(actor, principal)`, labelled with the device, with the D45 §5
   lifetime, and records which issuer and which factors stood behind it.

From there everything is as it is today: the browser swaps the token for the
HttpOnly cookie, SSE and the API use it, the predicate decides on it, and
revocation is a row in the org's database, visible to the person on their
devices page. Why the exchange rather than accepting IdP tokens on every
request: revocation is local and immediate instead of waiting for a token
to expire; an identity provider's outage does not log out every device
(D45 §5's bounded offline session needs exactly this); the predicate keeps
one credential kind instead of two; and a stolen ID token is worth one
enrolment attempt, which is audited, rather than open access until expiry.

**Step-up.** Some actions need a fresh second factor however old the
session: revealing a vault entry (D48 §1), approving a new device or
rotating keys (D46 §2a), registering or changing an identity provider,
granting admin, and the share step that moves private data into an org
(D46 §1). The client re-runs the OIDC flow with `max_age=300` and
`prompt=login`, the issuer re-verifies the second factor, and the resulting
token is presented alongside the request. The daemon checks `auth_time`
and the MFA evidence on that token, not the age of the device credential.
The predicate decides whether the action is permitted; step-up decides
whether the person is present. Both, never one.

### 5. AI actors never sign in

An AI actor (D26 §4) has no identity, no factors and no issuer. Its
credentials are minted per run by the chat worker or the harness from the
person's credential (D35 §5, D47 §1), pinned to the AI actor acting for
that principal, and expire with the run or the session. `tools/list` for
an AI credential shows no sign-in surface. This keeps invariant 2 honest
at the front door: "an AI acting for Nate" can never present itself as
Nate, because the only way to become Nate is a second factor an AI does
not hold.

The same applies to the bootstrap token and to any operator automation: it
is a credential, not a person, and the audit says so.

### 6. Identities across orgs and issuers

- One identity, several orgs (D43 §1): a person's home org and the orgs
  they were added to. Signing in once produces, after enrolment, a device
  credential **per org** the person belongs to, because a credential row
  lives in an org's database and pins that org's actor. The client holds
  them side by side (D46 §5's workspace), and one sign-in is still one
  sign-in.
- One identity, several issuers: linked by signing in through both within
  one step-up window, from a device already enrolled under the first. The
  link is an event on the identity; unlinking requires the same.
- An org that requires its own issuer refuses enrolment of its actor from
  any other, with the reason naming the issuer it wants.
- D29's trust domains map onto this as orgs: a person whose work org and
  household are both here has two orgs, two issuers if the work org
  demands it, two sets of credentials, and one client. D29's warning about
  an *account* that spans domains is a statement about Claude.ai accounts
  and stands unchanged.

### 7. What the client shows

A sign-in screen that is a list of issuers the instance knows for the
e-mail the person typed (one, usually), the factor enrolment flow, a
devices page (every device credential, its issuer, its factors, last seen,
revoke), a security page (factors, recovery codes regenerated with step-up,
linked issuers), and the admin's identity-provider page (register, test,
the MFA mapping, the required-issuer flag). All of it through the API; the
daemon renders no HTML (D45 §1), and the built-in issuer's own pages
(authorize, consent for a third-party client, device-code entry) are the
one exception, because an OIDC authorize endpoint has to render something
to a browser that has not loaded the client yet. Those pages are static,
scriptless where possible, and under the same CSP.

## Criteria for the picks, which outlive them

- **Relying party**: a maintained Rust OIDC client that does discovery,
  PKCE, nonce and JWKS verification without a native library; the
  `openidconnect` crate is the candidate.
- **Provider**: there is no Rust OpenID provider crate the record would
  stake itself on, so the provider surface is written here on axum, over
  a JOSE crate for JWS and JWK (candidates `josekit`, `jsonwebtoken`),
  `webauthn-rs` for passkeys, a TOTP crate, and argon2id. The surface is
  small (seven endpoints) and the alternative, bundling Keycloak, fails
  "someone doesn't have an identity server" by becoming one they have to
  run.
- **Conformance is tested, not claimed**: the e2e suite signs in through
  the built-in issuer with a passkey (Playwright's virtual authenticator)
  and through a containerised Keycloak as the external case, so both
  branches of §1 run on every build.

## Acceptance scenarios, checked in before the code

1. **No factor, no entry.** An identity with a password and no second
   factor cannot obtain a device credential; the flow stops at enrolment,
   and the test asserts no credential row was written.
2. **The ID token is not a credential.** Every authenticated route refuses
   a valid ID token in `Authorization`, with the one 401 shape.
3. **The exchange pins.** A device credential minted from a token for
   identity I in org O has `actor` = I's human actor in O and `principal`
   = that actor; the request body cannot set either (invariant 11).
4. **External MFA evidence.** A token from the test Keycloak without an
   `amr`/`acr` that maps to MFA is refused with the specific message; with
   it, enrolment succeeds; an org registered as `asserted-by-policy`
   succeeds and the audit row says so.
5. **Step-up checks the token, not the session.** A vault reveal with a
   device credential a day old and a step-up token thirty seconds old
   succeeds; with a step-up token six minutes old it is refused for age,
   not for permission (*Check which refusal*).
6. **AI never signs in.** An AI credential cannot start the OIDC flow and
   cannot enrol a device; refused by the predicate with the grant message.
7. **Refresh reuse is detected.** Presenting a rotated-out refresh token
   revokes the whole chain and the devices page shows it.
8. **Revocation is local.** Revoking a device credential logs that device
   out on its next request while the IdP session is still valid; nothing
   is asked of the IdP.

## Order

1. **The registry's identity model**: `identities`, `identity_issuers
   (issuer, subject)`, `factors`, under D43 phase 2 (the registry exists
   from then). Scenario 3's writer.
2. **The exchange**: `/credentials` accepting an ID token from a registered
   issuer, with the test Keycloak in the container tier; scenarios 2, 3, 4,
   8. An external IdP works end to end before the built-in one exists, so
   the household can sign in through its own identity server first if it
   has one.
3. **The built-in issuer**: authorization code with PKCE, passkeys, TOTP,
   recovery codes, first-boot enrolment; scenarios 1, 7; the e2e sign-in
   through it.
4. **Step-up** and the device authorization grant; scenario 5; the D47
   `hive-channel` and the CLI sign in through the device grant.
5. **The client's screens** (§7), with the D45 client.
6. Identity linking across issuers (§6), when a second issuer exists to
   link.

## What lost

- *A password-only login, with MFA as an option.* Nate's words; and a
  platform that hosts people's AI agents and their vault is exactly what a
  reused password is used against.
- *SMS or e-mail codes as a factor.* The attacker who has the mailbox has
  the code.
- *Accepting the IdP's tokens on every request.* Revocation, outage
  tolerance and a second credential kind in the predicate; §4.
- *Requiring Keycloak.* Running an identity server to sign in to a
  household box is the case Nate ruled out; the built-in issuer is why.
- *A non-OIDC built-in login.* It would be a second sign-in code path in
  the client and a second thing to get wrong.
- *An admin switch that waives the second factor.* "A must" has no
  off-switch; §3's `asserted-by-policy` is visibility, not a waiver.

## Open

- Whether the built-in issuer may serve **third-party** clients (an app in
  a D44 frame signing in through it, a household service using hive-mind
  as its IdP). The surface allows it; the consent page exists for it;
  whether to turn it on is a later decision.
- Session lifetime defaults (device credential, refresh token, step-up
  window) beyond the ones named here; set by the org, measured first.
- Whether an org admin may list and revoke members' device credentials
  (assumed yes, with step-up, and audited).
- Passkey synchronisation across a person's devices is the platform
  vendor's (Apple, Google, 1Password); the record relies on it and does
  not replace it.
