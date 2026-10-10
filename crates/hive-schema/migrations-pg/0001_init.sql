-- Migration one, for the Postgres engine (D43). The same schema as
-- ../migrations/0001_init.sql, which is the SQLite text; one schema in two
-- texts, held together by the suite that runs against both (D43 §5). Every
-- table here exists because getting it wrong becomes unrecoverable once a row
-- lands. Decision references are D<n> in docs/design/ and the epic's log.
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
-- The dialect, once (D38 §3 read right to left):
--
--   * uuid is uuid; gen_random_uuid() is core since Postgres 13, so a raw insert
--     gets a real id without an extension.
--   * Every timestamp is timestamptz, microsecond resolution, which is the
--     resolution the host clock binds (hive_db::now).
--   * JSON is jsonb.
--   * The three alphabets this schema constrains (hex, slug, event kind) are
--     regular expressions here, where SQLite spelled them with GLOB.
--   * Triggers are plpgsql functions. Their messages are the SAME static text
--     as the SQLite triggers', on purpose: a test that asserts which rule
--     refused a write asserts one string on both engines. Where the first
--     Postgres port interpolated an id, this one does not.
--   * Timing follows the SQLite file: BEFORE for a policy on the write itself,
--     AFTER where the rule has to see the new row in the table. Nothing is
--     deferred; the write order the SQLite port adopted (reserve, reference,
--     flip to live) is what every writer already does.
--   * The acting-actor rule is one function, acting_kind(), called by the two
--     triggers that need it. hive-store's predicate text carries its own copy
--     (ACTING_KIND); two copies, on the record (D38 §2, D43 §2).

-- ---------------------------------------------------------------------------
-- Actors: users, AI identities and orgs in one addressing model (D1.2).
-- ---------------------------------------------------------------------------

