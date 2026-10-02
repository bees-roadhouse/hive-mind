-- Migration one, for the libSQL engine (D38). Every table here exists because
-- getting it wrong becomes unrecoverable once a row lands. Decision references
-- are D<n> in docs/design/ and the epic's decision log.
--
-- Rules this file encodes, and which the database (not the application) is
-- responsible for holding:
--
--   * Every content row carries owner_kind + owner_id AND author_actor. "Nate
--     did this" and "an AI acting for Nate did this" are different facts (D17.4).
--   * Ownership, permission and trust are properties of a REFERENCE. blobs hold
--     bytes and nothing else; blob_refs hold owner, refcount and trust (D17.1).
--   * Revoking a grant deletes it, and inherited children go with it by foreign
--     key cascade rather than by application code (D18.3).
--   * Absence of scope is deny. The predicate hive-store composes from one text
--     is the only expression allowed to answer "may this actor do this."
--
-- The dialect, once (D38 §3):
--
--   * uuid is TEXT, lowercase hyphenated. The DEFAULT below mints a v4 so a raw
--     insert gets a real id; the host binds its own.
--   * Every timestamp is INTEGER microseconds since the Unix epoch, UTC. The
--     DEFAULT reads julianday('now') at millisecond resolution; the host binds
--     a microsecond clock. Nothing orders rows across the two.
--   * JSON is TEXT with json_valid() as the CHECK.
--   * There is no regex. The three alphabets this schema constrains (hex,
--     slug, event kind) are spelled with GLOB and length() instead, exactly.
--   * SQLite triggers are immediate and their messages are static. Where the
--     Postgres text interpolated an id, the message here names the rule and
--     the row is in the statement that failed.
--   * A trigger that reads a sibling row uses the row as it stands when the
--     trigger fires: BEFORE for a policy on the write itself, AFTER where the
--     rule has to see the new row in the table.

-- ---------------------------------------------------------------------------
-- Actors: users, AI identities and orgs in one addressing model (D1.2).
-- ---------------------------------------------------------------------------

CREATE TABLE actors (
    id             TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    kind           TEXT NOT NULL CHECK (kind IN ('human', 'ai', 'org')),
    handle         TEXT NOT NULL UNIQUE,
    display_name   TEXT NOT NULL DEFAULT '',

    -- D13.9: an AI identity is a per-principal instance of a persona, so it
    -- resolves to exactly one principal. A free-floating persona has nothing a
    -- grant can be written against, which makes every tag ambiguous.
    persona        TEXT,

    -- The principal this actor acts for. Humans and orgs are their own
    -- principal; an AI's principal is the human or org that owns it. An AI
    -- never appears as a principal (D13.4), which is enforced below.
    principal_kind TEXT NOT NULL CHECK (principal_kind IN ('user', 'org')),
    principal_id   TEXT NOT NULL REFERENCES actors (id),

    -- D19.1/D19.2. Exactly one actor may have no creator: the bootstrap root,
    -- guarded by the partial unique index below. A system whose first
    -- authorization can be requested over the network does not have a root.
    created_by_actor TEXT REFERENCES actors (id),

    meta           TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(meta)),
    created_at     INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    disabled_at    INTEGER,

    CONSTRAINT actors_persona_iff_ai
        CHECK ((kind = 'ai') = (persona IS NOT NULL)),

    -- A human or org actor is its own principal, and the principal kind follows
    -- from the actor kind. Only an AI points somewhere else.
    CONSTRAINT actors_self_principal
        CHECK (
            kind = 'ai'
            OR (principal_id = id
                AND principal_kind = CASE kind WHEN 'org' THEN 'org' ELSE 'user' END)
        ),
    CONSTRAINT actors_ai_is_not_its_own_principal
        CHECK (kind <> 'ai' OR principal_id <> id)
);

CREATE INDEX actors_principal_idx ON actors (principal_kind, principal_id);

-- There is exactly one root, and it is created out of band. Every other actor
-- names its creator, which is what makes D19.2 checkable.
CREATE UNIQUE INDEX actors_single_root ON actors ((created_by_actor IS NULL))
    WHERE created_by_actor IS NULL;

-- A CHECK cannot reach another row, and "an AI's principal is an AI" would
-- quietly break the authority ceiling in the predicate. Enforce it here, after
-- the row lands so a self-principal (a human or an org) can see itself.
CREATE TRIGGER actors_principal_check_insert
    AFTER INSERT ON actors
BEGIN
    SELECT RAISE(ABORT, 'actor has no principal row')
     WHERE NOT EXISTS (SELECT 1 FROM actors WHERE id = NEW.principal_id);
    SELECT RAISE(ABORT, 'an AI actor cannot be a principal')
     WHERE (SELECT kind FROM actors WHERE id = NEW.principal_id) = 'ai';
    SELECT RAISE(ABORT, 'principal_kind disagrees with the principal actor''s kind')
     WHERE ((SELECT kind FROM actors WHERE id = NEW.principal_id) = 'org') <> (NEW.principal_kind = 'org');
END;

CREATE TRIGGER actors_principal_check_update
    AFTER UPDATE OF principal_kind, principal_id ON actors
BEGIN
    SELECT RAISE(ABORT, 'actor has no principal row')
     WHERE NOT EXISTS (SELECT 1 FROM actors WHERE id = NEW.principal_id);
    SELECT RAISE(ABORT, 'an AI actor cannot be a principal')
     WHERE (SELECT kind FROM actors WHERE id = NEW.principal_id) = 'ai';
    SELECT RAISE(ABORT, 'principal_kind disagrees with the principal actor''s kind')
     WHERE ((SELECT kind FROM actors WHERE id = NEW.principal_id) = 'org') <> (NEW.principal_kind = 'org');
END;

-- D19.2: an org admin creates actors within their org; a person creates AI
-- persona instances owned by themselves. An AI never creates actors. Enforced
-- as a trigger rather than in a service, because "an AI cannot climb" has to
-- hold for any writer that reaches this database.
CREATE TRIGGER actors_creation_check
    AFTER INSERT ON actors
    WHEN NEW.created_by_actor IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'actor: creator does not exist')
     WHERE NOT EXISTS (SELECT 1 FROM actors WHERE id = NEW.created_by_actor);
    SELECT RAISE(ABORT, 'actor: an AI actor may not create actors (D19.2)')
     WHERE (SELECT kind FROM actors WHERE id = NEW.created_by_actor) = 'ai';
    -- A human or org actor is its own principal, so creating one confers no
    -- authority on the creator. Authority attaches when the new actor is seated
    -- in an org, and org_members carries that check. An AI persona instance IS
    -- authority ... it can act for its principal ... so the creator must be
    -- that person, or an admin of that org.
    SELECT RAISE(ABORT, 'actor: creator may not create an AI acting for that principal (D19.2)')
     WHERE NEW.kind = 'ai'
       AND NOT (NEW.principal_kind = 'user' AND NEW.principal_id = NEW.created_by_actor)
       AND NOT (NEW.principal_kind = 'org' AND EXISTS (
               SELECT 1 FROM org_members m
                WHERE m.org_id = NEW.principal_id
                  AND m.user_id = NEW.created_by_actor
                  AND m.role = 'admin'));
END;

-- Membership is where authority actually attaches, so this is where D19.2's
-- "an org admin, within their org" is enforced.
CREATE TABLE org_members (
    org_id         TEXT NOT NULL REFERENCES actors (id) ON DELETE CASCADE,
    user_id        TEXT NOT NULL REFERENCES actors (id) ON DELETE CASCADE,
    role           TEXT NOT NULL CHECK (role IN ('member', 'admin')),
    added_by_actor TEXT NOT NULL REFERENCES actors (id),
    created_at     INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    PRIMARY KEY (org_id, user_id)
);

CREATE INDEX org_members_user_idx ON org_members (user_id);

-- AFTER, so the first seat can see that no OTHER member exists yet.
CREATE TRIGGER org_members_check_insert
    AFTER INSERT ON org_members
BEGIN
    SELECT RAISE(ABORT, 'org_members.org_id is not an org')
     WHERE (SELECT kind FROM actors WHERE id = NEW.org_id) IS NOT 'org';
    SELECT RAISE(ABORT, 'org_members.user_id is not a human')
     WHERE (SELECT kind FROM actors WHERE id = NEW.user_id) IS NOT 'human';
    SELECT RAISE(ABORT, 'an AI actor may not seat members in an org (D19.2)')
     WHERE (SELECT kind FROM actors WHERE id = NEW.added_by_actor) = 'ai';
    -- An admin seats members. The first seat is the exception: the org's own
    -- creator becomes its first admin, and there is no membership row to check
    -- against yet.
    SELECT RAISE(ABORT, 'actor is not an admin of that org (D19.2)')
     WHERE NOT EXISTS (SELECT 1 FROM org_members m
                        WHERE m.org_id = NEW.org_id AND m.user_id = NEW.added_by_actor AND m.role = 'admin')
       AND NOT (
            (SELECT created_by_actor FROM actors WHERE id = NEW.org_id) IS NOT NULL
            AND (SELECT created_by_actor FROM actors WHERE id = NEW.org_id) = NEW.added_by_actor
            AND NOT EXISTS (SELECT 1 FROM org_members m WHERE m.org_id = NEW.org_id AND m.user_id <> NEW.user_id));
END;

CREATE TRIGGER org_members_check_update
    AFTER UPDATE ON org_members
BEGIN
    SELECT RAISE(ABORT, 'org_members.org_id is not an org')
     WHERE (SELECT kind FROM actors WHERE id = NEW.org_id) IS NOT 'org';
    SELECT RAISE(ABORT, 'org_members.user_id is not a human')
     WHERE (SELECT kind FROM actors WHERE id = NEW.user_id) IS NOT 'human';
    SELECT RAISE(ABORT, 'an AI actor may not seat members in an org (D19.2)')
     WHERE (SELECT kind FROM actors WHERE id = NEW.added_by_actor) = 'ai';
    SELECT RAISE(ABORT, 'actor is not an admin of that org (D19.2)')
     WHERE NOT EXISTS (SELECT 1 FROM org_members m
                        WHERE m.org_id = NEW.org_id AND m.user_id = NEW.added_by_actor AND m.role = 'admin')
       AND NOT (
            (SELECT created_by_actor FROM actors WHERE id = NEW.org_id) IS NOT NULL
            AND (SELECT created_by_actor FROM actors WHERE id = NEW.org_id) = NEW.added_by_actor
            AND NOT EXISTS (SELECT 1 FROM org_members m WHERE m.org_id = NEW.org_id AND m.user_id <> NEW.user_id));
END;

-- ---------------------------------------------------------------------------
-- Credentials (D17.4, D19.3). The credential is where author_actor and owner
-- principal enter every request as a PAIR. Without the pair on the request, the
-- pair can never be populated honestly on a row.
-- ---------------------------------------------------------------------------

