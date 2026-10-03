use chrono::{DateTime, Utc};
use hive_db::{Connection, query};
use hive_identity::{Credential, Owner, PrincipalKind};
use uuid::Uuid;

use crate::grants::{Access, Subject, write_install_grant};
use crate::predicate;
use crate::{Result, StoreError};

/// What an install authority permits, unattended: rolling a new build into an
/// install a human already stood up.
pub const CAPABILITY_ACTIVATE: &str = "activate";

/// Stages an app for an owner. The install lands DISABLED: D19.4 separates
/// building from making live, and staging is the unprivileged half that the
/// builder loop may do unattended.
///
/// There is deliberately no schema name here. It used to be a parameter, and
/// nothing checked it against the owner: Bob, acting honestly for his own
/// principal, could stage an install he owns that points at Alice's tables, and
/// the data layer reads the prefix off the install row, so every read and write
/// through it lands in her tables. The fix is invariant 11 in its plainest form:
/// stop accepting as an argument the fact you are deciding. `stage_install`
/// derives the name from the slug and the owner it has already authorised.
#[derive(Clone, Debug)]
pub struct InstallSpec {
    pub build_id: Uuid,
    pub slug: String,
    pub owner: Owner,
}

/// Whether the credential is genuinely the given principal: the pair agrees
/// (the predicate's acting-kind rule) and the principal is the one named.
///
/// This is the check that "is the actor a human" is not. Being a person says
/// nothing about whose app you are touching.
async fn acts_for(conn: &Connection, by: &Credential, principal: Owner) -> Result<bool> {
    if by.principal_kind != principal.kind || by.principal_id != principal.id {
        return Ok(false);
    }
    let kind: Option<String> = query(&format!(
        "SELECT {}",
        predicate::acting_kind("?1", "?2", "?3")
    ))
    .bind(by.actor_id)
    .bind(by.principal_kind.as_str())
    .bind(by.principal_id)
    .fetch_scalar(conn)
    .await
    .map_err(|e| StoreError::db("acting_kind", e))?;
    Ok(kind.is_some())
}

/// One row of [`installs_of`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallSummary {
    pub id: Uuid,
    pub slug: String,
    pub state: String,
    pub schema_name: String,
}

/// Every install an owner has, in every state, oldest first. A listing for
/// the owner's own eyes; the candidate set a caller may reach is the
/// predicate's (`hive-surfaces`), not this.
pub async fn installs_of(conn: &Connection, owner: Owner) -> Result<Vec<InstallSummary>> {
    let rows = query(
        "SELECT id, slug, state, schema_name FROM installs
          WHERE owner_kind = ?1 AND owner_id = ?2
          ORDER BY created_at, id",
    )
    .bind(owner.kind.as_str())
    .bind(owner.id)
    .fetch_all(conn)
    .await
    .map_err(|e| StoreError::db("list installs", e))?;
    Ok(rows
        .iter()
        .map(|r| InstallSummary {
            id: r.get("id"),
            slug: r.get("slug"),
            state: r.get("state"),
            schema_name: r.get("schema_name"),
        })
        .collect())
}