CREATE TABLE actors (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind           text NOT NULL CHECK (kind IN ('human', 'ai', 'org')),
    handle         text NOT NULL UNIQUE,
    display_name   text NOT NULL DEFAULT '',

    -- D13.9: an AI identity is a per-principal instance of a persona, so it
    -- resolves to exactly one principal. A free-floating persona has nothing a
    -- grant can be written against, which makes every tag ambiguous.
    persona        text,

    -- The principal this actor acts for. Humans and orgs are their own
    -- principal; an AI's principal is the human or org that owns it. An AI
    -- never appears as a principal (D13.4), which is enforced below.
    principal_kind text NOT NULL CHECK (principal_kind IN ('user', 'org')),
    principal_id   uuid NOT NULL REFERENCES actors (id),

    -- D19.1/D19.2. Exactly one actor may have no creator: the bootstrap root,
    -- guarded by the partial unique index below. A system whose first
    -- authorization can be requested over the network does not have a root.
    created_by_actor uuid REFERENCES actors (id),

    meta           jsonb NOT NULL DEFAULT '{}',
    created_at     timestamptz NOT NULL DEFAULT now(),
    disabled_at    timestamptz,

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
CREATE FUNCTION actors_principal_check() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    p_kind text;
BEGIN
    SELECT kind INTO p_kind FROM actors WHERE id = NEW.principal_id;
    IF p_kind IS NULL THEN
        RAISE EXCEPTION 'actor has no principal row';
    END IF;
    IF p_kind = 'ai' THEN
        RAISE EXCEPTION 'an AI actor cannot be a principal';
    END IF;
    IF (p_kind = 'org') <> (NEW.principal_kind = 'org') THEN
        RAISE EXCEPTION 'principal_kind disagrees with the principal actor''s kind';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER actors_principal_check_insert
    AFTER INSERT ON actors
    FOR EACH ROW EXECUTE FUNCTION actors_principal_check();

CREATE TRIGGER actors_principal_check_update
    AFTER UPDATE OF principal_kind, principal_id ON actors
    FOR EACH ROW EXECUTE FUNCTION actors_principal_check();

-- D19.2: an org admin creates actors within their org; a person creates AI
-- persona instances owned by themselves. An AI never creates actors. Enforced
-- as a trigger rather than in a service, because "an AI cannot climb" has to
-- hold for any writer that reaches this database.
CREATE FUNCTION actors_creation_check() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    c_kind text;
BEGIN
    SELECT kind INTO c_kind FROM actors WHERE id = NEW.created_by_actor;
    IF c_kind IS NULL THEN
        RAISE EXCEPTION 'actor: creator does not exist';
    END IF;
    IF c_kind = 'ai' THEN
        RAISE EXCEPTION 'actor: an AI actor may not create actors (D19.2)';
    END IF;
    -- A human or org actor is its own principal, so creating one confers no
    -- authority on the creator. Authority attaches when the new actor is seated
    -- in an org, and org_members carries that check. An AI persona instance IS
    -- authority ... it can act for its principal ... so the creator must be
    -- that person, or an admin of that org.
    IF NEW.kind = 'ai'
       AND NOT (NEW.principal_kind = 'user' AND NEW.principal_id = NEW.created_by_actor)
       AND NOT (NEW.principal_kind = 'org' AND EXISTS (
               SELECT 1 FROM org_members m
                WHERE m.org_id = NEW.principal_id
                  AND m.user_id = NEW.created_by_actor
                  AND m.role = 'admin')) THEN
        RAISE EXCEPTION 'actor: creator may not create an AI acting for that principal (D19.2)';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER actors_creation_check
    AFTER INSERT ON actors
    FOR EACH ROW WHEN (NEW.created_by_actor IS NOT NULL)
    EXECUTE FUNCTION actors_creation_check();

-- Membership is where authority actually attaches, so this is where D19.2's
-- "an org admin, within their org" is enforced.
CREATE TABLE org_members (
    org_id         uuid NOT NULL REFERENCES actors (id) ON DELETE CASCADE,
    user_id        uuid NOT NULL REFERENCES actors (id) ON DELETE CASCADE,
    role           text NOT NULL CHECK (role IN ('member', 'admin')),
    added_by_actor uuid NOT NULL REFERENCES actors (id),
    created_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (org_id, user_id)
);

CREATE INDEX org_members_user_idx ON org_members (user_id);

-- AFTER, so the first seat can see that no OTHER member exists yet.
CREATE FUNCTION org_members_check() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    org_creator uuid;
BEGIN
    IF (SELECT kind FROM actors WHERE id = NEW.org_id) IS DISTINCT FROM 'org' THEN
        RAISE EXCEPTION 'org_members.org_id is not an org';
    END IF;
    IF (SELECT kind FROM actors WHERE id = NEW.user_id) IS DISTINCT FROM 'human' THEN
        RAISE EXCEPTION 'org_members.user_id is not a human';
    END IF;
    IF (SELECT kind FROM actors WHERE id = NEW.added_by_actor) = 'ai' THEN
        RAISE EXCEPTION 'an AI actor may not seat members in an org (D19.2)';
    END IF;
    -- An admin seats members. The first seat is the exception: the org's own
    -- creator becomes its first admin, and there is no membership row to check
    -- against yet.
    IF EXISTS (SELECT 1 FROM org_members m
                WHERE m.org_id = NEW.org_id AND m.user_id = NEW.added_by_actor AND m.role = 'admin') THEN
        RETURN NEW;
    END IF;
    SELECT created_by_actor INTO org_creator FROM actors WHERE id = NEW.org_id;
    IF org_creator IS NOT NULL AND org_creator = NEW.added_by_actor
       AND NOT EXISTS (SELECT 1 FROM org_members m WHERE m.org_id = NEW.org_id AND m.user_id <> NEW.user_id) THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'actor is not an admin of that org (D19.2)';
END;
$$;

CREATE TRIGGER org_members_check_insert
    AFTER INSERT ON org_members
    FOR EACH ROW EXECUTE FUNCTION org_members_check();

CREATE TRIGGER org_members_check_update
    AFTER UPDATE ON org_members
    FOR EACH ROW EXECUTE FUNCTION org_members_check();

-- Credential coherence (D17.4). The pair (actor, principal) has to hold up or
-- it proves nothing. Returns the acting actor's kind, or NULL when the pair
-- does not. The SQLite file inlines this expression in two triggers; a
-- function is its natural form here. hive-store's predicate carries the same
-- rule as text (ACTING_KIND); two copies, on the record.
CREATE FUNCTION acting_kind(p_actor_id uuid, p_principal_kind text, p_principal_id uuid)
RETURNS text
LANGUAGE plpgsql STABLE AS $$
DECLARE
    a_kind   text;
    a_p_kind text;
    a_p_id   uuid;
    a_off    timestamptz;
BEGIN
    IF p_actor_id IS NULL OR p_principal_kind IS NULL OR p_principal_id IS NULL THEN
        RETURN NULL;
    END IF;
    SELECT kind, principal_kind, principal_id, disabled_at
      INTO a_kind, a_p_kind, a_p_id, a_off
      FROM actors WHERE id = p_actor_id;
    IF a_kind IS NULL OR a_off IS NOT NULL THEN
        RETURN NULL;
    END IF;
    IF a_kind = 'ai' THEN
        IF a_p_kind IS DISTINCT FROM p_principal_kind OR a_p_id IS DISTINCT FROM p_principal_id THEN
            RETURN NULL;
        END IF;
        RETURN 'ai';
    END IF;
    IF a_kind = 'human' THEN
        IF (p_principal_kind = 'user' AND p_principal_id = p_actor_id)
           OR (p_principal_kind = 'org' AND EXISTS (
                   SELECT 1 FROM org_members m
                    WHERE m.org_id = p_principal_id AND m.user_id = p_actor_id)) THEN
            RETURN 'human';
        END IF;
        RETURN NULL;
    END IF;
    -- An org is an owner and a grant target. It is not something that acts.
    RETURN NULL;
END;
$$;

-- ---------------------------------------------------------------------------
-- Credentials (D17.4, D19.3). The credential is where author_actor and owner
-- principal enter every request as a PAIR. Without the pair on the request, the
-- pair can never be populated honestly on a row.
-- ---------------------------------------------------------------------------

CREATE TABLE credentials (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    actor_id       uuid NOT NULL REFERENCES actors (id) ON DELETE CASCADE,
    principal_kind text NOT NULL CHECK (principal_kind IN ('user', 'org')),
    principal_id   uuid NOT NULL REFERENCES actors (id),

    -- The token is never stored. Only its hash comes back here.
    token_sha256   text NOT NULL UNIQUE CHECK (token_sha256 ~ '^[0-9a-f]{64}$'),
    label          text NOT NULL DEFAULT '',

    issued_by_actor          uuid NOT NULL REFERENCES actors (id),
    issued_by_principal_kind text NOT NULL CHECK (issued_by_principal_kind IN ('user', 'org')),
    issued_by_principal_id   uuid NOT NULL REFERENCES actors (id),

    created_at     timestamptz NOT NULL DEFAULT now(),
    expires_at     timestamptz,
    revoked_at     timestamptz,
    last_used_at   timestamptz
);

CREATE INDEX credentials_actor_idx ON credentials (actor_id) WHERE revoked_at IS NULL;

-- D19.3: a principal issues for itself, or an org admin for actors in their
-- org. An AI never issues credentials, which is the other half of "an AI cannot
-- climb" (the first half being D18's no-override-for-AI rule).
CREATE FUNCTION credentials_issue_check() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    subject_kind   text;
    subject_p_kind text;
    subject_p_id   uuid;
BEGIN
    IF (SELECT kind FROM actors WHERE id = NEW.issued_by_actor) = 'ai' THEN
        RAISE EXCEPTION 'credential: an AI actor may not issue credentials (D19.3)';
    END IF;
    -- The credential's pair has to be one the subject actor can actually hold.
    SELECT kind, principal_kind, principal_id INTO subject_kind, subject_p_kind, subject_p_id
      FROM actors WHERE id = NEW.actor_id;
    IF subject_kind = 'ai'
       AND NOT (subject_p_kind IS NOT DISTINCT FROM NEW.principal_kind
                AND subject_p_id IS NOT DISTINCT FROM NEW.principal_id) THEN
        RAISE EXCEPTION 'credential: an AI actor is pinned to one principal';
    END IF;
    IF subject_kind IS DISTINCT FROM 'ai'
       AND NOT (
            (NEW.principal_kind = 'user' AND NEW.principal_id = NEW.actor_id)
            OR (NEW.principal_kind = 'org' AND EXISTS (
                    SELECT 1 FROM org_members m
                     WHERE m.org_id = NEW.principal_id AND m.user_id = NEW.actor_id))) THEN
        RAISE EXCEPTION 'credential: actor cannot act for that principal';
    END IF;
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
    IF NOT (NEW.principal_kind = 'user' AND NEW.principal_id = NEW.issued_by_actor)
       AND NOT (NEW.principal_kind = 'org' AND EXISTS (
                SELECT 1 FROM org_members m
                 WHERE m.org_id = NEW.principal_id
                   AND m.user_id = NEW.issued_by_actor
                   AND m.role = 'admin')) THEN
        RAISE EXCEPTION 'credential: issuer may not issue for that principal (D19.3)';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER credentials_issue_check
    BEFORE INSERT ON credentials
    FOR EACH ROW EXECUTE FUNCTION credentials_issue_check();

-- ---------------------------------------------------------------------------
-- Blobs. Bytes and references are separate tables because ownership,
-- permission and trust are properties of a reference (D17.1, D6 key layout).
-- ---------------------------------------------------------------------------

CREATE TABLE blobs (
    sha256      text PRIMARY KEY CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    size        bigint NOT NULL CHECK (size >= 0),
    mime        text NOT NULL DEFAULT 'application/octet-stream',
    driver      text NOT NULL,
    driver_ref  text,

    -- pending is the reservation (D6.5): reserve the row, release the lock,
    -- move the bytes, flip to live. Every crash window fails toward reclaimable
    -- litter rather than a live row pointing at nothing.
    state       text NOT NULL CHECK (state IN ('pending', 'live', 'evicted', 'trashed')),

    class       text NOT NULL CHECK (class IN ('derived', 'build', 'capture', 'original')),
    source_hash text REFERENCES blobs (sha256),
    recipe      jsonb,

    created_at  timestamptz NOT NULL DEFAULT now(),
    live_at     timestamptz,
    evicted_at  timestamptz,
    trashed_at  timestamptz,

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
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    sha256       text NOT NULL REFERENCES blobs (sha256) ON DELETE RESTRICT,

    owner_kind   text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id     uuid NOT NULL REFERENCES actors (id),
    author_actor uuid NOT NULL REFERENCES actors (id),

    -- Every producer in the platform, not just host.storage.* (D17.5). A
    -- sweeper that does not know about modules deletes live modules.
    source_kind  text NOT NULL CHECK (source_kind IN (
        'upload', 'collection', 'module', 'guest_source', 'transcript',
        'spool', 'screenshot', 'step_output', 'harness_diff', 'workflow_input'
    )),
    source_id    text NOT NULL,

    -- D17.1. Trust rides the reference, never the bytes: global dedup makes an
    -- upload and a fetched page with identical bytes one blob row, and
    -- trusted-first would silently launder web content into trusted.
    trust        text NOT NULL CHECK (trust IN ('trusted', 'untrusted')),

    created_at   timestamptz NOT NULL DEFAULT now(),
    -- The mechanism to release a reference has to exist even when the policy is
    -- "keep forever", or the option cannot be exercised later (D6 retention).
    released_at  timestamptz,

    UNIQUE (sha256, owner_kind, owner_id, source_kind, source_id)
);

CREATE INDEX blob_refs_hash_idx ON blob_refs (sha256) WHERE released_at IS NULL;
CREATE INDEX blob_refs_owner_idx ON blob_refs (owner_kind, owner_id) WHERE released_at IS NULL;

-- Invariant: no blob exists without a ref, and whatever produced it writes one.
-- A pending reservation has no ref yet by design, so the rule binds at the flip
-- to live. Immediate, not deferred: the WRITE ORDER carries it on both engines
-- (reserve the row, write the reference, then flip to live), and a writer that
-- flips first is refused here.
CREATE FUNCTION blobs_live_ref_check() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.state = 'live' AND NOT EXISTS (
        SELECT 1 FROM blob_refs r WHERE r.sha256 = NEW.sha256 AND r.released_at IS NULL
    ) THEN
        RAISE EXCEPTION 'blob cannot go live with no reference';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER blobs_live_ref_check_insert
    AFTER INSERT ON blobs
    FOR EACH ROW WHEN (NEW.state = 'live')
    EXECUTE FUNCTION blobs_live_ref_check();

CREATE TRIGGER blobs_live_ref_check_update
    AFTER UPDATE OF state ON blobs
    FOR EACH ROW WHEN (NEW.state = 'live')
    EXECUTE FUNCTION blobs_live_ref_check();

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
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Bounded, because a slug is not only a name. It is the first segment of
    -- every event kind this app emits (`<slug>.<collection>.<verb>`), and
    -- events.kind is constrained; an unbounded slug would let a manifest emit
    -- a kind the events table refuses, so the app's writes would fail at the
    -- point of use rather than at registration. Same alphabet as the kind, so
    -- one cannot produce a value the other rejects.
    slug          text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    version       text NOT NULL DEFAULT '',
    kind          text NOT NULL CHECK (kind IN ('app', 'tool')),
    impl          text NOT NULL DEFAULT 'wasm' CHECK (impl IN ('wasm', 'host')),

    -- NULL for impl='host' builtins, which have no module bytes.
    module_sha256 text REFERENCES blobs (sha256),
    -- The guest source that produced the module. 'build' class blobs are
    -- rebuildable only if source AND toolchain are both recorded (D6).
    source_sha256 text REFERENCES blobs (sha256),
    toolchain     text NOT NULL DEFAULT '',

    manifest      jsonb NOT NULL,
    content_hash  text NOT NULL UNIQUE CHECK (content_hash ~ '^[0-9a-f]{64}$'),

    -- The surface this build exposes: its tools and routes after generated CRUD
    -- and overrides resolve. Recorded rather than recomputed, and the
    -- distinction is the whole point ... a recomputed hash says "this is what we
    -- would derive now", where a promotion reviewer needs "this is what a human
    -- approved". If the deriver ever changes, every historical hash silently
    -- changes meaning and the comparison starts measuring today's deriver
    -- against itself.
    surface_hash  text CHECK (surface_hash IS NULL OR surface_hash ~ '^[0-9a-f]{64}$'),

    -- WHICH deriver produced it. Without this, two hashes that differ are
    -- ambiguous between "the app changed" and "we changed", which is exactly
    -- the question the hash exists to answer.
    derive_version integer,

    -- What produced it. A build with no run is hand-written and first-party;
    -- a build with one came from the builder loop and says so.
    built_by_run_id uuid,

    -- The test outcome, attached to the build rather than living in a log
    -- somebody has to go find.
    test_state    text NOT NULL DEFAULT 'untested'
        CHECK (test_state IN ('untested', 'passed', 'failed')),
    test_summary  jsonb NOT NULL DEFAULT '{}',
    tested_at     timestamptz,

    -- D17.13: author_kind (user|org) cannot represent "Colette built this for
    -- Nate." Same author/owner split as every other content row.
    author_actor  uuid NOT NULL REFERENCES actors (id),
    owner_kind    text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id      uuid NOT NULL REFERENCES actors (id),

    visibility    text NOT NULL CHECK (visibility IN ('private', 'org', 'shared')),
    -- D10.9: trust tier sets default capabilities. Tools are cheap to create,
    -- which is exactly why 'local' starts with none.
    trust         text NOT NULL CHECK (trust IN ('builtin', 'local', 'imported')),

    -- 'registered' is where a build waits for a human, indefinitely and
    -- correctly. It is deliberately not called 'pending'.
    status        text NOT NULL CHECK (status IN ('building', 'registered', 'failed', 'withdrawn')),

    created_at    timestamptz NOT NULL DEFAULT now(),

    -- Both or neither. A hash with no deriver is a number nobody can interpret.
    CONSTRAINT app_builds_surface_is_attributable
        CHECK ((surface_hash IS NULL) = (derive_version IS NULL)),
    CONSTRAINT app_builds_wasm_has_a_module
        CHECK ((impl = 'wasm') = (module_sha256 IS NOT NULL)),
    CONSTRAINT app_builds_tools_have_no_storage
        CHECK (kind <> 'tool' OR NOT (manifest ? 'storage')),
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
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    -- The foreign key to installs is added after installs exists; the two
    -- tables reference each other and Postgres resolves a foreign key at
    -- CREATE time, where SQLite resolves it at write time.
    install_id  uuid NOT NULL,

    -- Who holds it. A principal, never an actor: an AI does not hold authority
    -- of its own, it acts for one that does.
    holder_kind text NOT NULL CHECK (holder_kind IN ('user', 'org')),
    holder_id   uuid NOT NULL REFERENCES actors (id) ON DELETE CASCADE,

    -- What it permits, unattended. One value today; the column exists so that
    -- adding "may uninstall" later is a value rather than a second table.
    capability  text NOT NULL CHECK (capability IN ('activate')),

    -- Always a human. Without this the rule is decorative: an AI acting for the
    -- install's owner would simply mint its own and promote its own output.
    granted_by_actor          uuid NOT NULL REFERENCES actors (id),
    granted_by_principal_kind text NOT NULL CHECK (granted_by_principal_kind IN ('user', 'org')),
    granted_by_principal_id   uuid NOT NULL REFERENCES actors (id),
    reason      text NOT NULL DEFAULT '',

    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz,
    revoked_at  timestamptz,
    revoked_by  uuid REFERENCES actors (id),

    CONSTRAINT install_authorities_revocation_is_attributed
        CHECK ((revoked_at IS NULL) = (revoked_by IS NULL)),
    UNIQUE (install_id, holder_kind, holder_id, capability)
);