CREATE TABLE credentials (
    id             TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    actor_id       TEXT NOT NULL REFERENCES actors (id) ON DELETE CASCADE,
    principal_kind TEXT NOT NULL CHECK (principal_kind IN ('user', 'org')),
    principal_id   TEXT NOT NULL REFERENCES actors (id),

    -- The token is never stored. Only its hash comes back here.
    token_sha256   TEXT NOT NULL UNIQUE
        CHECK (length(token_sha256) = 64 AND NOT (token_sha256 GLOB '*[^0-9a-f]*')),
    label          TEXT NOT NULL DEFAULT '',

    issued_by_actor          TEXT NOT NULL REFERENCES actors (id),
    issued_by_principal_kind TEXT NOT NULL CHECK (issued_by_principal_kind IN ('user', 'org')),
    issued_by_principal_id   TEXT NOT NULL REFERENCES actors (id),

    created_at     INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    expires_at     INTEGER,
    revoked_at     INTEGER,
    last_used_at   INTEGER
);

CREATE INDEX credentials_actor_idx ON credentials (actor_id) WHERE revoked_at IS NULL;

-- D19.3: a principal issues for itself, or an org admin for actors in their
-- org. An AI never issues credentials, which is the other half of "an AI cannot
-- climb" (the first half being D18's no-override-for-AI rule).
CREATE TRIGGER credentials_issue_check
    BEFORE INSERT ON credentials
BEGIN
    SELECT RAISE(ABORT, 'credential: an AI actor may not issue credentials (D19.3)')
     WHERE (SELECT kind FROM actors WHERE id = NEW.issued_by_actor) = 'ai';
    -- The credential's pair has to be one the subject actor can actually hold.
    SELECT RAISE(ABORT, 'credential: an AI actor is pinned to one principal')
     WHERE (SELECT kind FROM actors WHERE id = NEW.actor_id) = 'ai'
       AND NOT ((SELECT principal_kind FROM actors WHERE id = NEW.actor_id) IS NEW.principal_kind
                AND (SELECT principal_id FROM actors WHERE id = NEW.actor_id) IS NEW.principal_id);
    SELECT RAISE(ABORT, 'credential: actor cannot act for that principal')
     WHERE (SELECT kind FROM actors WHERE id = NEW.actor_id) IS NOT 'ai'
       AND NOT (
            (NEW.principal_kind = 'user' AND NEW.principal_id = NEW.actor_id)
            OR (NEW.principal_kind = 'org' AND EXISTS (
                    SELECT 1 FROM org_members m
                     WHERE m.org_id = NEW.principal_id AND m.user_id = NEW.actor_id)));
    -- A person issuing for themselves, which also covers a person issuing for
    -- an AI persona instance they own, since such an actor's principal IS them.
    --
    -- The test is against the issuing ACTOR, not against the issuing principal.
    -- Comparing principals looked equivalent and was not: a plain org member
    -- legitimately holds a credential of (actor = them, principal = the org),
    -- and presenting that pair read as "the principal issuing for itself",
    -- which let any member mint a credential naming ANOTHER member as
    -- author_actor. That forges "Nate did this", which is the one distinction
    -- invariant 2 exists to preserve. Otherwise: an org admin, for actors in
    -- their org. Membership and role, never a principal comparison.
    SELECT RAISE(ABORT, 'credential: issuer may not issue for that principal (D19.3)')
     WHERE NOT (NEW.principal_kind = 'user' AND NEW.principal_id = NEW.issued_by_actor)
       AND NOT (NEW.principal_kind = 'org' AND EXISTS (
                SELECT 1 FROM org_members m
                 WHERE m.org_id = NEW.principal_id
                   AND m.user_id = NEW.issued_by_actor
                   AND m.role = 'admin'));
END;

-- ---------------------------------------------------------------------------
-- Blobs. Bytes and references are separate tables because ownership,
-- permission and trust are properties of a reference (D17.1, D6 key layout).
-- ---------------------------------------------------------------------------

CREATE TABLE blobs (
    sha256      TEXT PRIMARY KEY
        CHECK (length(sha256) = 64 AND NOT (sha256 GLOB '*[^0-9a-f]*')),
    size        INTEGER NOT NULL CHECK (size >= 0),
    mime        TEXT NOT NULL DEFAULT 'application/octet-stream',
    driver      TEXT NOT NULL,
    driver_ref  TEXT,

    -- pending is the reservation (D6.5): reserve the row, release the lock,
    -- move the bytes, flip to live. Every crash window fails toward reclaimable
    -- litter rather than a live row pointing at nothing.
    state       TEXT NOT NULL CHECK (state IN ('pending', 'live', 'evicted', 'trashed')),

    class       TEXT NOT NULL CHECK (class IN ('derived', 'build', 'capture', 'original')),
    source_hash TEXT REFERENCES blobs (sha256),
    recipe      TEXT CHECK (recipe IS NULL OR json_valid(recipe)),

    created_at  INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    live_at     INTEGER,
    evicted_at  INTEGER,
    trashed_at  INTEGER,

    CONSTRAINT blobs_live_has_bytes
        CHECK (state <> 'live' OR driver_ref IS NOT NULL),
    CONSTRAINT blobs_trashed_at_set
        CHECK ((state = 'trashed') = (trashed_at IS NOT NULL)),
    -- Evictability needs class AND source AND recipe, or the host drops bytes
    -- believing it can get them back and then cannot say from what. A capture
    -- has none of the three and is structurally non-evictable.
    CONSTRAINT blobs_evictable_only_with_a_recipe
        CHECK (
            state <> 'evicted'
            OR (class IN ('derived', 'build')
                AND source_hash IS NOT NULL
                AND recipe IS NOT NULL)
        )
);

CREATE INDEX blobs_state_idx ON blobs (state, created_at);
CREATE INDEX blobs_source_idx ON blobs (source_hash) WHERE source_hash IS NOT NULL;

CREATE TABLE blob_refs (
    id           TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    sha256       TEXT NOT NULL REFERENCES blobs (sha256) ON DELETE RESTRICT,

    owner_kind   TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id     TEXT NOT NULL REFERENCES actors (id),
    author_actor TEXT NOT NULL REFERENCES actors (id),

    -- Every producer in the platform, not just host.storage.* (D17.5). A
    -- sweeper that does not know about modules deletes live modules.
    source_kind  TEXT NOT NULL CHECK (source_kind IN (
        'upload', 'collection', 'module', 'guest_source', 'transcript',
        'spool', 'screenshot', 'step_output', 'harness_diff', 'workflow_input'
    )),
    source_id    TEXT NOT NULL,

    -- D17.1. Trust rides the reference, never the bytes: global dedup makes an
    -- upload and a fetched page with identical bytes one blob row, and
    -- trusted-first would silently launder web content into trusted.
    trust        TEXT NOT NULL CHECK (trust IN ('trusted', 'untrusted')),

    created_at   INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    -- The mechanism to release a reference has to exist even when the policy is
    -- "keep forever", or the option cannot be exercised later (D6 retention).
    released_at  INTEGER,

    UNIQUE (sha256, owner_kind, owner_id, source_kind, source_id)
);

CREATE INDEX blob_refs_hash_idx ON blob_refs (sha256) WHERE released_at IS NULL;
CREATE INDEX blob_refs_owner_idx ON blob_refs (owner_kind, owner_id) WHERE released_at IS NULL;

-- Invariant: no blob exists without a ref, and whatever produced it writes one.
-- A pending reservation has no ref yet by design, so the rule binds at the flip
-- to live. The Postgres version was a deferred constraint trigger; SQLite's are
-- immediate, so the WRITE ORDER carries it: reserve the row, write the
-- reference, then flip to live. A writer that flips first is refused here.
CREATE TRIGGER blobs_live_ref_check_insert
    AFTER INSERT ON blobs
    WHEN NEW.state = 'live'
BEGIN
    SELECT RAISE(ABORT, 'blob cannot go live with no reference')
     WHERE NOT EXISTS (SELECT 1 FROM blob_refs r WHERE r.sha256 = NEW.sha256 AND r.released_at IS NULL);
END;

CREATE TRIGGER blobs_live_ref_check_update
    AFTER UPDATE OF state ON blobs
    WHEN NEW.state = 'live'
BEGIN
    SELECT RAISE(ABORT, 'blob cannot go live with no reference')
     WHERE NOT EXISTS (SELECT 1 FROM blob_refs r WHERE r.sha256 = NEW.sha256 AND r.released_at IS NULL);
END;

-- ---------------------------------------------------------------------------
-- Apps and installs (D1.1).
-- ---------------------------------------------------------------------------

-- D19.4, settled: building is unattended, promoting is a human act. So a
-- registered build with no install is a NORMAL RESTING STATE, not an error and
-- not an incomplete record. The columns below exist so promotion can be an
-- informed act: which build, from which source, produced by which run, with
-- which test outcome, waiting on which app. Those are facts on the build row
-- rather than a UI problem to solve later.
CREATE TABLE app_builds (
    id            TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    -- Bounded, because a slug is not only a name. It is the first segment of
    -- every event kind this app emits (`<slug>.<collection>.<verb>`), and
    -- events.kind is constrained; an unbounded slug would let a manifest emit
    -- a kind the events table refuses, so the app's writes would fail at the
    -- point of use rather than at registration. Same alphabet as the kind, so
    -- one cannot produce a value the other rejects.
    slug          TEXT NOT NULL
        CHECK (length(slug) BETWEEN 1 AND 63 AND slug GLOB '[a-z0-9]*' AND NOT (slug GLOB '*[^a-z0-9-]*')),
    version       TEXT NOT NULL DEFAULT '',
    kind          TEXT NOT NULL CHECK (kind IN ('app', 'tool')),
    impl          TEXT NOT NULL DEFAULT 'wasm' CHECK (impl IN ('wasm', 'host')),

    -- NULL for impl='host' builtins, which have no module bytes.
    module_sha256 TEXT REFERENCES blobs (sha256),
    -- The guest source that produced the module. 'build' class blobs are
    -- rebuildable only if source AND toolchain are both recorded (D6).
    source_sha256 TEXT REFERENCES blobs (sha256),
    toolchain     TEXT NOT NULL DEFAULT '',

    manifest      TEXT NOT NULL CHECK (json_valid(manifest)),
    content_hash  TEXT NOT NULL UNIQUE
        CHECK (length(content_hash) = 64 AND NOT (content_hash GLOB '*[^0-9a-f]*')),

    -- The surface this build exposes: its tools and routes after generated CRUD
    -- and overrides resolve. Recorded rather than recomputed, and the
    -- distinction is the whole point ... a recomputed hash says "this is what we
    -- would derive now", where a promotion reviewer needs "this is what a human
    -- approved". If the deriver ever changes, every historical hash silently
    -- changes meaning and the comparison starts measuring today's deriver
    -- against itself.
    surface_hash  TEXT
        CHECK (surface_hash IS NULL OR (length(surface_hash) = 64 AND NOT (surface_hash GLOB '*[^0-9a-f]*'))),

    -- WHICH deriver produced it. Without this, two hashes that differ are
    -- ambiguous between "the app changed" and "we changed", which is exactly
    -- the question the hash exists to answer.
    derive_version INTEGER,

    -- What produced it. A build with no run is hand-written and first-party;
    -- a build with one came from the builder loop and says so.
    built_by_run_id TEXT,

    -- The test outcome, attached to the build rather than living in a log
    -- somebody has to go find.
    test_state    TEXT NOT NULL DEFAULT 'untested'
        CHECK (test_state IN ('untested', 'passed', 'failed')),
    test_summary  TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(test_summary)),
    tested_at     INTEGER,

    -- D17.13: author_kind (user|org) cannot represent "Colette built this for
    -- Nate." Same author/owner split as every other content row.
    author_actor  TEXT NOT NULL REFERENCES actors (id),
    owner_kind    TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id      TEXT NOT NULL REFERENCES actors (id),

    visibility    TEXT NOT NULL CHECK (visibility IN ('private', 'org', 'shared')),
    -- D10.9: trust tier sets default capabilities. Tools are cheap to create,
    -- which is exactly why 'local' starts with none.
    trust         TEXT NOT NULL CHECK (trust IN ('builtin', 'local', 'imported')),

    -- 'registered' is where a build waits for a human, indefinitely and
    -- correctly. It is deliberately not called 'pending'.
    status        TEXT NOT NULL CHECK (status IN ('building', 'registered', 'failed', 'withdrawn')),

    created_at    INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),

    -- Both or neither. A hash with no deriver is a number nobody can interpret.
    CONSTRAINT app_builds_surface_is_attributable
        CHECK ((surface_hash IS NULL) = (derive_version IS NULL)),
    CONSTRAINT app_builds_wasm_has_a_module
        CHECK ((impl = 'wasm') = (module_sha256 IS NOT NULL)),
    CONSTRAINT app_builds_tools_have_no_storage
        CHECK (kind <> 'tool' OR json_type(manifest, '$.storage') IS NULL),
    CONSTRAINT app_builds_tested_at_recorded
        CHECK ((test_state = 'untested') = (tested_at IS NULL))
);

