//! The one SQL text that answers "may this actor do this" (D38 §2).
//!
//! Migration one on Postgres installed `access_decision()` as a stored
//! function and `Guard` called it. SQLite has no stored functions, so the
//! same decision is a SQL text this module composes and `Guard` is still its
//! only caller: the point check binds parameters into it, and every set read
//! (visible entities, visible conversations, the event feed) embeds it as a
//! correlated subquery over the row being filtered. One text, so a set read
//! cannot drift from a point check ... which is the property the stored
//! function had and the reason it was one function.
//!
//! Nothing outside this crate may reference these; nothing inside it outside
//! `grants.rs` and `events.rs` does.
//!
//! # The shape
//!
//! `decision` is a `SELECT reason, grant_id` over three nested derived
//! tables, because SQL has no `let`: the inner one resolves the acting kind
//! and the subject's owner and probes each grant branch once; the middle one
//! picks the principal's branch in the order D18 fixes (owner, grant, org
//! grant, override ... override last, so seeing it means nothing else would
//! have worked); the outer one applies D33's collection gate. Every argument
//! is a SQL expression the caller supplies: a `?N` placeholder for a point
//! check, a column reference for a set read.
//!
//! Note what is NOT in the signature: the owner. It is resolved from the
//! subject through the `subject_owners` view, so no caller can supply one
//! (invariant 11).

/// The arguments, as SQL expressions. A point check passes placeholders; a set
/// read passes column references for the subject and placeholders for the rest.
pub(crate) struct Args<'a> {
    pub subject_kind: &'a str,
    pub subject_id: &'a str,
    pub subject_name: &'a str,
    pub principal_kind: &'a str,
    pub principal_id: &'a str,
    pub actor_id: &'a str,
    pub access: &'a str,
    pub now: &'a str,
    /// `NULL` for "not reached through an install" (D33). It does not mean
    /// "no restriction": the restriction only has meaning when there is an
    /// asking install to name, and `Guard` is where a guest path that forgot
    /// to pass one is prevented, because SQL cannot tell a forgotten argument
    /// from an absent one.
    pub acting_install: &'a str,
}

/// Credential coherence (D17.4). The credential pins author_actor AND owner
/// principal, and the two have to agree or the pair proves nothing. Evaluates
/// to the acting actor's kind, or NULL when the pair does not hold up.
///
/// Doing this HERE rather than at the edge is what makes "an AI never gains
/// authority its principal lacks" structural: an AI cannot be handed a
/// principal it does not belong to, whatever the edge believed.
///
/// The same expression is inlined in two triggers in migration one (grants
/// and install authorities), because a trigger cannot call into the binary.
/// Three copies, on the record (D38 §2).
pub(crate) fn acting_kind(actor: &str, pk: &str, pid: &str) -> String {
    format!(
        "(CASE
            WHEN {actor} IS NULL OR {pk} IS NULL OR {pid} IS NULL THEN NULL
            WHEN (SELECT disabled_at FROM actors WHERE id = {actor}) IS NOT NULL THEN NULL
            WHEN (SELECT kind FROM actors WHERE id = {actor}) = 'ai' THEN
                 CASE WHEN (SELECT principal_kind FROM actors WHERE id = {actor}) IS {pk}
                       AND (SELECT principal_id FROM actors WHERE id = {actor}) IS {pid}
                      THEN 'ai' END
            WHEN (SELECT kind FROM actors WHERE id = {actor}) = 'human' THEN
                 CASE WHEN ({pk} = 'user' AND {pid} = {actor})
                        OR ({pk} = 'org' AND EXISTS (
                                SELECT 1 FROM org_members m
                                 WHERE m.org_id = {pid} AND m.user_id = {actor}))
                      THEN 'human' END
          END)"
    )
}

/// write implies read. call is orthogonal: it gates tools and routes, and a
/// reader of an install's data has no business invoking its tools.
fn access_satisfies(held: &str, required: &str) -> String {
    format!("({held} = {required} OR ({required} = 'read' AND {held} = 'write'))")
}

