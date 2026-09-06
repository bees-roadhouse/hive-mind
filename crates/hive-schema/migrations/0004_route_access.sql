-- Route access, allowlist only, the same rule tools have (D18.1): an install
-- grant with no route allowlist implies every route the app mounts; with one,
-- exactly those routes. A route is named "<METHOD> <path template>", the key
-- the manifest already makes unique per app.
--
-- Mirrors tool_access_reason line for line, including the narrow probe: only a
-- live, non-override, call-bearing route grant flips the install onto the
-- allowlist path, because an override row is read-only by CHECK and would
-- otherwise revoke every route at the moment an admin reached for one.
CREATE FUNCTION route_access_reason(
    p_install_id     uuid,
    p_route_name     text,
    p_principal_kind text,
    p_principal_id   uuid,
    p_actor_id       uuid,
    p_now            timestamptz DEFAULT now()
) RETURNS text
LANGUAGE plpgsql STABLE PARALLEL SAFE AS $fn$
DECLARE
    has_allowlist boolean;
BEGIN
    SELECT EXISTS (
        SELECT 1 FROM grants g
         WHERE g.subject_kind = 'route'
           AND g.subject_id = p_install_id
           AND g.source <> 'override'
           AND g.access = 'call'
           AND g.revoked_at IS NULL
           AND (g.expires_at IS NULL OR g.expires_at > p_now)
           AND (
                (g.target_kind = p_principal_kind AND g.target_id = p_principal_id)
             OR (p_principal_kind = 'user' AND g.target_kind = 'org' AND EXISTS (
                    SELECT 1 FROM org_members m
                     WHERE m.org_id = g.target_id AND m.user_id = p_principal_id))
           )
    ) INTO has_allowlist;

    IF has_allowlist THEN
        RETURN access_reason('route', p_install_id, p_route_name,
                             p_principal_kind, p_principal_id, p_actor_id, 'call', p_now);
    END IF;

    RETURN access_reason('install', p_install_id, NULL,
                         p_principal_kind, p_principal_id, p_actor_id, 'call', p_now);
END;
$fn$;