CREATE INDEX app_builds_slug_idx ON app_builds (slug, created_at DESC);
CREATE INDEX app_builds_owner_idx ON app_builds (owner_kind, owner_id);
CREATE INDEX app_builds_registered_idx ON app_builds (slug, created_at DESC)
    WHERE status = 'registered';

-- D19.4 is the reason app_builds and installs do not look alike. Producing a
-- content-addressed module is not privileged, so the builder loop writes
-- app_builds unattended. Making one live is a distinct act, and the columns
-- below are what make that distinction structural rather than a convention a
-- handler is trusted to remember.
--
-- ---------------------------------------------------------------------------
-- Install authority: a WRITE-PATH capability, in its own table (D20).
--
-- This used to be an ordinary `write` grant on the install subject, and that
-- was wrong in a way that only showed up when somebody asked what happens if
-- you give one to a delegate rather than to yourself: a grant on the install
-- subject is read by the ordinary predicate, so "may roll a rebuilt tool into
-- this app" also handed out general write on the install. One table carrying
-- two meanings, which is the trap this design has fallen into four times.
--
-- Nothing here is consulted by the predicate. Holding an install authority
-- confers no visibility whatsoever, and there is a test that says so.
-- ---------------------------------------------------------------------------
CREATE TABLE install_authorities (
    id          TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    -- The foreign key names a table created below; SQLite resolves it at
    -- write time, so the order of the two CREATEs does not matter here.
    install_id  TEXT NOT NULL REFERENCES installs (id) ON DELETE CASCADE,

    -- Who holds it. A principal, never an actor: an AI does not hold authority
    -- of its own, it acts for one that does.
    holder_kind TEXT NOT NULL CHECK (holder_kind IN ('user', 'org')),
    holder_id   TEXT NOT NULL REFERENCES actors (id) ON DELETE CASCADE,

    -- What it permits, unattended. One value today; the column exists so that
    -- adding "may uninstall" later is a value rather than a second table.
    capability  TEXT NOT NULL CHECK (capability IN ('activate')),

    -- Always a human. Without this the rule is decorative: an AI acting for the
    -- install's owner would simply mint its own and promote its own output.
    granted_by_actor          TEXT NOT NULL REFERENCES actors (id),
    granted_by_principal_kind TEXT NOT NULL CHECK (granted_by_principal_kind IN ('user', 'org')),
    granted_by_principal_id   TEXT NOT NULL REFERENCES actors (id),
    reason      TEXT NOT NULL DEFAULT '',

    created_at  INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    expires_at  INTEGER,
    revoked_at  INTEGER,
    revoked_by  TEXT REFERENCES actors (id),

    CONSTRAINT install_authorities_revocation_is_attributed
        CHECK ((revoked_at IS NULL) = (revoked_by IS NULL)),
    UNIQUE (install_id, holder_kind, holder_id, capability)
);

CREATE INDEX install_authorities_live_idx
    ON install_authorities (install_id, holder_kind, holder_id)
    WHERE revoked_at IS NULL;

CREATE TABLE installs (
    id                 TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    build_id           TEXT NOT NULL REFERENCES app_builds (id),
    -- Same alphabet as app_builds.slug, and for the same reason: this is the
    -- one the data layer actually reads when it composes an event kind.
    slug               TEXT NOT NULL
        CHECK (length(slug) BETWEEN 1 AND 63 AND slug GLOB '[a-z0-9]*' AND NOT (slug GLOB '*[^a-z0-9-]*')),

    -- An install is owned by the scope it is installed into; the columns are
    -- named owner_* so every grant-filtered read looks the same.
    owner_kind         TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id           TEXT NOT NULL REFERENCES actors (id),
    installed_by_actor TEXT NOT NULL REFERENCES actors (id),

    -- Exactly one of these authorises an active install: a human principal, or
    -- a standing authority on this install (which is how an unattended rebuild
    -- rolls a new build into an app a human already stood up).
    activated_by_actor      TEXT REFERENCES actors (id),
    activation_authority_id TEXT REFERENCES install_authorities (id) ON DELETE RESTRICT,

    -- The prefix of this install's collection tables: `<schema_name>__<collection>`.
    -- One file holds every install's tables today (D38 phase 1); the name is
    -- derived from the INSTALL, never from the app alone, because two installs
    -- of one app are two schemas (invariant 14, the sixth instance).
    schema_name        TEXT NOT NULL UNIQUE,
    state              TEXT NOT NULL CHECK (state IN ('active', 'disabled', 'uninstalling')),
    created_at         INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),

    CONSTRAINT installs_active_is_authorised
        CHECK (state <> 'active'
               OR activated_by_actor IS NOT NULL
               OR activation_authority_id IS NOT NULL),

    UNIQUE (slug, owner_kind, owner_id)
);

CREATE INDEX installs_owner_idx ON installs (owner_kind, owner_id) WHERE state = 'active';

-- A trigger can check that the named activator is a human. It CANNOT check
-- that the named activator is the actor on the credential, because there is
-- no credential in scope here ... so an AI could register a build and
-- activate it by naming a human in this column.
--
-- That binding lives in store::activate_install, which sets this column from
-- the credential's actor and refuses anything else. Do not write to
-- installs.state directly; the schema looks like it handles this and it only
-- handles half.
CREATE TRIGGER installs_activation_check_insert
    AFTER INSERT ON installs
    WHEN NEW.state = 'active'
BEGIN
    SELECT RAISE(ABORT, 'install: activation needs a human principal or a standing grant (D19.4)')
     WHERE NEW.activated_by_actor IS NOT NULL
       AND (SELECT kind FROM actors WHERE id = NEW.activated_by_actor) IS NOT 'human';
    -- The standing authority is scoped to this one install, which is what
    -- "scoped to one specific app" buys: it cannot promote a build into
    -- anything else, and it grants no visibility into anything at all.
    SELECT RAISE(ABORT, 'install: authority is not a live activate authority on this install')
     WHERE NEW.activated_by_actor IS NULL
       AND NOT EXISTS (
            SELECT 1 FROM install_authorities ia
             WHERE ia.id = NEW.activation_authority_id
               AND ia.install_id = NEW.id
               AND ia.capability = 'activate'
               AND ia.revoked_at IS NULL
               AND (ia.expires_at IS NULL OR ia.expires_at > (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER))));
END;

CREATE TRIGGER installs_activation_check_update
    AFTER UPDATE OF state, activated_by_actor, activation_authority_id ON installs
    WHEN NEW.state = 'active'
BEGIN
    SELECT RAISE(ABORT, 'install: activation needs a human principal or a standing grant (D19.4)')
     WHERE NEW.activated_by_actor IS NOT NULL
       AND (SELECT kind FROM actors WHERE id = NEW.activated_by_actor) IS NOT 'human';
    SELECT RAISE(ABORT, 'install: authority is not a live activate authority on this install')
     WHERE NEW.activated_by_actor IS NULL
       AND NOT EXISTS (
            SELECT 1 FROM install_authorities ia
             WHERE ia.id = NEW.activation_authority_id
               AND ia.install_id = NEW.id
               AND ia.capability = 'activate'
               AND ia.revoked_at IS NULL
               AND (ia.expires_at IS NULL OR ia.expires_at > (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER))));
END;

-- Who may write one. The same shape as the grant issue policy, for the same
-- reason: it has to hold for every writer that reaches this schema, not only
-- for the ones that remember to call a service.
--
-- The acting-actor rule inlined below is the same expression the predicate in
-- hive-store composes (`ACTING_KIND`). Two copies, on purpose and on the
-- record (D38 §2): a trigger cannot call into the binary.
CREATE TRIGGER install_authorities_issue_policy
    BEFORE INSERT ON install_authorities
