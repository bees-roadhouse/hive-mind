# D35: a person's subscription signs in to the binary, in their own container; the platform keeps the directory and never the token

**Decided** 2026-09-14 by Nate, relayed through Pia, after both of us had put
the terms concern to him. Recorded by Propolis the same day, with the residual
risk written down because he asked for it to be built anyway and the record
has to say what was known when he did.

This supersedes the mechanism in D32 §6 and the subscription half of #87. The
goal in D32 §6 stands unchanged: a person's own Claude subscription (and now
their ChatGPT subscription) runs the agents the platform launches for them,
billed to them, and the subscription is not the platform's login. What changes
is how the credential gets into a run, because the way D32 chose is the one
Anthropic's terms name.

## What the terms say, read on 2026-09-14

Anthropic's Claude Code documentation, *Legal and compliance*, under
*Authentication and credential use*, verbatim:

> Anthropic does not permit third-party developers to offer Claude.ai login
> into their own applications, or to route requests through Free, Pro, or Max
> plan credentials on behalf of their users. Moreover, developers may not
> collect, store, or intermediate Claude.ai credentials or session tokens —
> sign-in to a Claude account must complete through Anthropic's own flow.

and:

> Anthropic reserves the right to take measures to enforce these restrictions
> and may do so without prior notice.

The same page, under *Can customers offer Claude Code in their products?*,
names what a hosting platform may do: run the unmodified binary under the
Commercial Terms, remove no built-in authentication method, and have each end
user authenticate with their own credential, billed to them. And then:

> Nor does it prevent an end user from signing in to the unmodified Claude Code
> binary with their own Claude subscription, including where a platform hosts
> Claude Code as described under *Can customers offer Claude Code in their
> products?* above.

Two more sentences from that page shape the edges: "Advertised usage limits
for Pro and Max plans assume ordinary, individual usage of Claude Code and the
Agent SDK", and "The Claude Code binary must not be modified."

D32 §6 had the person run `claude setup-token`, paste the result into the
platform, and the platform store it in the vault and inject it into every run
as `CLAUDE_CODE_OAUTH_TOKEN`. That is "collect, store, or intermediate ...
session tokens", in the words of the paragraph that forbids it. The CLI's own
description of `setup-token` says it "does not save the token anywhere; copy
it and set it ... wherever you want to authenticate", which is a CI shape, not
a hosting shape. So the mechanism moves, and the goal does not.

For OpenAI, the primary source is the Codex CLI documentation (*Authentication*):
ChatGPT sign-in is a supported method, `codex login --device-auth` is the
headless flow, copying `~/.codex/auth.json` is described as working "across
SSH and Docker deployments", and "Use API key authentication for programmatic
Codex CLI workflows, such as CI/CD jobs." OpenAI's Terms of Use page could not
be fetched from this machine (HTTP 403, twice), so that document is
**unverified here**; the record says so rather than paraphrasing press. Using
Codex's OAuth from a client that is not Codex is unsupported by OpenAI and is
the same shape Anthropic acted against; nothing here does it.

## The decision

Nate, 2026-09-14: "i wanted it to sign in yourself but the platform holds the
token for the user. it's the same exact claude code just running in its own
container per user basically. and users should be able to use their own
container images or modify them in house."

Built as:

1. **The person signs in, inside their own container, through the binary's
   own flow.** A sign-in run is an interactive harness run of the unmodified
   CLI (`claude auth login`, or the first-launch prompt; `codex login
   --device-auth`) attached to a terminal in the browser client. The CLI
   prints Anthropic's URL, the person completes it in their own browser, and
   pastes the code back if the callback cannot reach the container, which in
   a container it cannot. The platform renders the terminal; it does not
   perform, proxy or observe the OAuth exchange.
2. **What persists is the directory, not the token.** The harness keeps home
   on a tmpfs as before, and mounts one more thing: a **per-principal config
   volume** at `/config/<runtime>`, with `CLAUDE_CONFIG_DIR` or `CODEX_HOME`
   set by the launcher to point at it. Both variables join `RESERVED_ENV`, so
   a spec cannot redirect them the way it already cannot redirect `HOME`. The
   CLI writes `.credentials.json` (mode 0600) or `auth.json` there itself and
   refreshes it itself; the next run for the same person finds it where the
   binary left it. A bind mount is not captured by `podman commit`, so a
   snapshot of the run's image is clean by construction (the property
   `docs/vault.md` already relies on for tmpfs).