CREATE INDEX install_authorities_live_idx
    ON install_authorities (install_id, holder_kind, holder_id)
    WHERE revoked_at IS NULL;

CREATE TABLE installs (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    build_id           uuid NOT NULL REFERENCES app_builds (id),
    -- Same alphabet as app_builds.slug, and for the same reason: this is the
    -- one the data layer actually reads when it composes an event kind.
    slug               text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,62}$'),

    -- An install is owned by the scope it is installed into; the columns are
    -- named owner_* so every grant-filtered read looks the same.
    owner_kind         text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id           uuid NOT NULL REFERENCES actors (id),
    installed_by_actor uuid NOT NULL REFERENCES actors (id),

    -- Exactly one of these authorises an active install: a human principal, or
    -- a standing authority on this install (which is how an unattended rebuild
    -- rolls a new build into an app a human already stood up).
    activated_by_actor      uuid REFERENCES actors (id),
    activation_authority_id uuid REFERENCES install_authorities (id) ON DELETE RESTRICT,

    -- The name of this install's collection tables' schema, derived from the
    -- INSTALL, never from the app alone, because two installs of one app are
    -- two schemas (invariant 14, the sixth instance). Under Postgres it is a
    -- schema again (D43 §2); under SQLite it was a table-name prefix.
    schema_name        text NOT NULL UNIQUE,
    state              text NOT NULL CHECK (state IN ('active', 'disabled', 'uninstalling')),
    created_at         timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT installs_active_is_authorised
        CHECK (state <> 'active'
               OR activated_by_actor IS NOT NULL
               OR activation_authority_id IS NOT NULL),

    UNIQUE (slug, owner_kind, owner_id)
);