BEGIN
    SELECT RAISE(ABORT, 'install authority: only a human may delegate unattended activation (D19.4)')
     WHERE (SELECT kind FROM actors WHERE id = NEW.granted_by_actor) IS NOT 'human';
    SELECT RAISE(ABORT, 'install authority: install does not exist')
     WHERE NOT EXISTS (SELECT 1 FROM installs WHERE id = NEW.install_id);
    -- Only the owning principal delegates authority over its own install, and
    -- the granting actor has to actually be bound to that principal.
    SELECT RAISE(ABORT, 'install authority: only the owning principal may delegate on this install')
     WHERE NOT ((SELECT owner_kind FROM installs WHERE id = NEW.install_id) IS NEW.granted_by_principal_kind
                AND (SELECT owner_id FROM installs WHERE id = NEW.install_id) IS NEW.granted_by_principal_id);
    SELECT RAISE(ABORT, 'install authority: granting actor is not bound to that principal')
     WHERE (CASE
              WHEN NEW.granted_by_actor IS NULL OR NEW.granted_by_principal_kind IS NULL OR NEW.granted_by_principal_id IS NULL THEN NULL
              WHEN (SELECT disabled_at FROM actors WHERE id = NEW.granted_by_actor) IS NOT NULL THEN NULL
              WHEN (SELECT kind FROM actors WHERE id = NEW.granted_by_actor) = 'ai' THEN
                   CASE WHEN (SELECT principal_kind FROM actors WHERE id = NEW.granted_by_actor) IS NEW.granted_by_principal_kind
                         AND (SELECT principal_id FROM actors WHERE id = NEW.granted_by_actor) IS NEW.granted_by_principal_id
                        THEN 'ai' END
              WHEN (SELECT kind FROM actors WHERE id = NEW.granted_by_actor) = 'human' THEN
                   CASE WHEN (NEW.granted_by_principal_kind = 'user' AND NEW.granted_by_principal_id = NEW.granted_by_actor)
                          OR (NEW.granted_by_principal_kind = 'org' AND EXISTS (
                                  SELECT 1 FROM org_members m
                                   WHERE m.org_id = NEW.granted_by_principal_id AND m.user_id = NEW.granted_by_actor))
                        THEN 'human' END
            END) IS NULL;
END;

-- Immutable except for revocation, for the same reason grants are: without it,
-- UPDATE walks around every rule above.
CREATE TRIGGER install_authorities_immutability
    BEFORE UPDATE ON install_authorities
    WHEN NEW.id IS NOT OLD.id
      OR NEW.install_id IS NOT OLD.install_id
      OR NEW.holder_kind IS NOT OLD.holder_kind
      OR NEW.holder_id IS NOT OLD.holder_id
      OR NEW.capability IS NOT OLD.capability
      OR NEW.granted_by_actor IS NOT OLD.granted_by_actor
      OR NEW.granted_by_principal_kind IS NOT OLD.granted_by_principal_kind
      OR NEW.granted_by_principal_id IS NOT OLD.granted_by_principal_id
      OR NEW.created_at IS NOT OLD.created_at
      OR NEW.expires_at IS NOT OLD.expires_at
BEGIN
    SELECT RAISE(ABORT, 'an install authority is immutable except for revoked_at and revoked_by');
END;

-- What is waiting on a human, with everything needed to decide. Promotion is
-- meant to be informed rather than a rubber stamp, so the facts live here and
-- not in whatever the first UI happens to join together.
-- D25: promotion IS the capability decision, so the promotion surface has to
-- show capabilities.
--
-- There is no per-capability grant and there deliberately is not going to be
-- one: capabilities are content-addressed with the build, so "install it but
-- deny egress" yields an app that does not work as built, and the granularity
-- people actually want is finer and already exists elsewhere (the egress
-- allowlist rather than the egress capability, the agent budget rather than the
-- agent_run capability). Declaring is granting, at install granularity.
--
-- Which puts the whole weight on this view. The capability set is sorted and
-- deduplicated so that a reordered manifest is not mistaken for a change ...
-- JSON array equality is order-sensitive, and a false "capabilities changed"
-- trains people to click through the true one. The set expression is written
-- out four times because a view cannot name a function; each copy is
-- `json_group_array` over `DISTINCT ... ORDER BY` of the manifest's array.
CREATE VIEW builds_awaiting_promotion AS
SELECT b.id            AS build_id,
       b.slug,
       b.kind,
       b.version,
       b.content_hash,
       b.module_sha256,
       b.source_sha256,
       b.toolchain,
       b.built_by_run_id,
       b.author_actor,
       b.owner_kind,
       b.owner_id,
       b.trust,
       b.test_state,
       b.test_summary,
       b.tested_at,
       b.created_at,
       -- Which app it is waiting on, and what is live there now, so the
       -- decision is "replace this with that" rather than "approve a hash".
       i.id            AS current_install_id,
       i.build_id      AS current_build_id,
       i.state         AS current_install_state,

       -- What this build is asking for, and what is live now (D25).
       (SELECT json_group_array(value) FROM (
            SELECT DISTINCT value FROM json_each(coalesce(json_extract(b.manifest, '$.capabilities'), '[]')) ORDER BY value))
           AS capabilities,
       CASE WHEN cb.id IS NULL THEN NULL ELSE
       (SELECT json_group_array(value) FROM (
            SELECT DISTINCT value FROM json_each(coalesce(json_extract(cb.manifest, '$.capabilities'), '[]')) ORDER BY value))
       END AS current_capabilities,

       -- The one that matters. A build whose capabilities differ from the live
       -- install is a CHANGE, not an equivalent promotion, and the risk is an
       -- app that has never had egress gaining it in v2 and being promoted as
       -- routine. Null for a first install, where there is nothing to compare
       -- against and every capability is new by definition.
       CASE WHEN i.build_id IS NULL THEN NULL
            ELSE (SELECT json_group_array(value) FROM (
                      SELECT DISTINCT value FROM json_each(coalesce(json_extract(b.manifest, '$.capabilities'), '[]')) ORDER BY value))
                 IS NOT
                 (SELECT json_group_array(value) FROM (
                      SELECT DISTINCT value FROM json_each(coalesce(json_extract(cb.manifest, '$.capabilities'), '[]')) ORDER BY value))
       END AS capability_change,

       -- Capabilities this build gains over the live one, so the reviewer reads
       -- the delta rather than diffing two arrays by eye.
       CASE WHEN i.build_id IS NULL THEN NULL
            ELSE (SELECT json_group_array(value) FROM (
                      SELECT DISTINCT value
                        FROM json_each(coalesce(json_extract(b.manifest, '$.capabilities'), '[]'))
                       WHERE value NOT IN (SELECT value FROM json_each(coalesce(json_extract(cb.manifest, '$.capabilities'), '[]')))
                       ORDER BY value))
       END AS capabilities_gained,

       b.surface_hash,
       b.derive_version,
       cb.surface_hash   AS current_surface_hash,
       cb.derive_version AS current_derive_version,

       -- Whether the tool and route surface moved.
       --
       -- Three-valued on purpose, and the null is the interesting one. If the
       -- two builds were derived by DIFFERENT derivers, the hashes are not
       -- comparable and saying "changed" would be a guess dressed as a fact ...
       -- the reviewer cannot tell "the app changed" from "we changed", which is
       -- precisely what derive_version exists to expose. Null means unanswerable
       -- and should read as "look at the surface yourself", not as "no change".
       CASE WHEN i.build_id IS NULL THEN NULL
            WHEN b.surface_hash IS NULL OR cb.surface_hash IS NULL THEN NULL
            WHEN b.derive_version IS NOT cb.derive_version THEN NULL
            ELSE b.surface_hash IS NOT cb.surface_hash
       END AS surface_change
  FROM app_builds b
  LEFT JOIN installs i
         ON i.slug = b.slug
        AND i.owner_kind = b.owner_kind
        AND i.owner_id = b.owner_id
  LEFT JOIN app_builds cb ON cb.id = i.build_id
 WHERE b.status = 'registered'
   AND (i.build_id IS NULL OR i.build_id <> b.id);

-- ---------------------------------------------------------------------------
-- Entities and links: the shared composition layer (D3.4).
-- ---------------------------------------------------------------------------

CREATE TABLE entities (
    id           TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    kind         TEXT NOT NULL,
    install_id   TEXT NOT NULL REFERENCES installs (id) ON DELETE CASCADE,
    collection   TEXT NOT NULL,
    ref          TEXT NOT NULL,

    owner_kind   TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id     TEXT NOT NULL REFERENCES actors (id),
    author_actor TEXT NOT NULL REFERENCES actors (id),

    trust        TEXT NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),
    -- Which operation first weakened the invocation that wrote this row.
    -- Diagnostic only; see events.tainted_by.
    tainted_by   TEXT,
    -- D17.12: cause_depth rides everything a run produces, not just mentions,
    -- or it cannot propagate and the loop guard has nothing to count.
    cause_depth  INTEGER NOT NULL DEFAULT 0 CHECK (cause_depth >= 0),
    run_id       TEXT,

    created_at   INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    updated_at   INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    deleted_at   INTEGER,

    UNIQUE (install_id, collection, ref)
);

CREATE INDEX entities_owner_idx ON entities (owner_kind, owner_id, kind) WHERE deleted_at IS NULL;
CREATE INDEX entities_collection_idx ON entities (install_id, collection) WHERE deleted_at IS NULL;

CREATE TABLE links (
    id           TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    kind         TEXT NOT NULL,
    src_id       TEXT NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    dst_id       TEXT NOT NULL REFERENCES entities (id) ON DELETE CASCADE,

    owner_kind   TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id     TEXT NOT NULL REFERENCES actors (id),
    author_actor TEXT NOT NULL REFERENCES actors (id),

    meta         TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(meta)),
    created_at   INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),

    UNIQUE (kind, src_id, dst_id)
);

CREATE INDEX links_dst_idx ON links (dst_id, kind);

-- ---------------------------------------------------------------------------
-- Conversations: a platform feature, not an app, and a subject kind of its own.
--
-- The alternative was making a conversation an `entities` row, which looks free
-- and is not: entities.install_id is NOT NULL -> installs.build_id is NOT NULL
-- -> app_builds. A chat would therefore need a synthetic build row that
-- describes no build, and a per-owner install, for a platform feature that is
-- not an app. Adding the kind is one `UNION` arm in subject_owners below and
-- one value in two CHECKs; the predicate never enumerates kinds.
-- ---------------------------------------------------------------------------

CREATE TABLE conversations (
    id           TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),

    -- Invariant 2 again: who created it, and whose authority it belongs to.
    author_actor TEXT NOT NULL REFERENCES actors (id),
    owner_kind   TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id     TEXT NOT NULL REFERENCES actors (id),

    -- Which agent this conversation is with, and how it runs. Pinned at
    -- creation so a resumed session cannot silently change model mid-thread.
    runtime      TEXT NOT NULL,
    model        TEXT NOT NULL DEFAULT '',

    title        TEXT NOT NULL DEFAULT '',
    created_at   INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    updated_at   INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    archived_at  INTEGER
);

CREATE INDEX conversations_owner_idx
    ON conversations (owner_kind, owner_id, updated_at DESC)
    WHERE archived_at IS NULL;

