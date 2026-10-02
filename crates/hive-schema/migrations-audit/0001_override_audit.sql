-- The override audit, in a file of its own (D38 §3).
--
-- D18.2: every access that succeeded ONLY because of an override is audited,
-- and the audit row must survive whatever transaction the caller was in: a
-- read can stream rows to a client and then roll back, and the evidence must
-- not roll back with it. On Postgres that was "a second connection, outside
-- the caller's transaction". With one writer per file, a second connection on
-- the same file would wait on the caller's write lock until the caller
-- finished, which is a deadlock when the caller is waiting on the audit. So
-- the audit lives in its own file, with its own lock, written by a connection
-- that holds nothing else.
--
-- No foreign keys: a file cannot reference rows in another. The ids are the
-- platform's, copied verbatim; an audit row outlives the grant and the actor
-- it names, which is the right direction for evidence. The owner is what the
-- predicate resolved from the subject at the moment of the decision, never a
-- value the caller supplied.

CREATE TABLE grant_override_audit (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    grant_id       TEXT,
    actor_id       TEXT NOT NULL,
    principal_kind TEXT NOT NULL CHECK (principal_kind IN ('user', 'org')),
    principal_id   TEXT NOT NULL,
    subject_kind   TEXT NOT NULL,
    subject_id     TEXT NOT NULL,
    subject_name   TEXT,
    owner_kind     TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id       TEXT NOT NULL,
    access         TEXT NOT NULL,
    reason         TEXT NOT NULL DEFAULT '',
    occurred_at    INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER))
);

CREATE INDEX grant_override_audit_actor_idx ON grant_override_audit (actor_id, occurred_at DESC);
