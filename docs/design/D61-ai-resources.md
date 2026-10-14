# D61: the kit's AI resources are a catalogue: inference channels (harness runs under Claude Code, Codex and OpenCode; the direct API through the gateway; local models), embeddings and voice, each a named resource an install is granted, listed and described on one `ai` surface a host app can show in its settings and bind in its manifest; the host profile says which are on; keys and sessions stay where D35, D37 and D48 put them

**Decided** 2026-10-14 by Nate: "hive mind kit needs to display ai resources
too.. like embeddings, infernce channels (claude code, codex, direct api,
etc) voice support all of it." Shape by Propolis. Builds on D35 (hosted
agent runs under claude, codex and opencode; a person's own subscription
signs in inside their container; the platform never holds the token), D37
(the Anthropic-format gateway in front of OpenAI as a daemon role, a
one-run token in the container and the provider key on the daemon's side),
D42 (the local model seam: `read`, `transcribe`, `generate`, `embed` bound
to OpenAI-shaped endpoints by configuration, `--run-models`, the `models`
capability), D36 (voice both ways on a server worker behind the OpenAI
audio shapes), D56 (vectors on tables and documents), D48 (the vault leases
an API key to an AI and never shows it), D55 (host installs and the host
profile's `surfaces`), D33 (grants with an install target) and invariant 2.
**Amends D55 §1**: `ai` joins the surfaces a host profile turns on or off.

## What exists, so the record names it rather than reinventing it

| resource today | record | how an app reaches it | what is missing |
|---|---|---|---|
| an agent run under Claude Code, Codex or OpenCode in a sandboxed container | D35, D53 §1's harness | the chat worker and `agent_run` steps; no app-facing call | a name an install can be granted, a listing |
| the direct Anthropic-format API, fronted for OpenAI | D37 | the harness container only (a one-run token) | no app or host may use it; no listing |
| local models: read, transcribe, generate, embed | D42 | the `models` guest capability, the job queue | no listing of which endpoint backs which capability, no per-install grant beyond the capability |
| vectors | D56 | `near` on tables and documents | fine; it is storage, listed here only as what `embed` feeds |
| voice: speech to text and text to speech | D36 | the client's audio shapes | not an app resource at all |
| API keys for providers | D48 | leased to an AI run, never read | the lease is the right shape; it needs a resource to lease for |

The pieces exist and each has its own door. What Nate asked for is one
front: a thing a host app can list, show a person, and be granted from.

## The decision

### 1. One `ai` surface, a catalogue of named resources

- **A resource** is a row the kit knows about, with a **kind**, a **name**
  unique within the deployment, a **provider** (what backs it), its
  **capabilities** (the verbs it answers), its **cost class** (`local`,
  `metered`, `subscription`), its **state** (`ready`, `degraded`, `down`,
  `unconfigured`, with the last reason) and **who may use it** (grants).
  The kinds:
  - `channel`: an inference channel. `harness:claude`, `harness:codex`,
    `harness:opencode` (D35 runs: the unmodified CLI in the person's own
    container with their own sign-in); `api:anthropic` and `api:openai`
    through the D37 gateway with a provider key from the vault (D48);
    `model:<name>` for a D42 endpoint that answers `generate` or `read`.
  - `embedding`: a D42 `embed` endpoint with its dimension and model name,
    which is what D56's `vector(dim)` columns are declared against.
  - `voice`: a D36 worker, `transcribe` and `speak`, with its model names.
  - `transcription` and `reading` where an endpoint answers those alone.
- **`ai.list`** returns the catalogue the caller may see: every resource
  the caller's install or person holds a grant on, with state and cost
  class, and the ones it may *ask* for (D48 §1's "may ask for a grant it
  lacks but never read" applied to resources). **`ai.describe {name}`**
  returns one resource with its capabilities, limits (context, rate,
  concurrency) and the current state's reason. Both are tools (D33 and
  invariant 14 qualify them per install), API routes (D45) and SDK calls
  (`hive.ai.list()`), the same answer on each.
- **`ai.invoke {resource, capability, input}`** is the one way an app
  uses a resource: a `generate` on a channel, an `embed` on an embedding,
  a `transcribe` on a voice resource, a `run` on a harness channel (which
  is D35's agent run, started for the install's principal and recorded as
  the AI actor's work, invariant 2). The call carries `{trust, data}` in
  and out (invariant 12), so a `generate` over untrusted input yields an
  untrusted answer, and a `read` of a fetched page stays untrusted
  downstream (invariant 9). Streaming answers come back as a stream route
  (D58) when the caller asked for one.
- **Grants decide.** A resource is a grant subject (`subject_kind = 'ai'`,
  the resource name as the subject id), granted to a person, an org or an
  install (D33's install target), with the verb being the capability.
  Absence is deny. An app declares the resources it wants in its manifest
  (`ai: [{name_or_kind, capabilities}]`), the install screen shows them,
  and the person grants or refuses each; a kind without a name (`any
  embedding`) is resolved to the deployment's default of that kind at
  install and the binding recorded, so a later default change does not
  silently move an app's embeddings to a different model (invariant 14:
  the dimension is in the binding).

### 2. The host profile says which are on, and the host shows them

- A host profile's `surfaces` gains `ai`; off is the refusal at the route
  (D55 §1). A host may further narrow with `ai.allow: [kinds or names]`,
  so a media app offers embeddings and transcription and not a coding
  harness.
- The catalogue is what a host app's settings slot renders (D55 §3, D62):
  each resource's name, provider, state and cost class, the grants this
  install holds, and a request button that is D48's "ask for a grant".
  The declared schema has a `resource` element for exactly this, so
  nectar's settings page and the kit's own UI show the same card.

### 3. Keys, sessions and tokens stay where they are

- **A person's Claude subscription** signs in inside their own container
  (D35); the kit never holds the token; `harness:claude` on their behalf
  is their sign-in, and `ai.describe` says `cost: subscription, signed in
  as <handle>` without saying anything else.
- **Provider API keys** are D48 vault entries, leased to a run or to the
  gateway for one call batch, never read by an app or a host. An
  `api:anthropic` resource is `unconfigured` until an admin puts a key in
  the vault and binds it; the catalogue says so, with the reason.
- **Local models** are configured by the operator (D42) and appear as
  resources automatically, `ready` when the endpoint answers its health
  route and `down` with the message when it does not.
- Nothing in this record moves a secret into the catalogue; the catalogue
  holds names, states and grants.

### 4. Where nectar's pieces land (D56 §1)

nectar's `infer` becomes one or more D42 endpoints behind the seam, and
the resources are **the configured models, never fixed names**. nectar's
text embedder is selected by configuration (`[embedding] local_model`,
one of `arctic-embed-m`, `bge-small`, `minilm-l6`, `mxbai-large`), the
default being `snowflake-arctic-embed-m-v1.5` at 768 dimensions because
`infer/examples/embed_eval.rs` chose it on the real library (nectar
#226), and every vector records the model key it was made with
(`local:<id>`). So the resource nectar declares is
`embedding:arctic-embed-m` by default and whichever model is configured
otherwise, with the dimension and model name in the binding (§1);
changing the configured model is a new binding and a re-embed of the
library, which is why the binding is recorded and never silently
followed. Beside it: `embedding:clip` and `embedding:clap` (which add a
multimodal `embed` to D42, the capability D56 named), `reading:rerank` (a
cross-encoder answering `rerank`, the other capability D56 named) and
`voice:whisper` for `transcribe`. nectar declares them in its manifest
and the household grants them; its recommendation worker calls
`ai.invoke` for embeddings the way it called its own code.

*Corrected 2026-10-14 by Forage: the first text named mxbai-embed-large
as nectar's embedder; it is one of the alternatives.*

## Acceptance scenarios, checked in before the code

1. **The catalogue is grant-filtered.** Two installs in one org, one
   granted `embedding:test`, see different `ai.list` answers; the ungranted
   one sees the resource as askable, not usable, and `ai.invoke` on it is
   refused with the grant message.
2. **A kind binds to a name at install.** An app declaring `any embedding`
   is bound to the default at install; changing the deployment default
   afterwards leaves the install bound to the first (asserted from the
   binding row), and `ai.describe` shows which.
3. **Trust travels.** `ai.invoke generate` with untrusted input returns an
   untrusted answer; a write made from it inherits untrusted (invariant
   12's existing test, through the new call).
4. **State is honest.** A `model:` resource whose endpoint is stopped
   reports `down` with the engine's message within one health interval;
   an `api:` resource with no key reports `unconfigured` with the reason;
   neither is listed as `ready`.
5. **Harness runs are the AI actor's.** `ai.invoke run` on
   `harness:claude` from an install records the run under the install's
   AI actor acting for its principal (invariant 2), and a person's
   sign-in state appears in `describe` as `subscription, signed in as`,
   never as a token.
6. **The host narrows.** A host profile with `ai.allow: [embedding]` gets
   `ai.list` answers with embeddings only and a refusal naming the profile
   for a `generate`.
7. **Secrets stay out.** `ai.describe` on every resource kind contains no
   key, token or URL with credentials (asserted by a scan of the response
   against the vault's values in the test fixture).
8. **The settings card renders.** The declared schema's `resource` element
   validates for every resource in a test catalogue (D55 §3's validator).

## Order

1. **The catalogue over what exists**: resource rows for D42 endpoints,
   D37's gateway and the three harness kinds; `ai.list` and `ai.describe`;
   the grant subject kind. Scenarios 1, 4, 7.
2. **`ai.invoke`** for `generate`, `embed`, `transcribe` and `read` over
   D42 and D37; `run` over D35. Scenarios 3, 5.
3. **Manifest declarations and the install binding.** Scenario 2.
4. **The host profile's `ai` surface and `ai.allow`.** Scenario 6.
5. **The `resource` schema element** (with D62). Scenario 8.
6. nectar's endpoints behind the seam (D56 §1's slices), with `rerank` and
   multimodal `embed` added to D42's capabilities.

After the D53 runtime; the catalogue is useful to the kit's own UI
before any process app exists, so step 1 may land earlier.

## What lost

- *Each resource keeping its own door.* Five records, five ways in, no
  listing; Nate asked for the listing.
- *The catalogue holding keys.* D48's leases are the shape; a catalogue
  that showed a key would be a second vault.
- *Apps calling providers directly.* No network (D53 §1), and a key in an
  app is a key everywhere.
- *Binding a kind without recording the name.* A default change would move
  an app's vectors to a different model and dimension silently (invariant
  14).
- *A separate resource type per provider.* Kinds and capabilities are the
  interface; providers are rows.

## Open

- Rate and concurrency limits per resource per install, and who sets them.
- Whether a person's own `harness:claude` sign-in may be used by an app
  installed by their org (assumed no: the subscription is the person's,
  and an org-installed app runs on the org's channels).
- Cost reporting per install (metered resources) on the storage page's
  neighbour; named, not designed.
- `rerank` and multimodal `embed` as D42 capabilities: a D42 amendment when
  nectar's endpoints land.