-- ---------------------------------------------------------------------------
-- Where a subject's authority lives: the one table the predicate and the grant
-- triggers resolve an owner through. tool, route and collection are
-- install-scoped, so subject_id is the install id for all three. A new
-- grantable kind is one more arm here and nothing else (D3's property).
--
-- A view rather than a function because SQLite has no stored functions; the
-- planner pushes `WHERE subject_kind = ? AND subject_id = ?` into each arm,
-- so a lookup is one indexed probe.
-- ---------------------------------------------------------------------------
CREATE VIEW subject_owners (subject_kind, subject_id, owner_kind, owner_id) AS
    SELECT 'entity', e.id, e.owner_kind, e.owner_id FROM entities e
    UNION ALL
    SELECT 'conversation', c.id, c.owner_kind, c.owner_id FROM conversations c
    UNION ALL
    SELECT 'install', i.id, i.owner_kind, i.owner_id FROM installs i
    UNION ALL
    SELECT 'tool', i.id, i.owner_kind, i.owner_id FROM installs i
    UNION ALL
    SELECT 'route', i.id, i.owner_kind, i.owner_id FROM installs i
    UNION ALL
    SELECT 'collection', i.id, i.owner_kind, i.owner_id FROM installs i;

-- ---------------------------------------------------------------------------
-- Grants (D1.3, D18, D33). One table, allowlist only, no deny rows.
-- ---------------------------------------------------------------------------

CREATE TABLE grants (
    id             TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),

    -- subject_id is the install id for install/tool/route/collection, the
    -- entity id for entity, the conversation id for conversation. subject_name
    -- qualifies the three install-scoped kinds. Keeping the install id in a
    -- column is what makes the allowlist rule a single query instead of a join
    -- through the manifest.
    subject_kind   TEXT NOT NULL
        CHECK (subject_kind IN ('install', 'tool', 'route', 'collection', 'entity', 'conversation')),
    subject_id     TEXT NOT NULL,
    subject_name   TEXT,

    -- D33: an install is a target too, so an app can be granted another's
    -- collection. An install is not an actor, so it cannot live in target_id's
    -- foreign key; a second column keeps BOTH references real, and a deleted
    -- install cascades its grants away exactly as a deleted actor does.
    target_kind    TEXT NOT NULL CHECK (target_kind IN ('user', 'org', 'install')),
    target_id      TEXT REFERENCES actors (id) ON DELETE CASCADE,
    target_install_id TEXT REFERENCES installs (id) ON DELETE CASCADE,
    access         TEXT NOT NULL CHECK (access IN ('read', 'write', 'call')),

    source         TEXT NOT NULL CHECK (source IN ('direct', 'inherited', 'override')),

    -- D18.3: inheritance is materialized. Revocation of a parent deletes every
    -- inherited child through this cascade, so the invariant is a foreign key
    -- rather than a code path someone can forget to call.
    inherited_from TEXT REFERENCES grants (id) ON DELETE CASCADE,

    -- Provenance: a grantee can see why they can see something (D13.15).
    granted_by_actor          TEXT NOT NULL REFERENCES actors (id),
    granted_by_principal_kind TEXT NOT NULL CHECK (granted_by_principal_kind IN ('user', 'org')),
    granted_by_principal_id   TEXT NOT NULL REFERENCES actors (id),
    reason         TEXT NOT NULL DEFAULT '',

    created_at     INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    expires_at     INTEGER,

    -- The tombstone, and it is deliberately NOT a second table. A revoked row
    -- is invisible to the predicate; it exists only so the inheritance
    -- materializer does not resurrect a deliberately narrowed child. The read
    -- path therefore still has exactly one policy: a live grant, or deny.
    revoked_at     INTEGER,
    revoked_by     TEXT REFERENCES actors (id),

    CONSTRAINT grants_inherited_iff_parent
        CHECK ((source = 'inherited') = (inherited_from IS NOT NULL)),
    -- D18.2: break-glass, never ambient.
    CONSTRAINT grants_override_is_time_boxed
        CHECK (source <> 'override' OR expires_at IS NOT NULL),
    -- Every case the design describes for override is a read. Widening this to
    -- write should be a deliberate migration, not an accident.
    CONSTRAINT grants_override_is_read_only
        CHECK (source <> 'override' OR access = 'read'),
    CONSTRAINT grants_named_subjects
        CHECK ((subject_kind IN ('install', 'entity', 'conversation')) = (subject_name IS NULL)),
    CONSTRAINT grants_revocation_is_attributed
        CHECK ((revoked_at IS NULL) = (revoked_by IS NULL)),
    -- Exactly one of the two target columns is set, and which one is decided
    -- by target_kind rather than by whichever the writer happened to fill in.
    CONSTRAINT grants_target_shape CHECK (
        (target_kind IN ('user', 'org') AND target_id IS NOT NULL AND target_install_id IS NULL)
     OR (target_kind = 'install'       AND target_id IS NULL     AND target_install_id IS NOT NULL)
    ),
    -- An install grant is a derived consequence of a manifest declaration that
    -- a human activated (D19), never a break-glass. Override is a human-only,
    -- time-boxed, org-owned path and an install is none of those things.
    CONSTRAINT grants_install_target_is_not_override
        CHECK (target_kind <> 'install' OR source <> 'override')
);

-- Nulls collapse to '' so two install-subject rows (subject_name NULL) collide
-- rather than silently duplicating; SQLite treats NULLs as distinct in a
-- unique index and this is the standing workaround.
--
-- Override rows are excluded, and that exclusion is load-bearing rather than
-- tidy. source and expires_at are not in the key, so with overrides included a
-- second break-glass on the same subject by the same admin collided with the
-- first ... forever, including after the first had expired, because nothing
-- reaps expired grants. Break-glass is the one path that has to work at 3am
-- under stress, and it worked exactly once per (subject, admin) for the life of
-- the database. An incident is inherently a repeatable event; an ordinary grant
-- is a statement of fact, and only the latter needs to be unique.
--
-- target_install_id is IN the key. Two installs holding the same collection
-- grant are two facts, and a key without the column would make the second
-- one a duplicate of the first (invariant 14).
CREATE UNIQUE INDEX grants_identity_uq ON grants (
    subject_kind, subject_id, coalesce(subject_name, ''),
    target_kind, coalesce(target_id, ''), coalesce(target_install_id, ''),
    access, coalesce(inherited_from, '')
) WHERE source <> 'override';

CREATE INDEX grants_override_idx ON grants (subject_kind, subject_id, target_id)
    WHERE source = 'override';

CREATE INDEX grants_lookup_idx ON grants (subject_kind, subject_id, target_kind, target_id)
    WHERE revoked_at IS NULL;
CREATE INDEX grants_target_idx ON grants (target_kind, target_id) WHERE revoked_at IS NULL;
CREATE INDEX grants_parent_idx ON grants (inherited_from) WHERE inherited_from IS NOT NULL;
CREATE INDEX grants_install_target_idx
    ON grants (target_install_id, subject_kind, subject_id, subject_name)
 WHERE target_install_id IS NOT NULL AND revoked_at IS NULL;

-- ---------------------------------------------------------------------------
-- Who may WRITE a grant (D13.14, D18.2, D19.3).
--
-- The predicate answers "may this actor see this." This answers the other
-- half, and it is the half that decides whether an AI can climb: a writer must
-- not be able to end a transaction holding authority its principal did not
-- already have. The checks are in the order the Postgres function evaluated
-- them, and each refusal names its rule.
--
-- The acting-actor rule is inlined here as it is in install_authorities above
-- and in hive-store's `ACTING_KIND`; three copies, on the record (D38 §2).
-- ---------------------------------------------------------------------------
CREATE TRIGGER grants_issue_policy
    BEFORE INSERT ON grants
BEGIN
    SELECT RAISE(ABORT, 'grant refused: subject does not exist')
     WHERE NOT EXISTS (SELECT 1 FROM subject_owners so
                        WHERE so.subject_kind = NEW.subject_kind AND so.subject_id = NEW.subject_id);
    SELECT RAISE(ABORT, 'grant refused: granting actor does not exist')
     WHERE NOT EXISTS (SELECT 1 FROM actors WHERE id = NEW.granted_by_actor);
    SELECT RAISE(ABORT, 'grant refused: granting actor is not bound to that principal')
     WHERE (CASE
              WHEN (SELECT disabled_at FROM actors WHERE id = NEW.granted_by_actor) IS NOT NULL THEN NULL
              WHEN (SELECT kind FROM actors WHERE id = NEW.granted_by_actor) = 'ai' THEN
                   CASE WHEN (SELECT principal_kind FROM actors WHERE id = NEW.granted_by_actor) IS NEW.granted_by_principal_kind
                         AND (SELECT principal_id FROM actors WHERE id = NEW.granted_by_actor) IS NEW.granted_by_principal_id
                        THEN 'ai' END
              WHEN (SELECT kind FROM actors WHERE id = NEW.granted_by_actor) = 'human' THEN
                   CASE WHEN (NEW.granted_by_principal_kind = 'user' AND NEW.granted_by_principal_id = NEW.granted_by_actor)
                          OR (NEW.granted_by_principal_kind = 'org' AND EXISTS (
                                  SELECT 1 FROM org_members m
                                   WHERE m.org_id = NEW.granted_by_principal_id AND m.user_id = NEW.granted_by_actor))
                        THEN 'human' END
            END) IS NULL;

    -- D18.2: produced by policy, org-owned rows only, human admin only. An AI
    -- never holds override and therefore never mints one either.
    SELECT RAISE(ABORT, 'grant refused: only a human actor may enter break-glass (D18.2)')
     WHERE NEW.source = 'override'
       AND (SELECT kind FROM actors WHERE id = NEW.granted_by_actor) <> 'human';
    SELECT RAISE(ABORT, 'grant refused: override never reaches a personally-owned row (D18.2)')
     WHERE NEW.source = 'override'
       AND (SELECT owner_kind FROM subject_owners so
             WHERE so.subject_kind = NEW.subject_kind AND so.subject_id = NEW.subject_id) <> 'org';
    SELECT RAISE(ABORT, 'grant refused: break-glass requires admin of the owning org (D18.2)')
     WHERE NEW.source = 'override'
       AND NOT EXISTS (SELECT 1 FROM org_members m
                        WHERE m.org_id = (SELECT owner_id FROM subject_owners so
                                           WHERE so.subject_kind = NEW.subject_kind AND so.subject_id = NEW.subject_id)
                          AND m.user_id = NEW.granted_by_actor AND m.role = 'admin');

    -- Sharing is not transfer (D13.10), and it is not laundering either: only
    -- the owner's principal may widen a row. A grantee reads and replies.
    SELECT RAISE(ABORT, 'grant refused: only the owning principal may grant on this subject')
     WHERE NEW.source <> 'override'
       AND NOT ((SELECT owner_kind FROM subject_owners so
                  WHERE so.subject_kind = NEW.subject_kind AND so.subject_id = NEW.subject_id) IS NEW.granted_by_principal_kind
                AND (SELECT owner_id FROM subject_owners so
                      WHERE so.subject_kind = NEW.subject_kind AND so.subject_id = NEW.subject_id) IS NEW.granted_by_principal_id);

    -- D13.14: a tag is an exfiltration primitive once an AI can write one. An
    -- AI may share with its own principal (widening nothing), with an org its
    -- principal belongs to, or with a member principal of the same org.
    -- Anything else needs a standing grant or a human.
    SELECT RAISE(ABORT, 'grant refused: AI-authored share crosses a principal boundary; needs a standing grant or human confirmation (D13.14)')
     WHERE NEW.source <> 'override'
       AND (SELECT kind FROM actors WHERE id = NEW.granted_by_actor) = 'ai'
       AND NOT (NEW.target_kind = (SELECT principal_kind FROM actors WHERE id = NEW.granted_by_actor)
                AND NEW.target_id = (SELECT principal_id FROM actors WHERE id = NEW.granted_by_actor))
       AND NOT (NEW.target_kind = 'org' AND (
                   ((SELECT principal_kind FROM actors WHERE id = NEW.granted_by_actor) = 'org'
                    AND (SELECT principal_id FROM actors WHERE id = NEW.granted_by_actor) = NEW.target_id)
                OR ((SELECT principal_kind FROM actors WHERE id = NEW.granted_by_actor) = 'user'
                    AND EXISTS (SELECT 1 FROM org_members m
                                 WHERE m.org_id = NEW.target_id
                                   AND m.user_id = (SELECT principal_id FROM actors WHERE id = NEW.granted_by_actor)))))
       AND NOT (NEW.target_kind = 'user'
                AND (SELECT principal_kind FROM actors WHERE id = NEW.granted_by_actor) = 'user'
                AND EXISTS (SELECT 1 FROM org_members mine
                              JOIN org_members theirs ON theirs.org_id = mine.org_id
                             WHERE mine.user_id = (SELECT principal_id FROM actors WHERE id = NEW.granted_by_actor)
                               AND theirs.user_id = NEW.target_id));