3. **The platform never reads, copies, injects or replays the credential.**
   One module in `hive-harness` knows the volume's host path and mounts it;
   nothing in the daemon opens a file beneath it, and the daemon never sets
   `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN` or `OPENAI_API_KEY` from
   a subscription. The directory is owned by the harness uid, mode 0700, and
   mounted into that principal's runs only; keyed on the principal, not the
   runtime or the image (invariant 14: a volume shared across principals
   would hand one person's login to another's run).
4. **One volume per principal per runtime**, so "link both" is two
   directories, and a person with neither still gets a run through the vault.
5. **The vault keeps its lease design for API keys** (`docs/vault.md`):
   Anthropic and OpenAI keys, owned by a principal, leased per run, injected
   as environment into the tmpfs, recorded against the run. A run resolves its
   credential as: the person's config volume for the runtime if one exists,
   else a leased key, else the run does not start. **Automated runs — a
   workflow step, a schedule, anything with no person at the other end —
   always take the lease path**, because "ordinary, individual usage" is the
   line the usage-limits sentence draws, and an unattended loop spending a
   consumer subscription is the thing on the other side of it.
6. **A person may bring their own image or modify one in house.** The image
   is already a per-run digest pin (`RunSpec::image_repository`,
   `image_digest`, D12.5); this adds a per-principal pin the run resolves
   before the deployment default. The image around the binary is theirs. The
   binary inside it is the other thing the terms single out, so the platform
   records the CLI version the image reports at pin time and the D says
   plainly: a modified `claude` binary is outside the case this decision
   stands in, and the platform does not check for one beyond the version
   label, because it cannot.

## Why this shape

- It is the case Anthropic's page describes as permitted, as far as the page
  goes: unmodified binary, the person's own sign-in through Anthropic's flow,
  billed to them, hosted by a platform. It is also the line enforcement was
  reported to fall on in January 2026: tools that extracted the token and
  presented it from elsewhere.
- The person's goal (sign in once) and the operator's goal (hold nothing) are
  met by the same object: a directory the binary owns. A vault row holding
  the token would meet the first and fail the second.
- The vault stays a vault. It holds the things that are ours to hold —
  API keys — and the subscription credential is not in it, so a vault
  compromise does not become an account compromise.

## Residual risk, recorded because it was accepted

- **The platform is still a third party hosting Claude Code for others, and
  the operator must be under the Commercial Terms for that.** That is Nate's
  to accept for the instance; this decision assumes he has.
- **"Never reads" is a rule, not a boundary.** The daemon runs as the host
  user and `--userns keep-id` maps that user to the harness uid, so the
  daemon *could* read the volume. What prevents it is one mounting module, a
  grep-able rule, and a test that the daemon's own source never opens a path
  under the volume root. A separate uid for the daemon would make it a
  boundary; that is a deployment change for later, not a reason to wait.
- **Enforcement is without notice.** If Anthropic decides a hosted container
  is not "ordinary, individual usage", the account that suffers is the
  person's, not the platform's. Every person who links a subscription is told
  this on the enrollment page, in one sentence, before the terminal opens.
- **The login expires.** A `/login` credential has a lifetime and the CLI
  warns three days out; a person whose runs start failing with *Login
  expired* signs in again through the same terminal. Unattended work is on
  API keys for this reason as much as the terms one.
- **OpenAI's terms are unverified from here** (above). The Codex docs support
  the mechanism; the contract behind them was not read.
- **A modified binary** in a bring-your-own image is undetectable by the
  platform and outside this decision.

## What lost

- *Paste a `setup-token` into the platform* (D32 §6): the prohibited sentence,
  verbatim.
- *A vault row for the OAuth token, leased like a key*: the same thing with a
  lease on it; "intermediate" covers it.
- *An Anthropic-format gateway in front of the subscription*: the docs allow
  a claude.ai login through `ANTHROPIC_BASE_URL`, but the gateway would then
  hold the session token in flight on every request, which is the platform
  intermediating it with extra steps.
- *Refuse to build it*: Nate heard the concern from both of us and decided;
  the record carries the risk instead.

## What this does not decide

- The interactive terminal in the browser client (a PTY-attached harness run
  over a WebSocket) is the enrollment surface and is a piece of work in its
  own right; it is not designed here.
- Where the per-principal volumes live on the Talos cluster (Longhorn, an
  encrypted volume or not) is Pia's side.
- Whether apis, the DTC identity, may link anything: no, until Nate says so,
  and never a DTC credential on the Roadhouse instance.