CREATE INDEX installs_owner_idx ON installs (owner_kind, owner_id) WHERE state = 'active';

ALTER TABLE install_authorities
    ADD CONSTRAINT install_authorities_install_fk
    FOREIGN KEY (install_id) REFERENCES installs (id) ON DELETE CASCADE;

-- A trigger can check that the named activator is a human. It CANNOT check
-- that the named activator is the actor on the credential, because there is
-- no credential in scope here ... so an AI could register a build and
-- activate it by naming a human in this column.
--
-- That binding lives in store::activate_install, which sets this column from
-- the credential's actor and refuses anything else. Do not write to
-- installs.state directly; the schema looks like it handles this and it only
-- handles half.
CREATE FUNCTION installs_activation_check() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.activated_by_actor IS NOT NULL
       AND (SELECT kind FROM actors WHERE id = NEW.activated_by_actor) IS DISTINCT FROM 'human' THEN
        RAISE EXCEPTION 'install: activation needs a human principal or a standing grant (D19.4)';
    END IF;
    -- The standing authority is scoped to this one install, which is what
    -- "scoped to one specific app" buys: it cannot promote a build into
    -- anything else, and it grants no visibility into anything at all.
    IF NEW.activated_by_actor IS NULL
       AND NOT EXISTS (
            SELECT 1 FROM install_authorities ia
             WHERE ia.id = NEW.activation_authority_id
               AND ia.install_id = NEW.id
               AND ia.capability = 'activate'
               AND ia.revoked_at IS NULL
               AND (ia.expires_at IS NULL OR ia.expires_at > now())) THEN
        RAISE EXCEPTION 'install: authority is not a live activate authority on this install';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER installs_activation_check_insert
    AFTER INSERT ON installs
    FOR EACH ROW WHEN (NEW.state = 'active')
    EXECUTE FUNCTION installs_activation_check();

CREATE TRIGGER installs_activation_check_update
    AFTER UPDATE OF state, activated_by_actor, activation_authority_id ON installs
    FOR EACH ROW WHEN (NEW.state = 'active')
    EXECUTE FUNCTION installs_activation_check();

-- Who may write one. The same shape as the grant issue policy, for the same
-- reason: it has to hold for every writer that reaches this schema, not only
-- for the ones that remember to call a service.
CREATE FUNCTION install_authorities_issue_policy() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    o_kind text;
    o_id   uuid;
BEGIN
    IF (SELECT kind FROM actors WHERE id = NEW.granted_by_actor) IS DISTINCT FROM 'human' THEN
        RAISE EXCEPTION 'install authority: only a human may delegate unattended activation (D19.4)';
    END IF;
    SELECT owner_kind, owner_id INTO o_kind, o_id FROM installs WHERE id = NEW.install_id;
    IF o_kind IS NULL THEN
        RAISE EXCEPTION 'install authority: install does not exist';
    END IF;
    -- Only the owning principal delegates authority over its own install, and
    -- the granting actor has to actually be bound to that principal.
    IF o_kind IS DISTINCT FROM NEW.granted_by_principal_kind
       OR o_id IS DISTINCT FROM NEW.granted_by_principal_id THEN
        RAISE EXCEPTION 'install authority: only the owning principal may delegate on this install';
    END IF;
    IF acting_kind(NEW.granted_by_actor, NEW.granted_by_principal_kind, NEW.granted_by_principal_id) IS NULL THEN
        RAISE EXCEPTION 'install authority: granting actor is not bound to that principal';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER install_authorities_issue_policy
    BEFORE INSERT ON install_authorities
    FOR EACH ROW EXECUTE FUNCTION install_authorities_issue_policy();

-- Immutable except for revocation, for the same reason grants are: without it,
-- UPDATE walks around every rule above.
CREATE FUNCTION install_authorities_immutability() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.install_id IS DISTINCT FROM OLD.install_id
       OR NEW.holder_kind IS DISTINCT FROM OLD.holder_kind
       OR NEW.holder_id IS DISTINCT FROM OLD.holder_id
       OR NEW.capability IS DISTINCT FROM OLD.capability
       OR NEW.granted_by_actor IS DISTINCT FROM OLD.granted_by_actor
       OR NEW.granted_by_principal_kind IS DISTINCT FROM OLD.granted_by_principal_kind
       OR NEW.granted_by_principal_id IS DISTINCT FROM OLD.granted_by_principal_id
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR NEW.expires_at IS DISTINCT FROM OLD.expires_at THEN
        RAISE EXCEPTION 'an install authority is immutable except for revoked_at and revoked_by';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER install_authorities_immutability
    BEFORE UPDATE ON install_authorities
    FOR EACH ROW EXECUTE FUNCTION install_authorities_immutability();

-- The capability set of a manifest, sorted and deduplicated, so a reordered
-- manifest is not mistaken for a change: jsonb array equality is
-- order-sensitive, and a false "capabilities changed" trains people to click
-- through the true one. SQLite wrote this expression out four times because a
-- view cannot name a function there; here it is one function.
CREATE FUNCTION capability_set(m jsonb) RETURNS jsonb
LANGUAGE sql IMMUTABLE AS $$
    SELECT coalesce(jsonb_agg(DISTINCT c ORDER BY c), '[]'::jsonb)
      FROM jsonb_array_elements_text(coalesce(m -> 'capabilities', '[]'::jsonb)) AS c;
$$;

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
       capability_set(b.manifest) AS capabilities,
       CASE WHEN cb.id IS NULL THEN NULL ELSE capability_set(cb.manifest) END AS current_capabilities,

       -- The one that matters. A build whose capabilities differ from the live
       -- install is a CHANGE, not an equivalent promotion, and the risk is an
       -- app that has never had egress gaining it in v2 and being promoted as
       -- routine. Null for a first install, where there is nothing to compare
       -- against and every capability is new by definition.
       CASE WHEN i.build_id IS NULL THEN NULL
            ELSE capability_set(b.manifest) IS DISTINCT FROM capability_set(cb.manifest)
       END AS capability_change,

       -- Capabilities this build gains over the live one, so the reviewer reads
       -- the delta rather than diffing two arrays by eye.
       CASE WHEN i.build_id IS NULL THEN NULL
            ELSE (SELECT coalesce(jsonb_agg(c ORDER BY c), '[]'::jsonb)
                    FROM jsonb_array_elements_text(capability_set(b.manifest)) AS c
                   WHERE NOT capability_set(cb.manifest) ? c)
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
            WHEN b.derive_version IS DISTINCT FROM cb.derive_version THEN NULL
            ELSE b.surface_hash IS DISTINCT FROM cb.surface_hash
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
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind         text NOT NULL,
    install_id   uuid NOT NULL REFERENCES installs (id) ON DELETE CASCADE,
    collection   text NOT NULL,
    ref          text NOT NULL,

    owner_kind   text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id     uuid NOT NULL REFERENCES actors (id),
    author_actor uuid NOT NULL REFERENCES actors (id),

    trust        text NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),
    -- Which operation first weakened the invocation that wrote this row.
    -- Diagnostic only; see events.tainted_by.
    tainted_by   text,
    -- D17.12: cause_depth rides everything a run produces, not just mentions,
    -- or it cannot propagate and the loop guard has nothing to count.
    cause_depth  integer NOT NULL DEFAULT 0 CHECK (cause_depth >= 0),
    run_id       uuid,

    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    deleted_at   timestamptz,

    UNIQUE (install_id, collection, ref)
);

CREATE INDEX entities_owner_idx ON entities (owner_kind, owner_id, kind) WHERE deleted_at IS NULL;
CREATE INDEX entities_collection_idx ON entities (install_id, collection) WHERE deleted_at IS NULL;

