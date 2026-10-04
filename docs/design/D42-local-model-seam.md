# D42: local models behind one seam, as a daemon role with a job queue; apps ask, the worker answers, nothing in the result is trusted

**Status:** decided 2026-10-03. Nate: documents "for using ai locally to
transcribe documents and store the transcription and summary / title etc..
next to the blob. auto categorize them and also vectorize them. allow the
user to set a static list of categories." Asked whether "transcribe" meant
pages or audio: "both". Shape by Propolis, against D36 (the voice worker),
D37 (the model gateway) and D41 (vectors in the engine).

## What was asked

A documents app, and a journal, that use a model running on the owner's
own hardware: read a scanned page or a photo, transcribe a recording,
write a title and a summary, pick a category from a list the person set,
and produce an embedding so the thing can be found by meaning. All of it
local, all of it stored beside the blob it came from.

## The decision

### 1. Four capabilities, one seam, no model named in code

The platform knows four model capabilities and nothing about models:

| capability | in | out | wire shape |
|---|---|---|---|
| `read` | an image, by blob | text | OpenAI chat completions with image content |
| `transcribe` | audio, by blob | text, with timings where the engine gives them | OpenAI `/v1/audio/transcriptions` (D36's shape) |
| `generate` | a prompt and text | text, or JSON when a schema is given | OpenAI chat completions |
| `embed` | text | `float[dim]` | OpenAI `/v1/embeddings` |

Each is bound by configuration to an endpoint and a model name
(`HIVE_SANDBOX_MODELS_<CAP>_URL`, `_MODEL`, and a key through the vault's
lease when the vault lands, the environment until then, D37 item 1). The
OpenAI shapes are the lingua franca every local server speaks today,
llama.cpp, vLLM, Ollama, the cluster's Kokoro and Parakeet deployments
(D36), and a remote provider if a person ever chooses one. Nothing above
the seam knows which; a swap is a URL.

Title, summary and category are not capabilities. They are `generate`
with a prompt the app owns and a schema the app owns, because what a
"summary" is belongs to the app that stores it, and a category list is
the owner's setting, not the platform's. The platform's one contribution
to categorisation is the constraint: the answer is validated against the
list, and an answer off the list is a refusal, not a new category.

### 2. A job queue in the store, a worker as a daemon role

`model_jobs`: one row per request, with the capability, the input (a blob
hash the requesting install holds, or text), the options, the owner, the
requesting install, the requesting credential's actor and principal
(invariant 2), state, a lease, the result and an error. `--run-models`
is the role (D7: one process serves every role; the worker is one more),
with the chat worker's claim pattern: `UPDATE ... RETURNING` under `BEGIN
IMMEDIATE`, a lease and a heartbeat, a reclaimer for abandoned claims,
and a kick from the submitter so a job does not wait out the poll.

A job finishing appends an event, `model.job.finished`, to the events
table under the requesting owner with the job's id as its subject, and
the bell rings (invariant 4). An app subscribes the way it subscribes to
anything, reads the result through the capability, and writes what it
chooses to write. **The worker writes no document.** It knows no app's
collections, so it cannot put a summary in the wrong place or skip the
predicate on the way; the app that asked is the app that stores, under
its own install and its own grants.

### 3. The guest sees `models.submit` and `models.result`

A new capability host module, `models`, with two verbs, granted like any
other capability and declared in the manifest. `submit` takes a
capability name, a blob hash or text, and options; it resolves the blob
through the **caller's** references (invariant 3: naming a hash is not
holding the bytes) and returns the job id. `result` takes a job id and
returns the state, the result and the error; a job another install
submitted is "not found" (invariant 14: the job is keyed on the install
that asked, and the owner alone is not the key).

### 4. Nothing a model says is trusted

A model's output is derived from content, and content is what invariant 9
is about: a scanned letter can carry an instruction and a transcription
carries it faithfully. Every result is `untrusted`, whatever the input
was. The platform's taint tracking does the rest: a guest that reads a
result and writes a document writes it untrusted, an event it emits is
untrusted, and nothing of it reaches instruction position without the
`sanitize` capability, which is granted and audited (invariant 12). An
embedding is the one exception in kind and not in rule: it is a vector,
it carries no instruction, and it is still stored on a row whose trust is
the document's.

### 5. Audio that is a document is a document

D36 says audio from a voice turn is never stored. A recording a person
uploads to the documents app is the document, the way a scan is, and it
is stored as a blob under the uploader's reference like any upload
(`POST /blobs`). The two rules do not conflict: D36 is about the chat
turn's microphone stream, which is a conversation, and this is about a
file a person chose to keep.

### 6. What the apps do with it

The documents app, per owner (the user's install in the user's file, the
family org's install in the org's file, D39): a document is a blob plus
what the models said about it, `read` or `transcribe` by MIME type, then
`generate` for a title, a summary and a category from the owner's list,
then `embed` over the summary and the text, all written back by the app
into its own `documents` collection with `fts(text)`, `fts(summary)` and
`vector(embedding, dim)` (D41). The journal uses `embed` over entries and
`generate` where it chooses to. Both relate through core entities and
`uses` grants (#86), never app to app.

## What lost

- *The worker writes the summary into the document.* The worker would
  then have to know every app's schema and reach its tables, which is a
  second data layer with no predicate. The app stores; the worker answers.
- *A capability per task (summarise, title, categorise).* Each is a
  prompt and a schema the app owns; baking them into the platform would
  make the platform's opinion of a summary everyone's.
- *Tesseract or another OCR engine as a special case.* A vision model
  behind the chat shape reads a page and a handwritten note alike, and
  the shape is the same one `generate` uses; an OCR engine can still sit
  behind that URL if a person prefers it.
- *Running the model in-process.* The engines are their own servers with
  their own hardware needs, and the cluster already runs some (D36). The
  seam is a URL and the daemon stays a daemon.
- *Trusting a result because the input was the owner's own scan.* §4; the
  content decides, not the uploader.

## Open

- PDFs: `read` takes an image today. A PDF is pages to rasterise first,
  which needs a renderer the daemon does not carry yet; the documents app
  can ask per page once one does.
- The guest SDK's `models_submit` and `models_result` verbs land with the
  first guest that calls them (the journal), so the committed reference
  guest is not rebuilt for a change it does not use.
- The endpoints themselves: which server, which models, which node. The
  record binds shapes, not picks; the picks are measured when there is a
  node to measure on (D36 §"Hardware").
- Batching embeddings and chunking long texts for `embed` and `generate`:
  the first apps decide what they need.
- Cost accounting against a vault lease when a remote provider is behind
  a URL: the gateway's audit row shape (D37) is the model.