/// `SELECT reason, grant_id FROM (...)`: one row, always.
///
/// `reason` is NULL for deny; `grant_id` names the row that decided, which for
/// 'override' is what the auditing caller records so it cannot see a different
/// answer across a clock tick, and for 'install_grant' is the narrower of the
/// two grants ... the one a person can revoke to shut one app out without
/// touching their own access.
pub(crate) fn decision(a: &Args<'_>) -> String {
    let Args {
        subject_kind: sk,
        subject_id: sid,
        subject_name: sname,
        principal_kind: pk,
        principal_id: pid,
        actor_id: actor,
        access,
        now,
        acting_install: acting,
    } = a;
    let a_kind = acting_kind(actor, pk, pid);
    let satisfies = access_satisfies("g.access", access);
    let live = format!("g.revoked_at IS NULL AND (g.expires_at IS NULL OR g.expires_at > {now})");
    let same_subject =
        format!("g.subject_kind = {sk} AND g.subject_id = {sid} AND g.subject_name IS {sname}");
    format!(
        "SELECT
            CASE WHEN p_reason IS NULL THEN NULL
                 WHEN gated AND g4 IS NULL THEN NULL
                 WHEN gated THEN 'install_grant'
                 ELSE p_reason END AS reason,
            CASE WHEN p_reason IS NULL THEN NULL
                 WHEN gated AND g4 IS NULL THEN NULL
                 WHEN gated THEN g4
                 ELSE p_grant END AS grant_id
         FROM (
            SELECT
                -- 1. The principal owns the row. 2. A grant written against
                -- this principal; direct and inherited are the same row shape
                -- by construction (D18.3), so they are one branch. 3. A grant
                -- written against an org this principal belongs to, resolved
                -- at read time against membership and never materialized per
                -- member. 4. Admin override: a grant produced by policy, under
                -- D18.2's four constraints, reached only when nothing above
                -- fired. Absence of scope is deny: a subject nobody owns has
                -- no scope, and an incoherent credential has no standing.
                CASE WHEN a_kind IS NULL OR o_kind IS NULL THEN NULL
                     WHEN o_kind = {pk} AND o_id = {pid} THEN 'owner'
                     WHEN g1 IS NOT NULL THEN 'grant'
                     WHEN {pk} = 'user' AND g2 IS NOT NULL THEN 'org_grant'
                     WHEN a_kind = 'human' AND o_kind = 'org' AND g3 IS NOT NULL THEN 'override'
                END AS p_reason,
                CASE WHEN a_kind IS NULL OR o_kind IS NULL THEN NULL
                     WHEN o_kind = {pk} AND o_id = {pid} THEN NULL
                     WHEN g1 IS NOT NULL THEN g1
                     WHEN {pk} = 'user' AND g2 IS NOT NULL THEN g2
                     WHEN a_kind = 'human' AND o_kind = 'org' AND g3 IS NOT NULL THEN g3
                END AS p_grant,
                g4,
                -- D33. The principal is allowed; is the ASKING app allowed?
                -- Only for a collection reached through some other install.
                ({sk} = 'collection' AND {acting} IS NOT NULL AND {acting} <> {sid}) AS gated
            FROM (
                SELECT
                    {a_kind} AS a_kind,
                    so.owner_kind AS o_kind,
                    so.owner_id AS o_id,
                    (SELECT g.id FROM grants g
                      WHERE {same_subject}
                        AND g.target_kind = {pk} AND g.target_id = {pid}
                        AND g.source <> 'override'
                        AND {satisfies} AND {live}
                      LIMIT 1) AS g1,
                    (SELECT g.id FROM grants g
                       JOIN org_members m ON m.org_id = g.target_id AND m.user_id = {pid}
                      WHERE {same_subject}
                        AND g.target_kind = 'org'
                        AND g.source <> 'override'
                        AND {satisfies} AND {live}
                      LIMIT 1) AS g2,
                    (SELECT g.id FROM grants g
                       JOIN org_members m ON m.org_id = so.owner_id AND m.user_id = {actor}
                      WHERE {same_subject}
                        AND g.source = 'override'
                        AND m.role = 'admin'
                        AND g.target_kind = 'user' AND g.target_id = {actor}
                        AND {satisfies}
                        AND g.revoked_at IS NULL AND g.expires_at > {now}
                      LIMIT 1) AS g3,
                    (SELECT g.id FROM grants g
                      WHERE g.subject_kind = 'collection' AND g.subject_id = {sid}
                        AND g.subject_name IS {sname}
                        AND g.target_kind = 'install' AND g.target_install_id = {acting}
                        AND {satisfies} AND {live}
                      LIMIT 1) AS g4
                FROM (SELECT 1 AS one)
                LEFT JOIN subject_owners so
                       ON so.subject_kind = {sk} AND so.subject_id = {sid}
            )
         )"
    )
}