CREATE TABLE links (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind         text NOT NULL,
    src_id       uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    dst_id       uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,

    owner_kind   text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id     uuid NOT NULL REFERENCES actors (id),
    author_actor uuid NOT NULL REFERENCES actors (id),

    meta         jsonb NOT NULL DEFAULT '{}',
    created_at   timestamptz NOT NULL DEFAULT now(),

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
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),

    -- Invariant 2 again: who created it, and whose authority it belongs to.
    author_actor uuid NOT NULL REFERENCES actors (id),
    owner_kind   text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id     uuid NOT NULL REFERENCES actors (id),

    -- Which agent this conversation is with, and how it runs. Pinned at
    -- creation so a resumed session cannot silently change model mid-thread.
    runtime      text NOT NULL,
    model        text NOT NULL DEFAULT '',

    title        text NOT NULL DEFAULT '',
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    archived_at  timestamptz
);

CREATE INDEX conversations_owner_idx
    ON conversations (owner_kind, owner_id, updated_at DESC)
    WHERE archived_at IS NULL;

-- ---------------------------------------------------------------------------
-- Where a subject's authority lives: the one relation the predicate and the
-- grant triggers resolve an owner through. tool, route and collection are
-- install-scoped, so subject_id is the install id for all three. A new
-- grantable kind is one more arm here and nothing else (D3's property).
--
-- A view on both engines, so the predicate text hive-store composes reads the
-- same name on either; the planner pushes `WHERE subject_kind = ? AND
-- subject_id = ?` into each arm, so a lookup is one indexed probe.
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
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),

    -- subject_id is the install id for install/tool/route/collection, the
    -- entity id for entity, the conversation id for conversation. subject_name
    -- qualifies the three install-scoped kinds. Keeping the install id in a
    -- column is what makes the allowlist rule a single query instead of a join
    -- through the manifest.
    subject_kind   text NOT NULL
        CHECK (subject_kind IN ('install', 'tool', 'route', 'collection', 'entity', 'conversation')),
    subject_id     uuid NOT NULL,
    subject_name   text,

    -- D33: an install is a target too, so an app can be granted another's
    -- collection. An install is not an actor, so it cannot live in target_id's
    -- foreign key; a second column keeps BOTH references real, and a deleted
    -- install cascades its grants away exactly as a deleted actor does.
    target_kind    text NOT NULL CHECK (target_kind IN ('user', 'org', 'install')),
    target_id      uuid REFERENCES actors (id) ON DELETE CASCADE,
    target_install_id uuid REFERENCES installs (id) ON DELETE CASCADE,
    access         text NOT NULL CHECK (access IN ('read', 'write', 'call')),

    source         text NOT NULL CHECK (source IN ('direct', 'inherited', 'override')),

    -- D18.3: inheritance is materialized. Revocation of a parent deletes every
    -- inherited child through this cascade, so the invariant is a foreign key
    -- rather than a code path someone can forget to call.
    inherited_from uuid REFERENCES grants (id) ON DELETE CASCADE,

    -- Provenance: a grantee can see why they can see something (D13.15).
    granted_by_actor          uuid NOT NULL REFERENCES actors (id),
    granted_by_principal_kind text NOT NULL CHECK (granted_by_principal_kind IN ('user', 'org')),
    granted_by_principal_id   uuid NOT NULL REFERENCES actors (id),
    reason         text NOT NULL DEFAULT '',

    created_at     timestamptz NOT NULL DEFAULT now(),
    expires_at     timestamptz,

    -- The tombstone, and it is deliberately NOT a second table. A revoked row
    -- is invisible to the predicate; it exists only so the inheritance
    -- materializer does not resurrect a deliberately narrowed child. The read
    -- path therefore still has exactly one policy: a live grant, or deny.
    revoked_at     timestamptz,
    revoked_by     uuid REFERENCES actors (id),

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

-- NULLS NOT DISTINCT so two install-subject rows (subject_name NULL) collide
-- rather than silently duplicating (Postgres 15+; the cluster is 17).
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
    subject_kind, subject_id, subject_name,
    target_kind, target_id, target_install_id,
    access, inherited_from
) NULLS NOT DISTINCT WHERE source <> 'override';

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
-- already have. The checks are in the order the first Postgres port evaluated
-- them, and each refusal names its rule, in the SQLite file's words.
-- ---------------------------------------------------------------------------
CREATE FUNCTION grants_issue_policy() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    o_kind   text;
    o_id     uuid;
    a_kind   text;
    a_p_kind text;
    a_p_id   uuid;
BEGIN
    SELECT so.owner_kind, so.owner_id INTO o_kind, o_id
      FROM subject_owners so
     WHERE so.subject_kind = NEW.subject_kind AND so.subject_id = NEW.subject_id;
    IF o_kind IS NULL THEN
        RAISE EXCEPTION 'grant refused: subject does not exist';
    END IF;
    SELECT kind, principal_kind, principal_id INTO a_kind, a_p_kind, a_p_id
      FROM actors WHERE id = NEW.granted_by_actor;
    IF a_kind IS NULL THEN
        RAISE EXCEPTION 'grant refused: granting actor does not exist';
    END IF;
    IF acting_kind(NEW.granted_by_actor, NEW.granted_by_principal_kind, NEW.granted_by_principal_id) IS NULL THEN
        RAISE EXCEPTION 'grant refused: granting actor is not bound to that principal';
    END IF;

    -- D18.2: produced by policy, org-owned rows only, human admin only. An AI
    -- never holds override and therefore never mints one either.
    IF NEW.source = 'override' THEN
        IF a_kind <> 'human' THEN
            RAISE EXCEPTION 'grant refused: only a human actor may enter break-glass (D18.2)';
        END IF;
        IF o_kind <> 'org' THEN
            RAISE EXCEPTION 'grant refused: override never reaches a personally-owned row (D18.2)';
        END IF;
        IF NOT EXISTS (SELECT 1 FROM org_members m
                        WHERE m.org_id = o_id AND m.user_id = NEW.granted_by_actor AND m.role = 'admin') THEN
            RAISE EXCEPTION 'grant refused: break-glass requires admin of the owning org (D18.2)';
        END IF;
        RETURN NEW;
    END IF;

    -- Sharing is not transfer (D13.10), and it is not laundering either: only
    -- the owner's principal may widen a row. A grantee reads and replies.
    IF o_kind IS DISTINCT FROM NEW.granted_by_principal_kind
       OR o_id IS DISTINCT FROM NEW.granted_by_principal_id THEN
        RAISE EXCEPTION 'grant refused: only the owning principal may grant on this subject';
    END IF;

    -- D13.14: a tag is an exfiltration primitive once an AI can write one. An
    -- AI may share with its own principal (widening nothing), with an org its
    -- principal belongs to, or with a member principal of the same org.
    -- Anything else needs a standing grant or a human.
    IF a_kind = 'ai'
       AND NOT (NEW.target_kind = a_p_kind AND NEW.target_id = a_p_id)
       AND NOT (NEW.target_kind = 'org' AND (
                   (a_p_kind = 'org' AND a_p_id = NEW.target_id)
                OR (a_p_kind = 'user' AND EXISTS (
                        SELECT 1 FROM org_members m
                         WHERE m.org_id = NEW.target_id AND m.user_id = a_p_id))))
       AND NOT (NEW.target_kind = 'user' AND a_p_kind = 'user' AND EXISTS (
                   SELECT 1 FROM org_members mine
                     JOIN org_members theirs ON theirs.org_id = mine.org_id
                    WHERE mine.user_id = a_p_id AND theirs.user_id = NEW.target_id)) THEN
        RAISE EXCEPTION 'grant refused: AI-authored share crosses a principal boundary; needs a standing grant or human confirmation (D13.14)';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER grants_issue_policy
    BEFORE INSERT ON grants
    FOR EACH ROW EXECUTE FUNCTION grants_issue_policy();