END;

-- A grant is immutable except for its revocation.
--
-- The issue policy above only fires on INSERT, so without this an UPDATE walks
-- straight around every rule in it: retarget a live grant at an unrelated
-- principal, widen read to write, reattribute it to an AI, or promote source to
-- 'override' and mint break-glass without passing the admin check. All four
-- were reproduced against a real database. Pinning which columns an UPDATE may
-- touch closes the whole class at once, and it is a smaller rule than re-running
-- the issue policy on every narrow.
CREATE TRIGGER grants_immutability
    BEFORE UPDATE ON grants
    WHEN NEW.id IS NOT OLD.id
      OR NEW.subject_kind IS NOT OLD.subject_kind
      OR NEW.subject_id IS NOT OLD.subject_id
      OR NEW.subject_name IS NOT OLD.subject_name
      OR NEW.target_kind IS NOT OLD.target_kind
      OR NEW.target_id IS NOT OLD.target_id
      OR NEW.target_install_id IS NOT OLD.target_install_id
      OR NEW.access IS NOT OLD.access
      OR NEW.source IS NOT OLD.source
      OR NEW.inherited_from IS NOT OLD.inherited_from
      OR NEW.granted_by_actor IS NOT OLD.granted_by_actor
      OR NEW.granted_by_principal_kind IS NOT OLD.granted_by_principal_kind
      OR NEW.granted_by_principal_id IS NOT OLD.granted_by_principal_id
      OR NEW.created_at IS NOT OLD.created_at
      OR NEW.expires_at IS NOT OLD.expires_at
BEGIN
    SELECT RAISE(ABORT, 'a grant is immutable except for revoked_at and revoked_by; delete it and write a new one');
END;

-- D18.2: every access that succeeded ONLY because of an override is audited.
-- The predicate returns 'override' exactly in that case, which is what makes
-- "only because" mechanically decidable rather than a judgement call. The
-- audit table is NOT in this file: it lives in the audit file
-- (migrations-audit/), because its rows must survive any caller's
-- transaction and on one file per writer that means its own file (D38 §3).

-- ---------------------------------------------------------------------------
-- Events: append-only, the transport of record (D4.5).
--
-- Not partitioned: one file is one log. The cursor is STILL the pair
-- (created_at, id), because a replica reading its own copy behind the primary
-- (D38 phase 3) sees rows late exactly as a late-committing transaction did,
-- and the tailer's overlap window is what catches that. See
-- docs/events-tailing.md.
-- ---------------------------------------------------------------------------

CREATE TABLE events (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at     INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),

    -- Two constraints rather than one, and the second is not redundant.
    --
    -- The format clause is the ordinary rule: a kind is a dotted identifier the
    -- platform composes, `<app>.<collection>.<verb>`. The control-character
    -- clause states the HAZARD directly, because a kind is written into the
    -- `event:` field of an SSE frame and a newline there renders one event as
    -- two ... including a forged `id:` on a frame the server had decided must
    -- not carry one. That forged cursor lands in the year 5138 and a client
    -- resuming from it receives nothing, forever.
    --
    -- Keeping them apart means widening the format later (uppercase, a longer
    -- name) cannot silently reopen frame injection.
    kind           TEXT NOT NULL
                     CONSTRAINT events_kind_is_an_identifier
                         CHECK (length(kind) BETWEEN 1 AND 128
                                AND kind GLOB '[a-z0-9]*'
                                AND NOT (kind GLOB '*[^a-z0-9._-]*'))
                     CONSTRAINT events_kind_has_no_frame_separator
                         CHECK (NOT (kind GLOB '*[^ -~]*')),

    -- What the event is about, in the shape the predicate takes, so replay
    -- filters with the same rule as a live read. 'collection' is deliberately
    -- absent: no writer produces one, and the feed's predicate cannot decide a
    -- collection without an acting install (D33), so the one guard is here at
    -- the INSERT rather than at every read of the feed.
    subject_kind   TEXT
        CONSTRAINT events_subject_kind_check
        CHECK (subject_kind IN ('install', 'tool', 'route', 'entity', 'conversation')),
    subject_id     TEXT,
    subject_name   TEXT,

    owner_kind     TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id       TEXT NOT NULL,
    author_actor   TEXT NOT NULL,
    principal_kind TEXT NOT NULL CHECK (principal_kind IN ('user', 'org')),
    principal_id   TEXT NOT NULL,

    body           TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(body)),
    trust          TEXT NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),
    -- Which operation FIRST weakened the invocation that produced this row.
    -- Diagnostic only: nothing branches on it, and nothing should. Without it,
    -- an untrusted row says it is untrusted and the answer to "why did this
    -- lose egress" lives in a log somebody has to still have.
    tainted_by     TEXT,
    cause_depth    INTEGER NOT NULL DEFAULT 0 CHECK (cause_depth >= 0),
    run_id         TEXT,

    -- D4.12: cross-hive bridging is far future, but this is the piece that is
    -- painful to retrofit and it costs nothing today.
    origin         TEXT NOT NULL DEFAULT 'local',
    origin_id      TEXT
);

CREATE INDEX events_cursor_idx ON events (created_at, id);
CREATE INDEX events_owner_idx ON events (owner_kind, owner_id, created_at DESC);
CREATE INDEX events_subject_idx ON events (subject_kind, subject_id, created_at DESC);

-- created_at is a LOCAL ingest timestamp, not a claim about when something
-- happened elsewhere. The realistic causes of a future value are exactly the
-- ones D4.12 plans for: clock skew, and a bridged event carrying another
-- hive's timestamp. A bridge puts the origin's timestamp in the body, where it
-- belongs. (One hour, in microseconds.)
CREATE TRIGGER events_no_future_timestamps
    BEFORE INSERT ON events
    WHEN NEW.created_at > (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)) + 3600000000
BEGIN
    SELECT RAISE(ABORT, 'events.created_at is more than an hour ahead of the server clock; it is a local ingest time, not the origin''s timestamp');
END;

-- D4.12 asks for (origin, origin_id) unique from the first migration. Global
-- uniqueness lives in a small side table, written by a trigger, and only for
-- events that actually came from somewhere else. Locally produced events leave
-- origin_id NULL and cost nothing.
CREATE TABLE event_origins (
    origin           TEXT NOT NULL,
    origin_id        TEXT NOT NULL,
    event_id         INTEGER NOT NULL,
    event_created_at INTEGER NOT NULL,
    PRIMARY KEY (origin, origin_id)
);

CREATE TRIGGER events_origin_dedupe
    AFTER INSERT ON events
    WHEN NEW.origin_id IS NOT NULL
BEGIN
    INSERT INTO event_origins (origin, origin_id, event_id, event_created_at)
    VALUES (NEW.origin, NEW.origin_id, NEW.id, NEW.created_at);
END;

-- Append-only means append-only.
CREATE TRIGGER events_append_only_update
    BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'events is append-only');
END;

CREATE TRIGGER events_append_only_delete
    BEFORE DELETE ON events
BEGIN
    SELECT RAISE(ABORT, 'events is append-only');
END;

-- ---------------------------------------------------------------------------
-- Mentions: host-owned, because a tag is a permission act (D13).
-- ---------------------------------------------------------------------------

CREATE TABLE mentions (
    id               TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    entity_id        TEXT NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    mentioned_actor  TEXT NOT NULL REFERENCES actors (id),

    -- D13.8: the grant goes to the tagged actor's PRINCIPAL. An AI does not own
    -- memory, so it cannot be the target of a share either.
    principal_kind   TEXT NOT NULL CHECK (principal_kind IN ('user', 'org')),
    principal_id     TEXT NOT NULL REFERENCES actors (id),

    author_actor     TEXT NOT NULL REFERENCES actors (id),
    owner_kind       TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id         TEXT NOT NULL REFERENCES actors (id),

    state            TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'delivered', 'acknowledged', 'actioned', 'dropped')),
    -- A denied cross-boundary tag is recorded with a reason, not dropped
    -- silently: the AI should be able to say "I wanted to loop in the other
    -- assistant and could not" (D13.14).
    drop_reason      TEXT,

    -- The share this tag wrote, in the same transaction as the entry and the
    -- mention (D13.2). SET NULL rather than CASCADE: revoking the share must
    -- not erase the record that the tag happened.
    grant_id         TEXT REFERENCES grants (id) ON DELETE SET NULL,

    run_id           TEXT,
    cause_depth      INTEGER NOT NULL DEFAULT 0 CHECK (cause_depth >= 0),
    trust            TEXT NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),

    delivered_at     INTEGER,
    acknowledged_at  INTEGER,
    created_at       INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),

    CONSTRAINT mentions_drop_reason_iff_dropped
        CHECK ((state = 'dropped') = (drop_reason IS NOT NULL)),
    UNIQUE (entity_id, mentioned_actor)
);

CREATE INDEX mentions_inbox_idx ON mentions (principal_kind, principal_id, state, created_at DESC);
CREATE INDEX mentions_actor_idx ON mentions (mentioned_actor, state);

-- ---------------------------------------------------------------------------
-- Workflows (D8). The step log is a checkpoint journal, never a replay tape.
-- ---------------------------------------------------------------------------

CREATE TABLE workflow_defs (
    id           TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    install_id   TEXT REFERENCES installs (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    spec         TEXT NOT NULL CHECK (json_valid(spec)),
    content_hash TEXT NOT NULL UNIQUE
        CHECK (length(content_hash) = 64 AND NOT (content_hash GLOB '*[^0-9a-f]*')),

    owner_kind   TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id     TEXT NOT NULL REFERENCES actors (id),
    author_actor TEXT NOT NULL REFERENCES actors (id),

    enabled      INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at   INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER))
);

