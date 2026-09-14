# D36: voice both ways, rendered by a server worker; the model picks and the criteria that chose them

**Decided** 2026-09-14 by Nate, relayed through Pia: "voice speech to text
and voice to text as well. best open model and voice quality available we
run server side in a worker. this should all be server side pretty much
with htmx etc.." Picks and criteria by Propolis the same day, from what
could be read and what could be measured on this desk; the numbers that
matter are the ones still to be measured on a cluster node, and the D says
which.

This **reverses D30's placement** (speech rendered on the client) and keeps
D30's principle (text is the record). D30 stays in the log so the reversal
is visible.

## The decision

1. **Both directions, on the server.** Speech-to-text and text-to-speech run
   in a **voice worker**: a daemon role (`--run-voice`), CPU-only today,
   behind the OpenAI audio API shapes (`/v1/audio/transcriptions`,
   `/v1/audio/speech`) so that the engine behind each is swappable and the
   cluster's existing Kokoro deployment is a valid backend on day one.
2. **The page is server-driven.** Transcript, captions, turn state and the
   composer are htmx fragments like everything else in the client (D32).
   **One JavaScript island** does what a page cannot: capture the
   microphone, stream audio up, play audio down, and detect barge-in. It
   holds no conversation state and renders no text; the fragments do that.
   Pia's note, accepted: htmx cannot reach the microphone, so the island is
   not optional, only minimal.