-- A grant is immutable except for its revocation.
--
-- The issue policy above only fires on INSERT, so without this an UPDATE walks
-- straight around every rule in it: retarget a live grant at an unrelated
-- principal, widen read to write, reattribute it to an AI, or promote source to
-- 'override' and mint break-glass without passing the admin check. All four
-- were reproduced against a real database. Pinning which columns an UPDATE may
-- touch closes the whole class at once, and it is a smaller rule than re-running
-- the issue policy on every narrow.
CREATE FUNCTION grants_immutability() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.id IS DISTINCT FROM OLD.id
       OR NEW.subject_kind IS DISTINCT FROM OLD.subject_kind
       OR NEW.subject_id IS DISTINCT FROM OLD.subject_id
       OR NEW.subject_name IS DISTINCT FROM OLD.subject_name
       OR NEW.target_kind IS DISTINCT FROM OLD.target_kind
       OR NEW.target_id IS DISTINCT FROM OLD.target_id
       OR NEW.target_install_id IS DISTINCT FROM OLD.target_install_id
       OR NEW.access IS DISTINCT FROM OLD.access
       OR NEW.source IS DISTINCT FROM OLD.source
       OR NEW.inherited_from IS DISTINCT FROM OLD.inherited_from
       OR NEW.granted_by_actor IS DISTINCT FROM OLD.granted_by_actor
       OR NEW.granted_by_principal_kind IS DISTINCT FROM OLD.granted_by_principal_kind
       OR NEW.granted_by_principal_id IS DISTINCT FROM OLD.granted_by_principal_id
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR NEW.expires_at IS DISTINCT FROM OLD.expires_at THEN
        RAISE EXCEPTION 'a grant is immutable except for revoked_at and revoked_by; delete it and write a new one';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER grants_immutability
    BEFORE UPDATE ON grants
    FOR EACH ROW EXECUTE FUNCTION grants_immutability();

-- D18.2: every access that succeeded ONLY because of an override is audited.
-- The predicate returns 'override' exactly in that case, which is what makes
-- "only because" mechanically decidable rather than a judgement call. The
-- row must survive the caller's transaction (a read can stream rows and then
-- roll back; the evidence must not roll back with it), which on Postgres is a
-- second connection, so the table is in this database rather than in a file
-- of its own as it is on SQLite (D38 §3). No foreign keys, as on SQLite: an
-- audit row outlives the grant and the actor it names, which is the right
-- direction for evidence. The owner is what the predicate resolved from the
-- subject at the moment of the decision, never a value the caller supplied.
CREATE TABLE grant_override_audit (
    id             bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    grant_id       uuid,
    actor_id       uuid NOT NULL,
    principal_kind text NOT NULL CHECK (principal_kind IN ('user', 'org')),
    principal_id   uuid NOT NULL,
    subject_kind   text NOT NULL,
    subject_id     uuid NOT NULL,
    subject_name   text,
    owner_kind     text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id       uuid NOT NULL,
    access         text NOT NULL,
    reason         text NOT NULL DEFAULT '',
    occurred_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX grant_override_audit_actor_idx ON grant_override_audit (actor_id, occurred_at DESC);

-- ---------------------------------------------------------------------------
-- Events: append-only, the transport of record (D4.5).
--
-- Not partitioned (D38 §3, kept by D43): one database is one log, and the
-- monthly partitions of the first Postgres port bought a DEFAULT partition
-- that could never be pruned. The cursor is the pair (created_at, id), as
-- docs/events-tailing.md says, because a late-committing transaction's row
-- carries a lower id than rows already read.
-- ---------------------------------------------------------------------------

CREATE TABLE events (
    id             bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    created_at     timestamptz NOT NULL DEFAULT now(),

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
    -- name) cannot silently reopen frame injection. (The SQLite text also
    -- guards a NUL by byte length; Postgres refuses a NUL in text at input.)
    kind           text NOT NULL
                     CONSTRAINT events_kind_is_an_identifier
                         CHECK (kind ~ '^[a-z0-9][a-z0-9._-]{0,127}$')
                     CONSTRAINT events_kind_has_no_frame_separator
                         CHECK (kind !~ '[[:cntrl:]]'),

    -- What the event is about, in the shape the predicate takes, so replay
    -- filters with the same rule as a live read. 'collection' is deliberately
    -- absent: no writer produces one, and the feed's predicate cannot decide a
    -- collection without an acting install (D33), so the one guard is here at
    -- the INSERT rather than at every read of the feed.
    subject_kind   text
        CONSTRAINT events_subject_kind_check
        CHECK (subject_kind IN ('install', 'tool', 'route', 'entity', 'conversation')),
    subject_id     uuid,
    subject_name   text,

    owner_kind     text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id       uuid NOT NULL,
    author_actor   uuid NOT NULL,
    principal_kind text NOT NULL CHECK (principal_kind IN ('user', 'org')),
    principal_id   uuid NOT NULL,

    body           jsonb NOT NULL DEFAULT '{}',
    trust          text NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),
    -- Which operation FIRST weakened the invocation that produced this row.
    -- Diagnostic only: nothing branches on it, and nothing should. Without it,
    -- an untrusted row says it is untrusted and the answer to "why did this
    -- lose egress" lives in a log somebody has to still have.
    tainted_by     text,
    cause_depth    integer NOT NULL DEFAULT 0 CHECK (cause_depth >= 0),
    run_id         uuid,

    -- D4.12: cross-hive bridging is far future, but this is the piece that is
    -- painful to retrofit and it costs nothing today.
    origin         text NOT NULL DEFAULT 'local',
    origin_id      text
);

CREATE INDEX events_cursor_idx ON events (created_at, id);
CREATE INDEX events_owner_idx ON events (owner_kind, owner_id, created_at DESC);
CREATE INDEX events_subject_idx ON events (subject_kind, subject_id, created_at DESC);

-- created_at is a LOCAL ingest timestamp, not a claim about when something
-- happened elsewhere. The realistic causes of a future value are exactly the
-- ones D4.12 plans for: clock skew, and a bridged event carrying another
-- hive's timestamp. A bridge puts the origin's timestamp in the body, where it
-- belongs.
CREATE FUNCTION events_no_future_timestamps() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.created_at > now() + interval '1 hour' THEN
        RAISE EXCEPTION 'events.created_at is more than an hour ahead of the server clock; it is a local ingest time, not the origin''s timestamp';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER events_no_future_timestamps
    BEFORE INSERT ON events
    FOR EACH ROW EXECUTE FUNCTION events_no_future_timestamps();

-- D4.12 asks for (origin, origin_id) unique from the first migration. Global
-- uniqueness lives in a small side table, written by a trigger, and only for
-- events that actually came from somewhere else. Locally produced events leave
-- origin_id NULL and cost nothing.
CREATE TABLE event_origins (
    origin           text NOT NULL,
    origin_id        text NOT NULL,
    event_id         bigint NOT NULL,
    event_created_at timestamptz NOT NULL,
    PRIMARY KEY (origin, origin_id)
);

CREATE FUNCTION events_origin_dedupe() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO event_origins (origin, origin_id, event_id, event_created_at)
    VALUES (NEW.origin, NEW.origin_id, NEW.id, NEW.created_at);
    RETURN NEW;
END;
$$;

CREATE TRIGGER events_origin_dedupe
    AFTER INSERT ON events
    FOR EACH ROW WHEN (NEW.origin_id IS NOT NULL)
    EXECUTE FUNCTION events_origin_dedupe();

-- Append-only means append-only.
CREATE FUNCTION events_append_only() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'events is append-only';
END;
$$;

CREATE TRIGGER events_append_only_update
    BEFORE UPDATE ON events
    FOR EACH ROW EXECUTE FUNCTION events_append_only();