CREATE INDEX workflow_defs_name_idx ON workflow_defs (name, created_at DESC);

-- Definitions are immutable and content-addressed. An AI editing a live
-- definition would otherwise change what an in-flight run resumes into. Editing
-- means writing a new row with a new hash and pointing triggers at it.
CREATE TRIGGER workflow_defs_immutable
    BEFORE UPDATE ON workflow_defs
    WHEN NEW.spec IS NOT OLD.spec OR NEW.content_hash IS NOT OLD.content_hash
BEGIN
    SELECT RAISE(ABORT, 'workflow_defs.spec and content_hash are immutable; write a new definition');
END;

CREATE TABLE workflow_triggers (
    id         TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    def_id     TEXT NOT NULL REFERENCES workflow_defs (id) ON DELETE CASCADE,
    kind       TEXT NOT NULL CHECK (kind IN ('event', 'cron', 'manual', 'webhook')),
    match      TEXT CHECK (match IS NULL OR json_valid(match)),
    cron_expr  TEXT,

    owner_kind TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id   TEXT NOT NULL REFERENCES actors (id),
    enabled    INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),

    CONSTRAINT workflow_triggers_cron_expr CHECK ((kind = 'cron') = (cron_expr IS NOT NULL))
);

CREATE INDEX workflow_triggers_event_idx ON workflow_triggers (kind) WHERE enabled = 1;

CREATE TABLE workflow_runs (
    id              TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    def_id          TEXT NOT NULL REFERENCES workflow_defs (id),
    -- Pinned at start. Resume reads recorded step results and never re-walks
    -- the definition, so a run is immune to a definition edit mid-flight.
    definition_hash TEXT NOT NULL,
    trigger_id      TEXT REFERENCES workflow_triggers (id) ON DELETE SET NULL,

    actor_id        TEXT NOT NULL REFERENCES actors (id),
    owner_kind      TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id        TEXT NOT NULL REFERENCES actors (id),

    input           TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(input)),
    state           TEXT NOT NULL DEFAULT 'running'
        CHECK (state IN ('running', 'waiting', 'succeeded', 'failed', 'cancelled')),

    -- Idempotent cron enqueue (D4.3) keys on this: the trigger id plus the
    -- fire time, INSERT ... ON CONFLICT DO NOTHING, RETURNING decides whether
    -- to notify at all.
    idem_key        TEXT UNIQUE,

    -- D17.12 / D17.3: both ride the run, and everything the run produces
    -- inherits them. An untrusted causal chain costs the run its egress.
    cause_depth     INTEGER NOT NULL DEFAULT 0 CHECK (cause_depth >= 0),
    trust           TEXT NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),
    egress_allowed  INTEGER NOT NULL DEFAULT 0 CHECK (egress_allowed IN (0, 1)),

    steps_used      INTEGER NOT NULL DEFAULT 0,
    max_steps       INTEGER NOT NULL DEFAULT 100 CHECK (max_steps > 0),
    deadline_at     INTEGER,
    started_at      INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    ended_at        INTEGER,
    error           TEXT,

    -- The trifecta rule, enforced at spawn rather than trusted to a prompt
    -- (D17.3): if anything in the causal chain is untrusted, the run has no
    -- egress unless a human granted that combination explicitly.
    CONSTRAINT workflow_runs_untrusted_has_no_egress
        CHECK (trust = 'trusted' OR egress_allowed = 0)
);

CREATE INDEX workflow_runs_state_idx ON workflow_runs (state, started_at) WHERE state IN ('running', 'waiting');
CREATE INDEX workflow_runs_owner_idx ON workflow_runs (owner_kind, owner_id, started_at DESC);

CREATE TABLE workflow_steps (
    id                   TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    run_id               TEXT NOT NULL REFERENCES workflow_runs (id) ON DELETE CASCADE,
    parent_step_id       TEXT REFERENCES workflow_steps (id) ON DELETE CASCADE,
    seq                  INTEGER NOT NULL,
    name                 TEXT NOT NULL,
    type                 TEXT NOT NULL CHECK (type IN (
        'wasm_call', 'http', 'agent_run', 'emit', 'workflow_call', 'sleep', 'wait_for_event')),

    -- Declared, not assumed (D8.4). agent_run spends money, so its default is
    -- at_most_once everywhere (D17.8) and the CHECK stops a definition author
    -- from talking us out of it.
    retry_policy         TEXT NOT NULL CHECK (retry_policy IN ('at_least_once', 'at_most_once')),

    input                TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(input)),
    output               TEXT CHECK (output IS NULL OR json_valid(output)),
    output_trust         TEXT NOT NULL DEFAULT 'trusted' CHECK (output_trust IN ('trusted', 'untrusted')),

    state                TEXT NOT NULL DEFAULT 'pending' CHECK (state IN (
        'pending', 'leased', 'waiting_timer', 'waiting_event',
        'succeeded', 'failed', 'skipped', 'indeterminate')),

    attempt              INTEGER NOT NULL DEFAULT 0,
    max_attempts         INTEGER NOT NULL DEFAULT 1 CHECK (max_attempts > 0),
    next_attempt_at      INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),

    lease_owner          TEXT,
    lease_expires_at     INTEGER,
    heartbeat_at         INTEGER,

    wake_at              INTEGER,
    wait_match           TEXT CHECK (wait_match IS NULL OR json_valid(wait_match)),

    idem_key             TEXT,
    pending_children     INTEGER NOT NULL DEFAULT 0 CHECK (pending_children >= 0),
    continue_on_error    INTEGER NOT NULL DEFAULT 0 CHECK (continue_on_error IN (0, 1)),

    error                TEXT,
    -- An at-most-once step reclaimed from a dead lease cannot know whether its
    -- effect happened. It lands here rather than re-firing (invariant 10).
    indeterminate_reason TEXT,

    created_at           INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    updated_at           INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),

    CONSTRAINT workflow_steps_agent_run_is_at_most_once
        CHECK (type <> 'agent_run' OR retry_policy = 'at_most_once'),
    CONSTRAINT workflow_steps_at_most_once_attempts
        CHECK (retry_policy <> 'at_most_once' OR max_attempts = 1),
    CONSTRAINT workflow_steps_indeterminate_has_a_reason
        CHECK ((state = 'indeterminate') = (indeterminate_reason IS NOT NULL)),
    CONSTRAINT workflow_steps_timer_has_a_wake
        CHECK (state <> 'waiting_timer' OR wake_at IS NOT NULL),
    CONSTRAINT workflow_steps_lease_is_bounded
        CHECK (state <> 'leased' OR (lease_owner IS NOT NULL AND lease_expires_at IS NOT NULL))
);

CREATE UNIQUE INDEX workflow_steps_idem_uq ON workflow_steps (run_id, idem_key)
    WHERE idem_key IS NOT NULL;

-- The claim path: UPDATE ... RETURNING over this index, under BEGIN IMMEDIATE.
CREATE INDEX workflow_steps_claim_idx ON workflow_steps (next_attempt_at, created_at)
    WHERE state = 'pending';
CREATE INDEX workflow_steps_timer_idx ON workflow_steps (wake_at) WHERE state = 'waiting_timer';
CREATE INDEX workflow_steps_lease_idx ON workflow_steps (lease_expires_at) WHERE state = 'leased';
CREATE INDEX workflow_steps_wait_idx ON workflow_steps (run_id) WHERE state = 'waiting_event';
CREATE INDEX workflow_steps_run_idx ON workflow_steps (run_id, seq);

-- ---------------------------------------------------------------------------
-- Harness runs get their own tables.
--
-- The harness runs AI agents (claude / codex / opencode) in rootless Podman
-- containers. These are NOT workflow_runs. A harness run can be started by a
-- workflow step, and can equally be started by a person opening a chat -- so
-- it cannot hang off workflow_steps without making the interactive case a
-- workflow that is not one. The link to a step is a nullable reference rather
-- than a parent.
-- ---------------------------------------------------------------------------

CREATE TABLE agent_runs (
    id              TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),

    -- Invariant 2, and the reason this table exists in the shape it does.
    -- author_actor is WHO AUTHORED the run and may be an AI; owner_* is whose
    -- authority is being spent and is never an AI. "Nate ran this" and "an AI
    -- acting for Nate ran this" must stay distinguishable on every row.
    --
    -- Both are pinned by the writer from the credential. They are NOT on
    -- RunRecord and must never be added to it: a caller that supplies them is
    -- supplying the fact the row is deciding about (invariant 11), and there
    -- are then as many enforcement points as call sites.
    author_actor    TEXT NOT NULL REFERENCES actors (id),
    owner_kind      TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id        TEXT NOT NULL REFERENCES actors (id),

    -- The agent's own identity, when the run acts as one. Distinct from
    -- author_actor: an AI may launch a run that acts as a different AI.
    agent_actor     TEXT REFERENCES actors (id),

    -- Nullable on purpose. A run started from a chat has no step.
    workflow_step_id TEXT REFERENCES workflow_steps (id) ON DELETE SET NULL,

    -- The harness's OWN run identifier, and it is not a uuid: it names a podman
    -- container and a network, so it is constrained to what podman will accept
    -- as a name. RunSpec.validate enforces the same pattern before a container
    -- is created; this repeats it because a writer is not the only way a row
    -- arrives, and the two fail for different callers.
    --
    -- UNIQUE fleet-wide, which is STRICTER than strictly necessary: the derived
    -- names are unique per podman daemon and daemons are per host, so two hosts
    -- could reuse one without colliding in reality. Fleet-wide is kept anyway
    -- because it costs nothing and makes a run id mean one run everywhere,
    -- which is what anyone reading a log will assume it means.
    run_key         TEXT NOT NULL UNIQUE
        CHECK (length(run_key) BETWEEN 1 AND 63
               AND run_key GLOB '[a-zA-Z0-9]*'
               AND NOT (run_key GLOB '*[^a-zA-Z0-9_.-]*')),

    -- What actually ran, from RunRecord.
    runtime         TEXT NOT NULL,
    image_digest    TEXT NOT NULL,
    cli_version     TEXT NOT NULL DEFAULT '',
    model           TEXT NOT NULL DEFAULT '',

    -- Scraped from the CLI's own output when it announces one, so a follow-up
    -- run can resume the conversation. Minted by the agent CLI, so it is NOT a
    -- capability: anything keyed on it alone would let a session id act as
    -- permission, which is invariant 14's shape.
    session_id      TEXT NOT NULL DEFAULT '',

    -- These are harness NetworkMode's values VERBATIM: none, daemon, proxied.
    -- The names here are not descriptions; they are the constants, and
    -- drifting from them is silent until the one mode nobody tested is used.
    network         TEXT NOT NULL CHECK (network IN ('none', 'daemon', 'proxied')),
    memory_bytes    INTEGER NOT NULL DEFAULT 0 CHECK (memory_bytes >= 0),
    cpus            REAL NOT NULL DEFAULT 0 CHECK (cpus >= 0),
    pids_limit      INTEGER NOT NULL DEFAULT 0 CHECK (pids_limit >= 0),

    -- Invariant 12. Monotonic, and recorded from the invocation rather than
    -- claimed by the run.
    trust           TEXT NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),

    -- harness TerminalState's values VERBATIM, plus 'running'.
    --
    -- INVARIANT 10 lives here. A harness run spends money, so a lease reclaim
    -- must land 'indeterminate' rather than re-firing. 'indeterminate' is
    -- therefore a first-class terminal state, not an error: it means the run
    -- may or may not have completed and NOTHING may retry it automatically.
    state           TEXT NOT NULL DEFAULT 'running'
        CHECK (state IN ('running', 'succeeded', 'failed', 'deadline_exceeded',
                         'cancelled', 'indeterminate')),

    -- A run caused by another run, so a loop guard has something to count
    -- (D17.12). An agent that spawns an agent that spawns an agent is the
    -- shape that spends money without a human ever seeing it.
    cause_depth     INTEGER NOT NULL DEFAULT 0 CHECK (cause_depth >= 0),

    exit_code       INTEGER,
    event_count     INTEGER NOT NULL DEFAULT 0 CHECK (event_count >= 0),
    stderr_tail     TEXT NOT NULL DEFAULT '',

    -- Reclaim bookkeeping. deadline_at is when a lease is considered lost.
    started_at      INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    heartbeat_at    INTEGER,
    deadline_at     INTEGER,
    ended_at        INTEGER,

    -- The conversation and the turn this run answers, when it answers one.
    conversation_id TEXT REFERENCES conversations (id) ON DELETE SET NULL,
    turn_id         TEXT REFERENCES chat_turns (id) ON DELETE SET NULL,

    -- D17.3 with teeth, on the column that actually grants egress.
    --
    -- An untrusted run must not reach the internet. Untrusted means content
    -- the platform pulled in from outside; a message typed by an authenticated
    -- principal spending their own authority is first-party input and stays
    -- trusted. It fires before a container exists: create_run runs ahead of
    -- the launcher, so this is enforcement rather than decoration.
    CONSTRAINT agent_runs_untrusted_has_no_egress
        CHECK (trust = 'trusted' OR network <> 'proxied'),

    CONSTRAINT agent_runs_terminal_has_end
        CHECK (state = 'running' OR ended_at IS NOT NULL),

    -- A run that ended cannot have ended before it started. Cheap, and it
    -- catches a clock or a writer passing the wrong timestamp.
    CONSTRAINT agent_runs_ends_after_start
        CHECK (ended_at IS NULL OR ended_at >= started_at)
);