/// Records an install without turning it on. Any actor that may act for the
/// owning principal may do this, including an AI.
pub async fn stage_install(conn: &Connection, spec: &InstallSpec, by: &Credential) -> Result<Uuid> {
    if spec.slug.is_empty() {
        return Err(StoreError::Other("install needs a slug".into()));
    }
    // Invariant 14, one input further back than the schema-name fix below.
    //
    // schema_name appends the owner digest LAST, so a long enough slug pushes
    // it past the bound the manifest keeps identifiers under. Two owners
    // would then derive names that differ only past the point anything reads,
    // and schema_name UNIQUE would never see it. Every caller today arrives
    // through prepare, where validate enforces the same bound ... which is
    // what makes this latent rather than live, and is also exactly the
    // reasoning that would leave it here until the caller that does not exist
    // yet appears.
    if spec.slug.len() > hive_manifest::MAX_APP_NAME {
        return Err(StoreError::Other(format!(
            "install slug is {} characters, over the {} that leave room for the owner suffix in a {}-character identifier",
            spec.slug.len(),
            hive_manifest::MAX_APP_NAME,
            hive_manifest::MAX_IDENTIFIER
        )));
    }
    if !acts_for(conn, by, spec.owner).await? {
        // Staging an install writes into a principal's own scope.
        return Err(StoreError::Denied);
    }
    // Derived AFTER the ownership check and from the same values it
    // authorised, so the prefix on the row is the one this owner is entitled
    // to whatever the caller believed. It is the same derivation
    // register_build used to provision, which is what makes the install point
    // at tables that exist.
    let schema_name = hive_manifest::schema_name(
        &spec.slug,
        spec.owner.kind.as_str(),
        &spec.owner.id.to_string(),
    );
    query(
        "INSERT INTO installs (id, build_id, slug, owner_kind, owner_id, installed_by_actor, schema_name, state, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,'disabled',?8)
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(spec.build_id)
    .bind(&spec.slug)
    .bind(spec.owner.kind.as_str())
    .bind(spec.owner.id)
    .bind(by.actor_id)
    .bind(schema_name)
    .bind(hive_db::now())
    .fetch_scalar(conn)
    .await
    .map_err(|e| StoreError::db("stage install", e))
}

/// Delegates unattended activation on ONE install (D19.4, D20).
///
/// This is deliberately not a grant. An authority is a write-path capability
/// and confers no visibility: it lives in its own table and the predicate
/// never looks at it. Modelling it as a `write` grant on the install subject
/// meant that delegating "may roll a rebuilt tool into this app" also handed the
/// delegate general write on the install through the ordinary predicate, which
/// is one table carrying two meanings.
///
/// The database refuses this unless the granting actor is human and acts for
/// the install's owning principal.
pub async fn grant_install_authority(
    db: &Connection,
    install_id: Uuid,
    holder: Owner,
    capability: &str,
    by: &Credential,
    reason: &str,
    expires: Option<DateTime<Utc>>,
) -> Result<Uuid> {
    query(
        "INSERT INTO install_authorities (
             id, install_id, holder_kind, holder_id, capability,
             granted_by_actor, granted_by_principal_kind, granted_by_principal_id,
             reason, expires_at, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(install_id)
    .bind(holder.kind.as_str())
    .bind(holder.id)
    .bind(capability)
    .bind(by.actor_id)
    .bind(by.principal_kind.as_str())
    .bind(by.principal_id)
    .bind(reason)
    .bind(expires)
    .bind(hive_db::now())
    .fetch_scalar(db)
    .await
    .map_err(|e| StoreError::db("grant install authority", e))
}

/// Withdraws one. Tombstoned rather than deleted, because the record that it
/// was ever delegated is worth keeping.
pub async fn revoke_install_authority(db: &Connection, id: Uuid, by: Uuid) -> Result<()> {
    let n = query(
        "UPDATE install_authorities SET revoked_at = ?3, revoked_by = ?2 WHERE id = ?1 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(by)
    .bind(hive_db::now())
    .execute(db)
    .await
    .map_err(|e| StoreError::db("revoke install authority", e))?;
    if n == 0 {
        return Err(StoreError::NoRows);
    }
    Ok(())
}

/// Makes a staged install live (D19.4).
///
/// This function exists because the schema CANNOT enforce the rule on its own,
/// and the schema looks like it can. `installs_activation_check` checks that
/// activated_by_actor names a human, but a trigger has no credential in scope,
/// so an AI could register a build and activate it by naming any human in that
/// column. The missing binding is exactly one line: the activator is the actor
/// ON THE CREDENTIAL, not a value the writer chose.
///
/// Do not set installs.state directly. That is the whole point of this function.
pub async fn activate_install(conn: &Connection, install_id: Uuid, by: &Credential) -> Result<()> {
    let kind: Option<String> = query("SELECT kind FROM actors WHERE id = ?1")
        .bind(by.actor_id)
        .fetch_scalar_optional(conn)
        .await
        .map_err(|e| StoreError::db("look up activating actor", e))?;
    let kind = kind.ok_or(StoreError::Denied)?;

    let row = query(
        "SELECT i.owner_kind, i.owner_id, i.state, b.status
           FROM installs i
           JOIN app_builds b ON b.id = i.build_id
          WHERE i.id = ?1",
    )
    .bind(install_id)
    .fetch_optional(conn)
    .await
    .map_err(|e| StoreError::db("look up install", e))?
    .ok_or(StoreError::NoRows)?;
    let owner_kind: String = row.get("owner_kind");
    let owner = Owner::new(
        PrincipalKind::parse(&owner_kind)
            .ok_or_else(|| StoreError::Other(format!("owner kind {owner_kind:?}")))?,
        row.get("owner_id"),
    );
    let install_state: String = row.get("state");
    let build_status: String = row.get("status");

    // The direct route is the OWNER acting in person. Being human is one of the
    // two conditions, not the only one: without the ownership test any human
    // could promote anybody's build into anybody's app, which is exactly what
    // the trigger's kind='human' check looks like it prevents and does not.
    //
    // An AI acting for the owner falls through to the standing route, which is
    // the whole point of D19.4.
    let is_owner = acts_for(conn, by, owner).await?;
    let uses = declared_uses(conn, install_id).await?;
    if kind == "human" && is_owner {
        // The grants BEFORE the flip, in the same unit of work: an install
        // that is active with its declaration unmet is the shape #86 exists
        // to prevent, and a use that resolves to nothing refuses the whole
        // activation rather than activating an app whose every cross-app
        // read would then deny with nothing to say why.
        derive_use_grants(conn, install_id, owner, &uses, by).await?;
        return activate(
            conn,
            install_id,
            &install_state,
            &build_status,
            Some(by.actor_id),
            None,
        )
        .await;
    }
    // The standing-authority route cannot write what the app asks for: an
    // install grant is a share across apps on the owner's behalf, and only
    // the owning principal in person may make one (D13.14, the trigger).
    // Activating anyway would leave the declaration meaning nothing.
    if !uses.is_empty() {
        return Err(StoreError::NotHuman(format!(
            "this app uses {} other app collection(s); activating it needs the owning principal in person, because those grants are theirs to make (D19, D13.14)",
            uses.len()
        )));
    }

    // Otherwise: a standing authority a human delegated on this specific
    // install, held by the principal this actor is acting for. This is the
    // route for an AI, and for a human delegate who does not own the app.
    let authority: Option<Uuid> = query(
        "SELECT ia.id
           FROM install_authorities ia
          WHERE ia.install_id = ?1
            AND ia.capability = 'activate'
            AND ia.revoked_at IS NULL
            AND (ia.expires_at IS NULL OR ia.expires_at > ?4)
            AND (
                 (ia.holder_kind = ?2 AND ia.holder_id = ?3)
              OR (?2 = 'user' AND ia.holder_kind = 'org' AND EXISTS (
                     SELECT 1 FROM org_members m
                      WHERE m.org_id = ia.holder_id AND m.user_id = ?3))
            )
          LIMIT 1",
    )
    .bind(install_id)
    .bind(by.principal_kind.as_str())
    .bind(by.principal_id)
    .bind(hive_db::now())
    .fetch_scalar_optional(conn)
    .await
    .map_err(|e| StoreError::db("look up install authority", e))?;
    let authority = authority.ok_or_else(|| {
        StoreError::NotHuman(
            "activating an install needs the owning principal in person, or a human-delegated activate authority on this install (D19.4)".into(),
        )
    })?;
    activate(
        conn,
        install_id,
        &install_state,
        &build_status,
        None,
        Some(authority),
    )
    .await
}

/// What the install's manifest declares it uses, read back off the build
/// row rather than carried in by the caller (invariant 11).
async fn declared_uses(conn: &Connection, install_id: Uuid) -> Result<Vec<hive_manifest::Use>> {
    let raw: Option<String> = query(
        "SELECT coalesce(json_extract(b.manifest, '$.storage.uses'), '[]')
           FROM installs i JOIN app_builds b ON b.id = i.build_id
          WHERE i.id = ?1",
    )
    .bind(install_id)
    .fetch_scalar_optional(conn)
    .await
    .map_err(|e| StoreError::db("read declared uses", e))?;
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw).map_err(|e| {
        StoreError::Other(format!(
            "install {install_id}: manifest uses do not parse: {e}"
        ))
    })
}

/// Writes the collection grants the manifest's `uses` ask for, each against
/// the OWNER's install of the used app, resolved by (slug, owner): a slug
/// alone is not a key (invariant 14). `core` is the owner's core install.
async fn derive_use_grants(
    conn: &Connection,
    install_id: Uuid,
    owner: Owner,
    uses: &[hive_manifest::Use],
    by: &Credential,
) -> Result<()> {
    for u in uses {
        let used: Option<Uuid> = query(
            "SELECT id FROM installs
              WHERE slug = ?1 AND owner_kind = ?2 AND owner_id = ?3 AND state = 'active'
              ORDER BY created_at LIMIT 1",
        )
        .bind(&u.app)
        .bind(owner.kind.as_str())
        .bind(owner.id)
        .fetch_scalar_optional(conn)
        .await
        .map_err(|e| StoreError::db("resolve used app", e))?;
        let used = used.ok_or_else(|| {
            StoreError::Other(format!(
                "this app uses {}/{} and {} {} has no active install of {:?}; install that first",
                u.app,
                u.collection,
                owner.kind.as_str(),
                owner.id,
                u.app
            ))
        })?;
        let access = match u.access {
            hive_manifest::UseAccess::Read => Access::Read,
            hive_manifest::UseAccess::Write => Access::Write,
        };
        write_install_grant(
            conn,
            &Subject::collection(used, &u.collection),
            install_id,
            access,
            by,
            "derived from the manifest's uses at activation",
        )
        .await?;
    }
    Ok(())
}

/// The second half of the promotion seam: what is being promoted.
///
/// The authority logic above decides WHO may promote, and it was the only
/// question the seam ever asked. Nothing looked at what they were promoting
/// into, so a build with status='withdrawn' activated cleanly, and the
/// install's own state was never read, so 'uninstalling' could be pulled back to
/// 'active' mid-teardown. Deliberately called only AFTER authority is
/// established: a caller with no standing must not learn from the error message
/// whether somebody else's build was withdrawn.
async fn activate(
    conn: &Connection,
    install_id: Uuid,
    install_state: &str,
    build_status: &str,
    activated_by: Option<Uuid>,
    authority: Option<Uuid>,
) -> Result<()> {
    if build_status != "registered" {
        // The build behind the install is not promotable at all (D25).
        return Err(StoreError::Denied);
    }
    if install_state == "uninstalling" {
        // Activating it would pull a teardown back to live.
        return Err(StoreError::Denied);
    }
    // The same two conditions again, in the UPDATE. Not belt and braces: the
    // reads above happened at some earlier instant, and a withdrawal committing
    // in between would otherwise be activated straight over.
    let n = query(
        "UPDATE installs
            SET state = 'active', activated_by_actor = ?2, activation_authority_id = ?3
          WHERE id = ?1
            AND state <> 'uninstalling'
            AND EXISTS (SELECT 1 FROM app_builds b
                         WHERE b.id = installs.build_id AND b.status = 'registered')",
    )
    .bind(install_id)
    .bind(activated_by)
    .bind(authority)
    .execute(conn)
    .await
    .map_err(|e| StoreError::db("activate install", e))?;
    if n == 0 {
        // Not NoRows. The install existed a moment ago and the checks above
        // passed, so this is a concurrent withdrawal or teardown rather than a
        // caller naming something that is not there.
        return Err(StoreError::Denied);
    }
    Ok(())
}