CREATE TRIGGER events_append_only_delete
    BEFORE DELETE ON events
    FOR EACH ROW EXECUTE FUNCTION events_append_only();

-- ---------------------------------------------------------------------------
-- Mentions: host-owned, because a tag is a permission act (D13).
-- ---------------------------------------------------------------------------

CREATE TABLE mentions (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    entity_id        uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    mentioned_actor  uuid NOT NULL REFERENCES actors (id),

    -- D13.8: the grant goes to the tagged actor's PRINCIPAL. An AI does not own
    -- memory, so it cannot be the target of a share either.
    principal_kind   text NOT NULL CHECK (principal_kind IN ('user', 'org')),
    principal_id     uuid NOT NULL REFERENCES actors (id),

    author_actor     uuid NOT NULL REFERENCES actors (id),
    owner_kind       text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id         uuid NOT NULL REFERENCES actors (id),

    state            text NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'delivered', 'acknowledged', 'actioned', 'dropped')),
    -- A denied cross-boundary tag is recorded with a reason, not dropped
    -- silently: the AI should be able to say "I wanted to loop in the other
    -- assistant and could not" (D13.14).
    drop_reason      text,

    -- The share this tag wrote, in the same transaction as the entry and the
    -- mention (D13.2). SET NULL rather than CASCADE: revoking the share must
    -- not erase the record that the tag happened.
    grant_id         uuid REFERENCES grants (id) ON DELETE SET NULL,

    run_id           uuid,
    cause_depth      integer NOT NULL DEFAULT 0 CHECK (cause_depth >= 0),
    trust            text NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),

    delivered_at     timestamptz,
    acknowledged_at  timestamptz,
    created_at       timestamptz NOT NULL DEFAULT now(),

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
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    install_id   uuid REFERENCES installs (id) ON DELETE CASCADE,
    name         text NOT NULL,
    spec         jsonb NOT NULL,
    content_hash text NOT NULL UNIQUE CHECK (content_hash ~ '^[0-9a-f]{64}$'),

    owner_kind   text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id     uuid NOT NULL REFERENCES actors (id),
    author_actor uuid NOT NULL REFERENCES actors (id),

    enabled      boolean NOT NULL DEFAULT true,
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX workflow_defs_name_idx ON workflow_defs (name, created_at DESC);

-- Definitions are immutable and content-addressed. An AI editing a live
-- definition would otherwise change what an in-flight run resumes into. Editing
-- means writing a new row with a new hash and pointing triggers at it.
CREATE FUNCTION workflow_defs_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.spec IS DISTINCT FROM OLD.spec OR NEW.content_hash IS DISTINCT FROM OLD.content_hash THEN
        RAISE EXCEPTION 'workflow_defs.spec and content_hash are immutable; write a new definition';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER workflow_defs_immutable
    BEFORE UPDATE ON workflow_defs
    FOR EACH ROW EXECUTE FUNCTION workflow_defs_immutable();

CREATE TABLE workflow_triggers (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    def_id     uuid NOT NULL REFERENCES workflow_defs (id) ON DELETE CASCADE,
    kind       text NOT NULL CHECK (kind IN ('event', 'cron', 'manual', 'webhook')),
    match      jsonb,
    cron_expr  text,

    owner_kind text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id   uuid NOT NULL REFERENCES actors (id),
    enabled    boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT workflow_triggers_cron_expr CHECK ((kind = 'cron') = (cron_expr IS NOT NULL))
);

CREATE INDEX workflow_triggers_event_idx ON workflow_triggers (kind) WHERE enabled;

CREATE TABLE workflow_runs (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    def_id          uuid NOT NULL REFERENCES workflow_defs (id),
    -- Pinned at start. Resume reads recorded step results and never re-walks
    -- the definition, so a run is immune to a definition edit mid-flight.
    definition_hash text NOT NULL,
    trigger_id      uuid REFERENCES workflow_triggers (id) ON DELETE SET NULL,

    actor_id        uuid NOT NULL REFERENCES actors (id),
    owner_kind      text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id        uuid NOT NULL REFERENCES actors (id),

    input           jsonb NOT NULL DEFAULT '{}',
    state           text NOT NULL DEFAULT 'running'
        CHECK (state IN ('running', 'waiting', 'succeeded', 'failed', 'cancelled')),

    -- Idempotent cron enqueue (D4.3) keys on this: the trigger id plus the
    -- fire time, INSERT ... ON CONFLICT DO NOTHING, RETURNING decides whether
    -- to notify at all.
    idem_key        text UNIQUE,

    -- D17.12 / D17.3: both ride the run, and everything the run produces
    -- inherits them. An untrusted causal chain costs the run its egress.
    cause_depth     integer NOT NULL DEFAULT 0 CHECK (cause_depth >= 0),
    trust           text NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),
    egress_allowed  boolean NOT NULL DEFAULT false,

    steps_used      integer NOT NULL DEFAULT 0,
    max_steps       integer NOT NULL DEFAULT 100 CHECK (max_steps > 0),
    deadline_at     timestamptz,
    started_at      timestamptz NOT NULL DEFAULT now(),
    ended_at        timestamptz,
    error           text,

    -- The trifecta rule, enforced at spawn rather than trusted to a prompt
    -- (D17.3): if anything in the causal chain is untrusted, the run has no
    -- egress unless a human granted that combination explicitly.
    CONSTRAINT workflow_runs_untrusted_has_no_egress
        CHECK (trust = 'trusted' OR NOT egress_allowed)
);

CREATE INDEX workflow_runs_state_idx ON workflow_runs (state, started_at) WHERE state IN ('running', 'waiting');
CREATE INDEX workflow_runs_owner_idx ON workflow_runs (owner_kind, owner_id, started_at DESC);

CREATE TABLE workflow_steps (
    id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id               uuid NOT NULL REFERENCES workflow_runs (id) ON DELETE CASCADE,
    parent_step_id       uuid REFERENCES workflow_steps (id) ON DELETE CASCADE,
    seq                  integer NOT NULL,
    name                 text NOT NULL,
    type                 text NOT NULL CHECK (type IN (
        'wasm_call', 'http', 'agent_run', 'emit', 'workflow_call', 'sleep', 'wait_for_event')),

    -- Declared, not assumed (D8.4). agent_run spends money, so its default is
    -- at_most_once everywhere (D17.8) and the CHECK stops a definition author
    -- from talking us out of it.
    retry_policy         text NOT NULL CHECK (retry_policy IN ('at_least_once', 'at_most_once')),

    input                jsonb NOT NULL DEFAULT '{}',
    output               jsonb,
    output_trust         text NOT NULL DEFAULT 'trusted' CHECK (output_trust IN ('trusted', 'untrusted')),

    state                text NOT NULL DEFAULT 'pending' CHECK (state IN (
        'pending', 'leased', 'waiting_timer', 'waiting_event',
        'succeeded', 'failed', 'skipped', 'indeterminate')),

    attempt              integer NOT NULL DEFAULT 0,
    max_attempts         integer NOT NULL DEFAULT 1 CHECK (max_attempts > 0),
    next_attempt_at      timestamptz NOT NULL DEFAULT now(),

    lease_owner          text,
    lease_expires_at     timestamptz,
    heartbeat_at         timestamptz,

    wake_at              timestamptz,
    wait_match           jsonb,

    idem_key             text,
    pending_children     integer NOT NULL DEFAULT 0 CHECK (pending_children >= 0),
    continue_on_error    boolean NOT NULL DEFAULT false,

    error                text,
    -- An at-most-once step reclaimed from a dead lease cannot know whether its
    -- effect happened. It lands here rather than re-firing (invariant 10).
    indeterminate_reason text,

    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),

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

-- The claim path: FOR UPDATE SKIP LOCKED over this index (D43 §2).
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

