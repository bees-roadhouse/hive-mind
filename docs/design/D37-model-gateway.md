# D37: an Anthropic-format model gateway in front of OpenAI, as a daemon role, for the Claude Code harness

**Decided** 2026-09-14 by Nate, relayed through Pia: "openai through
gateway, claude code harness only for now." Codex as a hive-mind runtime is
not in scope and is not extended. Shape by Propolis the same day, against
the Claude Code gateway documentation as it stood that day.

## What Claude Code needs from the other end

Claude Code speaks the Anthropic Messages format to `ANTHROPIC_BASE_URL`
(or Bedrock and Vertex shapes to their own variables); it has no OpenAI
mode. Its gateway compatibility guide is explicit that a gateway "must
expose at least one of the following API formats", and its per-developer
page that the thing on the other end has to talk Anthropic. So OpenAI models
under Claude Code's loop need **a translation layer**, and the choice is
whose.

## The decision

1. **Our own, in Rust, as a daemon role: `--run-model-gateway`.** Not
   LiteLLM or another proxy stack. The house rule (Rust) is one reason and
   not the deciding one. The deciding one is the credential boundary: a run
   gets a **one-run bearer token to the gateway**, and the OpenAI key is a
   vault lease (`docs/vault.md`, D35 item 5) held by the gateway on the
   daemon's side. **The provider key never enters the container.** A proxy
   stack with `OPENAI_API_KEY` in the run's environment gives that up on
   day one.
2. **Reachable from a run only through `NetworkMode::Proxied`**, over the
   run's own internal network, with `egress_allow` naming the gateway and
   nothing else. The run's `ANTHROPIC_BASE_URL` points at it and
   `ANTHROPIC_AUTH_TOKEN` carries the run token. The gateway's own egress to
   `api.openai.com` is the daemon's, allowlisted like any host capability.
3. **Keyed per run, in every reused thing** (invariant 14): the upstream
   HTTP connection, any response cache, any memo of model capabilities.
   The transport case in that invariant's history was exactly a pooled
   connection opened under one run's rule and reused for another's.
4. **The run token is the credential** (invariant 2): it names the run, the
   run names the actor acting for the principal, and the gateway's audit
   row says which run spent what against which lease. A request with no
   valid run token is 401 in the one shape (`hive-httpauth`).
5. **Translation is Anthropic Messages ⇄ OpenAI**, streaming both ways,
   with the degradations below stated rather than papered over.
6. **Model selection is per run** via `ANTHROPIC_MODEL` in the run's
   environment; discovery (`/v1/models`) is not offered, because Claude
   Code's picker keeps only ids containing `claude` or `anthropic` and
   aliasing an OpenAI model to pass that filter would be a lie in a UI.

## What degrades with a non-Claude model, from the compatibility guide

| Claude Code sends | Behind this gateway | Consequence |
|---|---|---|
| `cache_control` markers on system and messages | no OpenAI equivalent in that shape; markers dropped | every turn bills as uncached input unless OpenAI's automatic prefix caching happens to hit; visible as high input tokens |
| `thinking: {type: adaptive}` for current models | translated to `reasoning_effort` where the model has it; otherwise rejected with wording Claude Code recognises so it retries without and disables it for the conversation | no interleaved thinking; no thinking signatures |
| beta headers for context management, tool fields (`strict`, `defer_loading`), `output_config` | not forwardable; the run sets `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1` or they 400 | fewer pre-release capabilities; structured outputs and effort settings absent |
| `tool_use` / `tool_result` blocks, streamed | translated to `tool_calls` / tool messages; partial-JSON streaming differs and parallel-call semantics differ | tool-call fidelity is the gateway's translation quality, tested per model |
| `count_tokens` | not offered | Claude Code falls back to a character estimate; `/context` is approximate |
| the system-prompt attribution block | forwarded as ordinary system text | reaches the model; `CLAUDE_CODE_ATTRIBUTION_HEADER=0` set in the run env to omit it |
| keep-alive pings during long pauses | the gateway emits its own SSE pings | without them Claude Code aborts a silent stream at 300 s |
| error bodies | forwarded unmodified, or with the `capability_rejected:` token | Claude Code's retry logic matches on wording; an envelope breaks it |

Also from the docs, and not a degradation but a boundary: channels (the
delivery path the bus, #99, proposes) require Anthropic authentication and are unavailable behind a
gateway, so a run on this gateway cannot receive a channel and reads
`bus.inbox` instead. Remote Control and voice dictation are likewise off.

## Terms

The credential is an API key to the gateway and an OpenAI API key behind
it; no Anthropic traffic and no subscription credential is involved, so
D35's terms section does not apply. Anthropic's compatibility guide treats
third-party upstreams behind a gateway as a supported configuration and
documents what breaks. OpenAI's API key path is its sanctioned one.

## What lost

- *LiteLLM (or any proxy stack) with the key in the run's environment*:
  the credential boundary above, and a Python stack on the fleet for one
  seam.
- *The Codex harness as the OpenAI path*: it exists and is subscription-
  billed, and Nate said Claude Code's loop only, for now. It stays built and
  unextended.
- *Model discovery through the picker*: the alias would be a lie.

## Not decided here

- Which OpenAI models are offered per run, and the translation of each
  one's reasoning controls; per model, in the issue.
- Whether the gateway later fronts other providers; the seam is provider-
  shaped and nothing here prevents it.