/// The single-value form, for composing into a WHERE clause: the reason, or
/// NULL. Same decision, same text, so a set read cannot drift from a point
/// check. A set read that uses this still owes the D18.2 audit for every row
/// whose reason is 'override'; `Guard` is the only thing that may call it and
/// discharges that obligation on every path.
pub(crate) fn reason(a: &Args<'_>) -> String {
    format!("(SELECT reason FROM ({}))", decision(a))
}

/// Whether a live, non-override, call-bearing grant on `kind` (tool or route)
/// exists for this principal on this install: the allowlist probe of D18.1.
/// An install grant with no allowlist implies the app's full set; with one,
/// exactly those names.
///
/// The probe is narrow on purpose. Anything looser flips the install onto
/// the allowlist path for rows that can never satisfy it: an override row is
/// read-only by CHECK, so a break-glass on one tool used to silently revoke
/// call access to every tool on that install, at the moment an admin needed it
/// most.
pub(crate) fn has_allowlist(kind: &str, install: &str, pk: &str, pid: &str, now: &str) -> String {
    format!(
        "SELECT EXISTS (
            SELECT 1 FROM grants g
             WHERE g.subject_kind = {kind}
               AND g.subject_id = {install}
               AND g.source <> 'override'
               AND g.access = 'call'
               AND g.revoked_at IS NULL
               AND (g.expires_at IS NULL OR g.expires_at > {now})
               AND (
                    (g.target_kind = {pk} AND g.target_id = {pid})
                 OR ({pk} = 'user' AND g.target_kind = 'org' AND EXISTS (
                        SELECT 1 FROM org_members m
                         WHERE m.org_id = g.target_id AND m.user_id = {pid}))
               ))"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The composed text has to parse; a set read embeds it as a scalar
    /// subquery over a column. Checked against the real schema so a renamed
    /// column fails here before any policy test can pass for the wrong
    /// reason.
    #[tokio::test]
    async fn the_predicate_parses_as_a_point_check_and_as_a_subquery() {
        let dir = tempfile::tempdir().unwrap();
        let db = hive_db::Db::open(dir.path().join("p.db")).await.unwrap();
        hive_schema::migrate(&db).await.unwrap();
        let c = db.conn().await.unwrap();
        let point = decision(&Args {
            subject_kind: "?1",
            subject_id: "?2",
            subject_name: "?3",
            principal_kind: "?4",
            principal_id: "?5",
            actor_id: "?6",
            access: "?7",
            now: "?8",
            acting_install: "?9",
        });
        let row = hive_db::query(&point)
            .bind("entity")
            .bind("nobody")
            .bind(None::<String>)
            .bind("user")
            .bind("nobody")
            .bind("nobody")
            .bind("read")
            .bind(0i64)
            .bind(None::<String>)
            .fetch_one(&c)
            .await
            .expect("point check parses");
        assert_eq!(row.get::<Option<String>>("reason"), None);
        let set = format!(
            "SELECT e.id FROM entities e WHERE {} IS NOT NULL",
            reason(&Args {
                subject_kind: "'entity'",
                subject_id: "e.id",
                subject_name: "NULL",
                principal_kind: "?1",
                principal_id: "?2",
                actor_id: "?3",
                access: "?4",
                now: "?5",
                acting_install: "NULL",
            })
        );
        let rows = hive_db::query(&set)
            .bind("user")
            .bind("nobody")
            .bind("nobody")
            .bind("read")
            .bind(0i64)
            .fetch_all(&c)
            .await
            .expect("set read parses");
        assert!(rows.is_empty());
        let probe = has_allowlist("'tool'", "?1", "?2", "?3", "?4");
        let has: bool = hive_db::query(&probe)
            .bind("nobody")
            .bind("user")
            .bind("nobody")
            .bind(0i64)
            .fetch_scalar(&c)
            .await
            .expect("allowlist probe parses");
        assert!(!has);
    }
}
