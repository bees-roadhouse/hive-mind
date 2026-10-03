-- The model job queue (D42): one row per request an install made of a local
-- model, claimed by the models worker with a lease, finished with a result
-- or an error, and announced on the events table.
--
-- The worker writes no document. It knows no app's collections; the app
-- that asked reads the result through the capability and stores what it
-- chooses, under its own install and its own grants. Every result is
-- untrusted (invariant 9): a model's output is derived from content.
CREATE TABLE model_jobs (
    id             TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),

    -- What is asked. The four the platform knows (D42 §1).
    capability     TEXT NOT NULL CHECK (capability IN ('read', 'transcribe', 'generate', 'embed')),
    -- The input: bytes the requesting install proved it holds, or text.
    -- Exactly one of the two.
    input_blob     TEXT REFERENCES blobs (sha256) ON DELETE RESTRICT,
    -- The blob's content type as the catalogue described it at submit, so
    -- the worker can say what it is sending without a second lookup.
    input_mime     TEXT,
    input_text     TEXT,
    -- Capability-specific options as the app wrote them (a prompt, a
    -- schema, a list of categories). JSON, never interpreted here.
    options        TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(options)),

    -- Who asked. The install is part of the key: a result is readable by
    -- the install that asked and no other, whatever the owner (invariant 14).
    install_id     TEXT NOT NULL REFERENCES installs (id) ON DELETE CASCADE,
    owner_kind     TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id       TEXT NOT NULL REFERENCES actors (id),
    author_actor   TEXT NOT NULL REFERENCES actors (id),
    principal_kind TEXT NOT NULL CHECK (principal_kind IN ('user', 'org')),
    principal_id   TEXT NOT NULL REFERENCES actors (id),

    state          TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'claimed', 'done', 'failed')),
    claimed_by       TEXT,
    claimed_at       INTEGER,
    lease_expires_at INTEGER,
    attempts         INTEGER NOT NULL DEFAULT 0,

    -- The answer, as JSON the capability defines, or why there is none.
    result         TEXT CHECK (result IS NULL OR json_valid(result)),
    error          TEXT,

    created_at     INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    finished_at    INTEGER,

    CONSTRAINT model_jobs_one_input
        CHECK ((input_blob IS NULL) <> (input_text IS NULL)),
    CONSTRAINT model_jobs_claim_is_complete
        CHECK ((state = 'pending') = (claimed_by IS NULL)),
    CONSTRAINT model_jobs_done_has_result
        CHECK (state <> 'done' OR result IS NOT NULL),
    CONSTRAINT model_jobs_failed_has_error
        CHECK (state <> 'failed' OR error IS NOT NULL)
);

CREATE INDEX model_jobs_pending_idx ON model_jobs (created_at) WHERE state = 'pending';
CREATE INDEX model_jobs_lease_idx ON model_jobs (lease_expires_at) WHERE state = 'claimed';
CREATE INDEX model_jobs_install_idx ON model_jobs (install_id, created_at DESC);
