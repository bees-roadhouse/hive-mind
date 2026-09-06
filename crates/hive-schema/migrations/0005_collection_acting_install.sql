-- D33: cross-app collection access is decided on the ASKING install as well as
-- on the principal.
--
-- Before this, access_decision() resolved a 'collection' subject's owner
-- through subject_owner(), which for install-scoped kinds is the INSTALL's
-- owner, and returned 'owner' on the first branch. It was never told which
-- install was asking, so two installs of one principal were indistinguishable
-- to it and the collection grants D32 derives decided nothing.
--
-- That is invariant 14, and it is the fail-open kind: it did not collide and
-- refuse, it resolved to 'owner' and the read succeeded, with the audit
-- recording it honestly as the principal's own access ... because it was.
--
-- The rule this installs: when a collection is reached THROUGH an install that
-- is not the one owning it, the principal check still has to pass AND the
-- asking install must hold a live grant. Both, never either. The principal half
-- is what keeps invariant 2 whole: widening access to an app must never widen
-- it past the person the app acts for.
--
-- p_acting_install NULL means "not reached through an install" ... a person on
-- the HTTP surface reading their own data. It does NOT mean "no restriction":
-- the restriction only has meaning when there is an asking install to name, and
-- the Rust side is where a guest path that forgot to pass one is prevented,
-- because SQL cannot tell a forgotten argument from an absent one.

-- An install is not an actor, so it cannot live in target_id's foreign key.
-- A second column keeps BOTH references real, which matters: a deleted actor
-- must still cascade its grants away, and now so must a deleted install.
ALTER TABLE grants ADD COLUMN target_install_id uuid REFERENCES installs (id) ON DELETE CASCADE;

ALTER TABLE grants DROP CONSTRAINT grants_target_kind_check;
ALTER TABLE grants ADD CONSTRAINT grants_target_kind_check
    CHECK (target_kind IN ('user', 'org', 'install'));

-- target_id was NOT NULL for a two-kind world. Exactly one of the two target
-- columns is set, and which one is decided by target_kind rather than by
-- whichever the writer happened to fill in.
ALTER TABLE grants ALTER COLUMN target_id DROP NOT NULL;
ALTER TABLE grants ADD CONSTRAINT grants_target_shape CHECK (
    (target_kind IN ('user', 'org') AND target_id IS NOT NULL AND target_install_id IS NULL)
 OR (target_kind = 'install'       AND target_id IS NULL     AND target_install_id IS NOT NULL)
);

-- An install grant is a derived consequence of a manifest declaration that a
-- human activated (D19), never a break-glass. Override is a human-only,
-- time-boxed, org-owned path and an install is none of those things.
ALTER TABLE grants ADD CONSTRAINT grants_install_target_is_not_override
    CHECK (target_kind <> 'install' OR source <> 'override');

CREATE INDEX grants_install_target_idx
    ON grants (target_install_id, subject_kind, subject_id, subject_name)
 WHERE target_install_id IS NOT NULL AND revoked_at IS NULL;

DROP FUNCTION access_reason(text, uuid, text, text, uuid, uuid, text, timestamptz);
DROP FUNCTION access_decision(text, uuid, text, text, uuid, uuid, text, timestamptz);

CREATE FUNCTION access_decision(
    p_subject_kind   text,
    p_subject_id     uuid,
    p_subject_name   text,
    p_principal_kind text,
    p_principal_id   uuid,
    p_actor_id       uuid,
    p_access         text,
    p_now            timestamptz DEFAULT now(),
    p_acting_install uuid DEFAULT NULL
) RETURNS TABLE (reason text, grant_id uuid)
LANGUAGE plpgsql STABLE PARALLEL SAFE AS $fn$
DECLARE
    a_kind text;
    o_kind text;
    o_id   uuid;
    g_id   uuid;