-- ONE workflow step gets ONE harness run, for the life of the database.
--
-- This is invariant 10 made structural. Without it, a step whose lease was
-- reclaimed could produce a second run row and spend money twice. It omits the
-- attempt number deliberately: including it is exactly how a reclaimed lease
-- gets a second run.
CREATE UNIQUE INDEX agent_runs_step_uq
    ON agent_runs (workflow_step_id)
    WHERE workflow_step_id IS NOT NULL;

-- One run per turn, for the same reason: a reclaimed turn must not produce a
-- second paid run (invariant 10).
CREATE UNIQUE INDEX agent_runs_turn_uq
    ON agent_runs (turn_id)
    WHERE turn_id IS NOT NULL;

-- Idempotency for runs started OUTSIDE a workflow, keyed WITH the owner.
--
-- This departs from workflow_runs.idem_key being bare UNIQUE, and the
-- difference is the point: the workflow engine composes its own keys and can
-- guarantee they are unique fleet-wide, but a chat client cannot. A bare unique
-- key would let one owner's key collide with another's and silently return
-- someone else's run -- a key that omits a dimension its correctness depends
-- on (invariant 14).
CREATE TABLE agent_run_keys (
    owner_kind TEXT NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id   TEXT NOT NULL REFERENCES actors (id),
    idem_key   TEXT NOT NULL,
    run_id     TEXT NOT NULL REFERENCES agent_runs (id) ON DELETE CASCADE,
    PRIMARY KEY (owner_kind, owner_id, idem_key)
);

-- The reclaimer's index. It deliberately omits the owner, and must: the
-- reclaimer is host machinery rather than an actor spending anyone's authority,
-- and it has to see every stalled run or a crashed one is never reconciled.
-- Nothing reads runs through this index on behalf of a caller.
CREATE INDEX agent_runs_reclaim_idx
    ON agent_runs (deadline_at)
    WHERE state = 'running';

-- Listing an owner's runs, newest first.
CREATE INDEX agent_runs_owner_idx
    ON agent_runs (owner_kind, owner_id, started_at DESC);

-- Resuming a conversation. The owner is IN the key because session_id comes
-- from the agent CLI and is not a secret: keyed on session_id alone, knowing
-- one would be enough to find someone else's run.
CREATE INDEX agent_runs_session_idx
    ON agent_runs (owner_kind, owner_id, runtime, session_id)
    WHERE session_id <> '';

CREATE INDEX agent_runs_conversation_idx
    ON agent_runs (conversation_id, started_at)
    WHERE conversation_id IS NOT NULL;

-- One row per line the child process emitted.
--
-- append_event is on the critical path of a pipe drain -- a slow store slows
-- the agent and a blocking one hangs it -- so this table is deliberately narrow
-- and carries no owner, author or trust of its own. run_id is NOT NULL with a
-- foreign key, so every event has exactly one owner, author and trust value,
-- reachable in one join.
CREATE TABLE agent_run_events (
    run_id  TEXT NOT NULL REFERENCES agent_runs (id) ON DELETE CASCADE,

    -- Starts at 1 and is unique within a run. The primary key is (run_id, seq)
    -- rather than a surrogate id: it is what the drain path already has, it
    -- makes an accidental double-append a constraint violation rather than a
    -- duplicate line, and it gives ordered reads for free.
    seq     INTEGER NOT NULL CHECK (seq >= 1),

    at      INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),
    stream  TEXT NOT NULL CHECK (stream IN ('stdout', 'stderr')),

    -- The stream-json "type" field, empty when the line was not JSON.
    type    TEXT NOT NULL DEFAULT '',
    -- The parsed line, null when the line was not JSON.
    body    TEXT CHECK (body IS NULL OR json_valid(body)),
    -- The raw line, always. A line that failed to parse is still evidence.
    text    TEXT NOT NULL DEFAULT '',

    PRIMARY KEY (run_id, seq)
);

-- ---------------------------------------------------------------------------
-- Chat: messages and the turn ledger.
--
-- A chat turn is ONE HARNESS RUN PER MESSAGE, resumed through session_id. Not a
-- long-lived container: a conversation that is idle costs nothing, a crash
-- loses one turn rather than a session, and the cold start is the accepted
-- price.
-- ---------------------------------------------------------------------------

CREATE TABLE chat_messages (
    conversation_id TEXT NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,

    -- Dense per conversation, assigned by the writer inside the transaction
    -- that appends. (conversation_id, seq) is the primary key rather than a
    -- surrogate: it is what a client pages on, it makes a double-post a
    -- constraint violation instead of a duplicate message, and it gives ordered
    -- reads without a sort.
    seq             INTEGER NOT NULL CHECK (seq >= 1),

    -- 'user' is a person, 'agent' is the AI, 'system' is the platform.
    role            TEXT NOT NULL CHECK (role IN ('user', 'agent', 'system')),

    -- Who actually wrote it. An agent message has the agent's actor here, which
    -- is what makes "an AI acting for Nate said this" recoverable later.
    author_actor    TEXT NOT NULL REFERENCES actors (id),

    body            TEXT NOT NULL,

    -- Invariant 9. A message from a browser is first-party input and trusted;
    -- an agent message that quoted fetched content is not, and must stay marked
    -- so downstream turns inherit it.
    trust           TEXT NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),

    -- The run that produced an agent message. Null for a user message.
    run_id          TEXT REFERENCES agent_runs (id) ON DELETE SET NULL,

    created_at      INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),

    PRIMARY KEY (conversation_id, seq)
);

-- A turn is a durable claim: "this message needs an agent run". It exists so a
-- crash between accepting a message and starting a run does not lose the turn,
-- and so exactly one worker acts on it.
CREATE TABLE chat_turns (
    id              TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))),
    conversation_id TEXT NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,

    -- The user message this turn answers.
    request_seq     INTEGER NOT NULL,

    state           TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'claimed', 'done', 'failed')),

    -- A claim under BEGIN IMMEDIATE plus a lease and a heartbeat, as the
    -- repo's convention requires. A claim that stops heartbeating is
    -- reclaimable.
    claimed_by       TEXT,
    claimed_at       INTEGER,
    lease_expires_at INTEGER,

    run_id          TEXT REFERENCES agent_runs (id) ON DELETE SET NULL,
    created_at      INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)),

    CONSTRAINT chat_turns_claim_is_complete
        CHECK ((state = 'pending') = (claimed_by IS NULL)),

    -- ONE turn per user message. This is the at-most-once guard for chat, the
    -- analogue of agent_runs_step_uq for workflow steps: a client retrying a
    -- post, or two workers racing, cannot produce a second paid run for one
    -- message.
    CONSTRAINT chat_turns_one_per_message UNIQUE (conversation_id, request_seq)
);

-- The claimer's index. Omits the owner deliberately and must: a turn worker is
-- host machinery rather than an actor spending authority, and it has to see
-- every pending turn or a conversation stalls forever. Nothing reads turns
-- through this index on behalf of a caller.
CREATE INDEX chat_turns_pending_idx
    ON chat_turns (created_at)
    WHERE state = 'pending';

-- Reclaiming a claim whose lease lapsed.
CREATE INDEX chat_turns_lease_idx
    ON chat_turns (lease_expires_at)
    WHERE state = 'claimed';

-- Session continuity, keyed on the CONVERSATION, not on (owner, runtime).
--
-- agent_runs_session_idx is (owner_kind, owner_id, runtime, session_id), which
-- is right for finding a run by session but wrong for answering "which session
-- should this conversation resume": keyed that way, a second conversation with
-- the same AI would resume the first one's session and the two threads would
-- merge. A key that omits a dimension its correctness depends on is a bypass
-- (invariant 14), and the omitted dimension here is the conversation.
CREATE TABLE chat_sessions (
    conversation_id TEXT PRIMARY KEY REFERENCES conversations (id) ON DELETE CASCADE,
    runtime         TEXT NOT NULL,

    -- Scraped from the CLI's own output. Empty until the first run reports one,
    -- which is why the first turn starts fresh and every later turn resumes.
    session_id      TEXT NOT NULL DEFAULT '',
    updated_at      INTEGER NOT NULL DEFAULT (CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER))
);