3. **Text stays the record** (D30's principle). Transcribed speech becomes
   the person's message; the agent's text becomes the transcript; what was
   heard is what was shown. **Audio is never stored**: not in the event log,
   not in the blob store, not on disk past the request that carried it. A
   transcribed message carries `source: voice` so a reader can tell dictated
   from typed, and the STT confidence is on the message, not in the text.
4. **Interruption is a first-class turn event.** The island detects the
   person speaking over the agent, stops playback locally at once, and the
   daemon cancels the running turn (the supervisor kills on cancel and the
   run lands `cancelled`, invariant 10 untouched). The transcript records
   the answer as cut after the last sentence that was spoken aloud, so the
   record says what the person actually heard.
5. **Speech starts at the first sentence boundary** of the streamed reply,
   as D30 already said, now on the worker: the chat SSE stream is the
   worker's input, the worker chunks on sentence punctuation, and a new
   turn from the same agent cancels its queue.
6. **The voice is part of the agent's definition** (D27, D30): a voice id
   on the agent record, honoured by the worker.
7. **Trust.** Speech from an authenticated person's microphone is
   first-party input and starts `trusted`, like a typed message. A recording
   or file uploaded for transcription is content the platform was handed and
   is `untrusted` (invariant 9). The worker does not decide this; the route
   that received the audio does.

## Why the server, given D30 said the client

D30's reasons were real and are recorded there: the text is already on the
client, it scales with the people, it works offline, local synthesis is
faster than a hop. Nate's reasons for reversing outweigh them for this
platform, and they are these:

- **"Best open model and voice quality available."** The models that win
  on quality are hundreds of millions to billions of parameters. A browser
  runs an 82M model well and a 600M streaming ASR badly; a phone runs
  neither. The client tier caps quality at whatever the weakest client can
  run, and the server tier caps it at what one node can run, which is a
  number we can raise.
- **One engine, one measurement.** A server pick is measured once on known
  hardware. A client pick is a different experience per device, and D30's
  "swappable behind the OpenAI shape" fallback would have become the common
  path on every phone anyway.
- **Online-first is already the rule** (D32). The offline argument for
  client rendering was accepted as lost when the shell went htmx.
- **What it costs** (accepted): a hop per direction, CPU on the cluster per
  concurrent speaker, and a worker that must not be on any critical path
  the chat run holds (the hub already drops a slow subscriber rather than
  waiting; the voice worker is a subscriber).

## Hardware, and what it buys

Today's constraint, from Pia: the Talos nodes are Hyper-V VMs with no GPU;
the Pi is arm64 with no usable accelerator under Talos. The worker is
**CPU-only on 8 vCPU**. Every pick below is made for that, and the D is
explicit about what a GPU node changes so the second decision is cheap.

## Criteria (these outlive the picks)

For **speech-to-text**, in order:

1. **License permits self-hosting for a household and commercial use**
   (Apache-2.0, MIT, CC-BY-4.0 acceptable; non-commercial or research-only
   licences excluded before anything is measured).
2. **Real-time on 8 vCPU with int8 weights**: a chunk transcribes in well
   under its own duration, so the pipeline never falls behind a speaker.
   Target ≥ 5× real time; the floor is 2×, below which barge-in lags.
3. **Streaming or chunkable at ≤ 2 s latency** from end of speech to text,
   because the turn cannot start until the words exist.
4. **English WER on the Open ASR leaderboard ≤ 7%.** The top of the board
   clusters within a point; below 7% the difference is inaudible in a
   household and above it the agent starts answering the wrong question.
5. **Languages the house speaks**, English first; multilingual is a tie-break.
6. **A maintained CPU runtime** (ONNX Runtime or CTranslate2), not a
   research checkpoint.

For **text-to-speech**:

1. License, as above.
2. **Faster than real time on CPU per sentence**, so the first sentence
   plays before the second is synthesised. Target ≤ 0.5 s for a typical
   sentence on 8 vCPU.
3. **Quality**, by blind preference where a benchmark exists (Speech Arena
   Elo, published MOS), and by ear on this house's agents.
4. **Stable voice identity** across sentences and sessions, so an agent
   sounds like itself (D27). Cloning is not required and is a liability
   here: nothing on the platform should be able to speak in a person's
   voice.
5. Streaming synthesis, so playback starts before the sentence is done.

## Picks, September 2026

**Speech-to-text: NVIDIA Parakeet-TDT 0.6B v3, ONNX int8, chunked
streaming**, as the first driver. Why, against the criteria: CC-BY-4.0;
6.3% mean WER on the Open ASR leaderboard, which is the best in the CPU
tier and within a point of the 2B-class leaders; 25 European languages
including English; a TDT architecture that community ONNX builds run at
many times real time on consumer CPUs; chunked streaming with a sliding
window at 1–4 s latency depending on the chunk and lookahead chosen. What
was *not* verified: no published number on an 8-vCPU Hyper-V node. **The
first task of the voice issue is to measure it there**, with the low-latency
preset (0.5–1 s chunks), and the pick is final only if it clears criterion 2.

Alternatives, held: **NVIDIA Nemotron Speech Streaming** is natively
streaming at 0.56 s algorithmic latency and its paper reports comfortably
faster than real time on CPU at 0.67 GB int4, at 8.2% streaming WER; if
Parakeet's chunk latency proves too long for barge-in, this is the swap, at a
cost in accuracy. **Kyutai STT 1B** streams at 0.5 s and 6.4% WER but is
GPU-oriented and English/French. **Whisper large-v3-turbo via faster-whisper**
(MIT, 99 languages, ~8–10% WER, near real time on 8 cores at int8) is the
multilingual fallback and nothing else; it lost on WER and speed. The
2B-class leaders (Granite Speech 4.1, Canary-Qwen, Qwen3-ASR) lost on
criterion 2: LLM-decoder ASR is not a CPU streaming model.

**Text-to-speech: Kokoro-82M**, which is already deployed on the cluster
(brh-infra `cluster/tofu/voice.tf`, Kokoro-FastAPI behind the OpenAI speech
API). Why: Apache-2.0; faster than real time on ordinary CPU cores at ~165 MB;
the highest Speech Arena Elo among open models in the comparisons read
today, ahead of several models forty times its size; 54 fixed voices, which
is the voice-identity property wanted and the no-cloning property wanted.
It is the rare case where the CPU-tier pick is also the quality pick.

Alternative, held for a GPU: **Chatterbox** (MIT, ~0.5B, Resemble) wins
blind preference against a commercial API in its authors' study and adds
emotion control, but is not real-time on CPU, and it clones voices, which
this platform would have to refuse to expose.

## What a GPU node changes

- STT: native streaming at ~0.5 s (Nemotron, Kyutai) and the 2B-class
  models in real time, so barge-in gets sharper and the WER gap closes.
- TTS: Chatterbox-class quality and emotion, and Orpheus-class expressive
  speech LLMs, in real time.
- Multilingual at full Whisper large-v3 quality in real time.
- None of the interfaces change: both directions sit behind the OpenAI
  audio shapes, so a GPU node is a config change and a new pin, not a
  redesign. That is the reason for criterion 6 and for the shape in item 1.

## The latency budget, and the number nobody has yet

End of speech → first audio heard is: STT chunk latency (1–2 s at the
low-latency preset, to be measured) + **the turn's cold start** + time to
the first sentence boundary + TTS for that sentence (< 0.5 s). The middle
term is the one this repo has never measured: a chat turn is one harness
run, a container start of Claude Code, and if that is several seconds then
voice needs a **warm per-conversation session**, which changes "one turn is
one run" (`docs/chat.md`) and is a decision on its own. It is deliberately
not made here. Measure the cold start on a cluster node first; decide after.

## What lost

- *A realtime speech-to-speech API* (gpt-realtime over WebRTC): a different
  model would be the one talking, at best a receptionist calling our agent
  as a tool, with a per-minute API dependency on every conversation. Held as
  a later option if the pipeline's latency is unacceptable after the warm
  session question is settled.
- *Client-side rendering* (D30): above.
- *Voice cloning*: the platform should not be able to speak as a person.
- *Storing audio*: D30's principle, kept.

## Not decided here

- The warm per-conversation session (above).
- Wake word versus push-to-talk: push-to-talk first, because the island is
  smaller and a wake word listening on the server is a privacy decision
  Nate has not made.
- Whether the voice worker is one process per direction or one for both;
  a deployment shape for Pia's side once the models are measured.