BEGIN
    reason := NULL;
    grant_id := NULL;

    a_kind := acting_kind(p_actor_id, p_principal_kind, p_principal_id);
    IF a_kind IS NULL THEN
        RETURN NEXT;
        RETURN;
    END IF;

    -- Absence of scope is deny, and a subject nobody owns has no scope.
    SELECT so.owner_kind, so.owner_id INTO o_kind, o_id
      FROM subject_owner(p_subject_kind, p_subject_id) so;
    IF o_kind IS NULL THEN
        RETURN NEXT;
        RETURN;
    END IF;

    -- 1. The principal owns the row.
    IF o_kind = p_principal_kind AND o_id = p_principal_id THEN
        reason := 'owner';

    -- 2. A grant written against this principal. Direct and inherited are the
    --    same row shape by construction (D18.3), so they are one branch.
    ELSE
        SELECT g.id INTO g_id FROM grants g
         WHERE g.subject_kind = p_subject_kind
           AND g.subject_id = p_subject_id
           AND g.subject_name IS NOT DISTINCT FROM p_subject_name
           AND g.target_kind = p_principal_kind
           AND g.target_id = p_principal_id
           AND g.source <> 'override'
           AND access_satisfies(g.access, p_access)
           AND g.revoked_at IS NULL
           AND (g.expires_at IS NULL OR g.expires_at > p_now)
         LIMIT 1;
        IF g_id IS NOT NULL THEN
            reason := 'grant';
        END IF;

        -- 3. A grant written against an org this principal belongs to.
        --    Resolved at read time against membership, never materialized per
        --    member (D18.3): materialized rows would be wrong the moment
        --    membership changes.
        IF reason IS NULL AND p_principal_kind = 'user' THEN
            SELECT g.id INTO g_id
              FROM grants g
              JOIN org_members m ON m.org_id = g.target_id AND m.user_id = p_principal_id
             WHERE g.subject_kind = p_subject_kind
               AND g.subject_id = p_subject_id
               AND g.subject_name IS NOT DISTINCT FROM p_subject_name
               AND g.target_kind = 'org'
               AND g.source <> 'override'
               AND access_satisfies(g.access, p_access)
               AND g.revoked_at IS NULL
               AND (g.expires_at IS NULL OR g.expires_at > p_now)
             LIMIT 1;
            IF g_id IS NOT NULL THEN
                reason := 'org_grant';
            END IF;
        END IF;

        -- 4. Admin override. A grant produced by policy, evaluated in the same
        --    predicate as everything else, under four constraints (D18.2):
        --    org-owned rows only, time-boxed, a human actor only, and audited
        --    by the caller ... which is safe to require because this branch is
        --    reached only when nothing above it fired.
        IF reason IS NULL AND a_kind = 'human' AND o_kind = 'org' THEN
            SELECT g.id INTO g_id
              FROM grants g
              JOIN org_members m ON m.org_id = o_id AND m.user_id = p_actor_id
             WHERE g.subject_kind = p_subject_kind
               AND g.subject_id = p_subject_id
               AND g.subject_name IS NOT DISTINCT FROM p_subject_name
               AND g.source = 'override'
               AND m.role = 'admin'
               AND g.target_kind = 'user'
               AND g.target_id = p_actor_id
               AND access_satisfies(g.access, p_access)
               AND g.revoked_at IS NULL
               AND g.expires_at > p_now
             LIMIT 1;
            IF g_id IS NOT NULL THEN
                reason := 'override';
            END IF;
        END IF;
    END IF;

    IF reason IS NULL THEN
        grant_id := NULL;
        RETURN NEXT;
        RETURN;
    END IF;

    -- D33. The principal is allowed. Now: is the ASKING app allowed?
    --
    -- Only for a collection reached through some other install. An app touching
    -- its own collections is the ordinary case and asks nothing extra; a person
    -- with no acting install is not reaching through an app at all.
    IF p_subject_kind = 'collection'
       AND p_acting_install IS NOT NULL
       AND p_acting_install <> p_subject_id
    THEN
        SELECT g.id INTO g_id FROM grants g
         WHERE g.subject_kind = 'collection'
           AND g.subject_id = p_subject_id
           AND g.subject_name IS NOT DISTINCT FROM p_subject_name
           AND g.target_kind = 'install'
           AND g.target_install_id = p_acting_install
           AND access_satisfies(g.access, p_access)
           AND g.revoked_at IS NULL
           AND (g.expires_at IS NULL OR g.expires_at > p_now)
         LIMIT 1;
        IF g_id IS NULL THEN
            -- The person may read it; this app may not.
            reason := NULL;
            grant_id := NULL;
            RETURN NEXT;
            RETURN;
        END IF;
        -- The install grant is the narrower of the two and is what the caller
        -- is shown, so provenance names the row a person can revoke to close
        -- this door without touching their own access.
        reason := 'install_grant';
    END IF;

    grant_id := g_id;
    RETURN NEXT;
END;
$fn$;

-- The single-value form, for composing into a WHERE clause. Same decision, same
-- function, so a set read cannot drift from a point check.
--
-- A set read that uses this still owes the D18.2 audit for every row whose
-- reason is 'override'. store.Guard is the only thing that may call it, and it
-- discharges that obligation on every path.
CREATE FUNCTION access_reason(
    p_subject_kind   text,
    p_subject_id     uuid,
    p_subject_name   text,
    p_principal_kind text,
    p_principal_id   uuid,
    p_actor_id       uuid,
    p_access         text,
    p_now            timestamptz DEFAULT now(),
    p_acting_install uuid DEFAULT NULL
) RETURNS text
LANGUAGE sql STABLE PARALLEL SAFE AS $fn$
    SELECT reason FROM access_decision(p_subject_kind, p_subject_id, p_subject_name,
                                       p_principal_kind, p_principal_id, p_actor_id,
                                       p_access, p_now, p_acting_install);
$fn$;