-- chat_turns is referenced by agent_runs and references it back; the
-- agent_runs side is added after chat_turns exists, below.
CREATE TABLE agent_runs (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),

    -- Invariant 2, and the reason this table exists in the shape it does.
    -- author_actor is WHO AUTHORED the run and may be an AI; owner_* is whose
    -- authority is being spent and is never an AI. "Nate ran this" and "an AI
    -- acting for Nate ran this" must stay distinguishable on every row.
    --
    -- Both are pinned by the writer from the credential. They are NOT on
    -- RunRecord and must never be added to it: a caller that supplies them is
    -- supplying the fact the row is deciding about (invariant 11), and there
    -- are then as many enforcement points as call sites.
    author_actor    uuid NOT NULL REFERENCES actors (id),
    owner_kind      text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id        uuid NOT NULL REFERENCES actors (id),

    -- The agent's own identity, when the run acts as one. Distinct from
    -- author_actor: an AI may launch a run that acts as a different AI.
    agent_actor     uuid REFERENCES actors (id),

    -- Nullable on purpose. A run started from a chat has no step.
    workflow_step_id uuid REFERENCES workflow_steps (id) ON DELETE SET NULL,

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
    run_key         text NOT NULL UNIQUE CHECK (run_key ~ '^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,62}$'),

    -- What actually ran, from RunRecord.
    runtime         text NOT NULL,
    image_digest    text NOT NULL,
    cli_version     text NOT NULL DEFAULT '',
    model           text NOT NULL DEFAULT '',

    -- Scraped from the CLI's own output when it announces one, so a follow-up
    -- run can resume the conversation. Minted by the agent CLI, so it is NOT a
    -- capability: anything keyed on it alone would let a session id act as
    -- permission, which is invariant 14's shape.
    session_id      text NOT NULL DEFAULT '',

    -- These are harness NetworkMode's values VERBATIM: none, daemon, proxied.
    -- The names here are not descriptions; they are the constants, and
    -- drifting from them is silent until the one mode nobody tested is used.
    network         text NOT NULL CHECK (network IN ('none', 'daemon', 'proxied')),
    memory_bytes    bigint NOT NULL DEFAULT 0 CHECK (memory_bytes >= 0),
    cpus            double precision NOT NULL DEFAULT 0 CHECK (cpus >= 0),
    pids_limit      bigint NOT NULL DEFAULT 0 CHECK (pids_limit >= 0),

    -- Invariant 12. Monotonic, and recorded from the invocation rather than
    -- claimed by the run.
    trust           text NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),

    -- harness TerminalState's values VERBATIM, plus 'running'.
    --
    -- INVARIANT 10 lives here. A harness run spends money, so a lease reclaim
    -- must land 'indeterminate' rather than re-firing. 'indeterminate' is
    -- therefore a first-class terminal state, not an error: it means the run
    -- may or may not have completed and NOTHING may retry it automatically.
    state           text NOT NULL DEFAULT 'running'
        CHECK (state IN ('running', 'succeeded', 'failed', 'deadline_exceeded',
                         'cancelled', 'indeterminate')),

    -- A run caused by another run, so a loop guard has something to count
    -- (D17.12). An agent that spawns an agent that spawns an agent is the
    -- shape that spends money without a human ever seeing it.
    cause_depth     integer NOT NULL DEFAULT 0 CHECK (cause_depth >= 0),

    exit_code       integer,
    event_count     bigint NOT NULL DEFAULT 0 CHECK (event_count >= 0),
    stderr_tail     text NOT NULL DEFAULT '',

    -- Reclaim bookkeeping. deadline_at is when a lease is considered lost.
    started_at      timestamptz NOT NULL DEFAULT now(),
    heartbeat_at    timestamptz,
    deadline_at     timestamptz,
    ended_at        timestamptz,

    -- The conversation and the turn this run answers, when it answers one.
    conversation_id uuid REFERENCES conversations (id) ON DELETE SET NULL,
    turn_id         uuid,

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
    owner_kind text NOT NULL CHECK (owner_kind IN ('user', 'org')),
    owner_id   uuid NOT NULL REFERENCES actors (id),
    idem_key   text NOT NULL,
    run_id     uuid NOT NULL REFERENCES agent_runs (id) ON DELETE CASCADE,
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
    run_id  uuid NOT NULL REFERENCES agent_runs (id) ON DELETE CASCADE,

    -- Starts at 1 and is unique within a run. The primary key is (run_id, seq)
    -- rather than a surrogate id: it is what the drain path already has, it
    -- makes an accidental double-append a constraint violation rather than a
    -- duplicate line, and it gives ordered reads for free.
    seq     bigint NOT NULL CHECK (seq >= 1),

    at      timestamptz NOT NULL DEFAULT now(),
    stream  text NOT NULL CHECK (stream IN ('stdout', 'stderr')),

    -- The stream-json "type" field, empty when the line was not JSON.
    type    text NOT NULL DEFAULT '',
    -- The parsed line, null when the line was not JSON.
    body    jsonb,
    -- The raw line, always. A line that failed to parse is still evidence.
    text    text NOT NULL DEFAULT '',

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
    conversation_id uuid NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,

    -- Dense per conversation, assigned by the writer inside the transaction
    -- that appends. (conversation_id, seq) is the primary key rather than a
    -- surrogate: it is what a client pages on, it makes a double-post a
    -- constraint violation instead of a duplicate message, and it gives ordered
    -- reads without a sort.
    seq             bigint NOT NULL CHECK (seq >= 1),

    -- 'user' is a person, 'agent' is the AI, 'system' is the platform.
    role            text NOT NULL CHECK (role IN ('user', 'agent', 'system')),

    -- Who actually wrote it. An agent message has the agent's actor here, which
    -- is what makes "an AI acting for Nate said this" recoverable later.
    author_actor    uuid NOT NULL REFERENCES actors (id),

    body            text NOT NULL,

    -- Invariant 9. A message from a browser is first-party input and trusted;
    -- an agent message that quoted fetched content is not, and must stay marked
    -- so downstream turns inherit it.
    trust           text NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),

    -- The run that produced an agent message. Null for a user message.
    run_id          uuid REFERENCES agent_runs (id) ON DELETE SET NULL,

    created_at      timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (conversation_id, seq)
);

-- A turn is a durable claim: "this message needs an agent run". It exists so a
-- crash between accepting a message and starting a run does not lose the turn,
-- and so exactly one worker acts on it.
CREATE TABLE chat_turns (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    conversation_id uuid NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,

    -- The user message this turn answers.
    request_seq     bigint NOT NULL,

    state           text NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'claimed', 'done', 'failed')),

    -- A claim under FOR UPDATE SKIP LOCKED plus a lease and a heartbeat, as
    -- the repo's convention requires. A claim that stops heartbeating is
    -- reclaimable.
    claimed_by       text,
    claimed_at       timestamptz,
    lease_expires_at timestamptz,

    run_id          uuid REFERENCES agent_runs (id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT chat_turns_claim_is_complete
        CHECK ((state = 'pending') = (claimed_by IS NULL)),

    -- ONE turn per user message. This is the at-most-once guard for chat, the
    -- analogue of agent_runs_step_uq for workflow steps: a client retrying a
    -- post, or two workers racing, cannot produce a second paid run for one
    -- message.
    CONSTRAINT chat_turns_one_per_message UNIQUE (conversation_id, request_seq)
);

ALTER TABLE agent_runs
    ADD CONSTRAINT agent_runs_turn_fk
    FOREIGN KEY (turn_id) REFERENCES chat_turns (id) ON DELETE SET NULL;

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
    conversation_id uuid PRIMARY KEY REFERENCES conversations (id) ON DELETE CASCADE,
    runtime         text NOT NULL,

    -- Scraped from the CLI's own output. Empty until the first run reports one,
    -- which is why the first turn starts fresh and every later turn resumes.
    session_id      text NOT NULL DEFAULT '',
    updated_at      timestamptz NOT NULL DEFAULT now()
);
