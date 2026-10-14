//! The install credential (D53 §1, #163 slice 2): what a process app's
//! container holds to reach the host.
//!
//! Three facts carry the design, and each is a row rather than a rule in a
//! handler:
//!
//! - **An install has an AI-shaped actor of its own** (`installs.actor_id`),
//!   created when the install is staged by the human who staged it, acting
//!   for the install's owner. The database's own triggers decide who may
//!   create it (D19.2: the owning person, or an admin of the owning org,
//!   and never an AI) and who may issue for it (D19.3). Nothing here talks
//!   the database out of either.
//! - **The actor is the install's, not the app's.** Two installs of one app
//!   are two actors, so a credential carries the install dimension by
//!   construction: the predicate can learn which install is asking from
//!   `actor_id` alone, through [`install_of_actor`], with no new field on
//!   [`Credential`]. Invariant 14's eighth instance was a predicate never
//!   told which install was asking; this is the answer that cannot be
//!   omitted, because an app has no credential that is not an install's.
//! - **A credential is minted when the container starts and revoked when it
//!   stops**, keyed on the install (`credentials.install_id`), never found by
//!   its label. It carries no expiry of its own: the container's life is its
//!   life, and a daemon that comes back after a crash revokes every install
//!   credential first ([`revoke_all_install_credentials`]), because every
//!   container it was supervising died with it.
//!
//! What is deliberately not here: no new resolve path. `resolve_credential`
//! answers an install's token the way it answers a person's, with the
//! install's actor and the owner as principal; every call the app makes is a
//! call the client or an agent could make (D53 §1), through the same
//! `Guard`.

use hive_db::{Connection, query};
use hive_identity::{Credential, Owner, PrincipalKind};
use uuid::Uuid;

use crate::credentials::issue_credential;
use crate::{Result, StoreError};

/// The label every install credential carries. Prose for the person reading
/// the credentials list; `install_id` is the key.
pub const INSTALL_CREDENTIAL_LABEL: &str = "install";

/// The persona an install's actor carries: `app:<slug>`. One persona per app,
/// many instances (one per install), which is D13.9's shape for AI identities.
pub fn install_persona(slug: &str) -> String {
    format!("app:{slug}")
}

/// Creates the install's actor if it has none and returns its id. Idempotent.
///
/// `by` is the human staging the install. The `actors_creation_check`
/// trigger refuses a creator who is not the owning person or an admin of the
/// owning org, and refuses an AI creator outright; the refusal comes back as
/// the database's message, which names D19.2.
pub async fn ensure_install_actor(
    conn: &Connection,
    install_id: Uuid,
    by: &Credential,
) -> Result<Uuid> {
    let row = query("SELECT slug, owner_kind, owner_id, actor_id FROM installs WHERE id = ?1")
        .bind(install_id)
        .fetch_optional(conn)
        .await
        .map_err(|e| StoreError::db("look up install", e))?
        .ok_or(StoreError::NoRows)?;
    if let Some(existing) = row.get::<Option<Uuid>>("actor_id") {
        return Ok(existing);
    }
    let slug: String = row.get("slug");
    let owner_kind: String = row.get("owner_kind");
    let owner_id: Uuid = row.get("owner_id");
    let actor = Uuid::new_v4();
    // The handle carries the install id, not only the slug: handles are
    // unique and two installs of one app need two.
    let handle = format!("app:{slug}:{}", &actor.to_string()[..8]);
    query(
        "INSERT INTO actors (id, kind, handle, display_name, persona, principal_kind, principal_id, created_by_actor)
         VALUES (?1, 'ai', ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(actor)
    .bind(&handle)
    .bind(format!("{slug} (app)"))
    .bind(install_persona(&slug))
    .bind(&owner_kind)
    .bind(owner_id)
    .bind(by.actor_id)
    .execute(conn)
    .await
    .map_err(|e| StoreError::db("create install actor", e))?;
    let n = query("UPDATE installs SET actor_id = ?2 WHERE id = ?1 AND actor_id IS NULL")
        .bind(install_id)
        .bind(actor)
        .execute(conn)
        .await
        .map_err(|e| StoreError::db("bind install actor", e))?;
    if n != 1 {
        // A concurrent staging won; the actor just created is unreferenced.
        // Rare, harmless (an unreferenced AI actor can do nothing: it has no
        // credential and never will), and the caller gets the one that won.
        let winner: Uuid = query("SELECT actor_id FROM installs WHERE id = ?1")
            .bind(install_id)
            .fetch_scalar(conn)
            .await
            .map_err(|e| StoreError::db("re-read install actor", e))?;
        return Ok(winner);
    }
    Ok(actor)
}

/// Mints the credential a container starts with: the install's actor acting
/// for the install's owner, issued in the name of the human who installed the
/// app (the database refuses the issue when that person may no longer act
/// for the owner, which is the right answer: the authority that stood the app
/// up is gone). Returns the bearer token, the only moment it exists in
/// plaintext, and the row id.
///
/// No expiry: the container's life is the credential's. See the module doc
/// for what happens after a crash.
pub async fn issue_install_credential(
    conn: &Connection,
    install_id: Uuid,
) -> Result<(String, Uuid)> {
    let row = query(
        "SELECT i.actor_id, i.owner_kind, i.owner_id, i.installed_by_actor, i.state
           FROM installs i WHERE i.id = ?1",
    )
    .bind(install_id)
    .fetch_optional(conn)
    .await
    .map_err(|e| StoreError::db("look up install", e))?
    .ok_or(StoreError::NoRows)?;
    let state: String = row.get("state");
    if state != "active" {
        return Err(StoreError::Other(format!(
            "install {install_id} is {state}; only an active install gets a credential"
        )));
    }
    let actor: Uuid = row.get::<Option<Uuid>>("actor_id").ok_or_else(|| {
        StoreError::Other(format!(
            "install {install_id} has no actor; it is not a process app or was staged before one"
        ))
    })?;
    let owner_kind: String = row.get("owner_kind");
    let owner = Owner::new(
        PrincipalKind::parse(&owner_kind)
            .ok_or_else(|| StoreError::Other(format!("owner kind {owner_kind:?}")))?,
        row.get("owner_id"),
    );
    let installer: Uuid = row.get("installed_by_actor");
    // The issuer is the installer acting for the owner. The trigger decides
    // whether that pair may still issue; this function does not.
    let by = Credential::new(installer, owner.kind, owner.id);
    let (token, id) =
        issue_credential(conn, actor, owner, &by, INSTALL_CREDENTIAL_LABEL, None).await?;
    query("UPDATE credentials SET install_id = ?2 WHERE id = ?1")
        .bind(id)
        .bind(install_id)
        .execute(conn)
        .await
        .map_err(|e| StoreError::db("key credential on install", e))?;
    Ok((token, id))
}

/// Revokes every live credential of one install: the container stopped.
/// Returns how many.
pub async fn revoke_install_credentials(conn: &Connection, install_id: Uuid) -> Result<u64> {
    query("UPDATE credentials SET revoked_at = ?2 WHERE install_id = ?1 AND revoked_at IS NULL")
        .bind(install_id)
        .bind(hive_db::now())
        .execute(conn)
        .await
        .map_err(|e| StoreError::db("revoke install credentials", e))
}

/// Revokes every live install credential there is: the daemon is starting
/// and every container it supervised died with it. A person's credentials
/// are not install credentials and are untouched. Returns how many.
pub async fn revoke_all_install_credentials(conn: &Connection) -> Result<u64> {
    query("UPDATE credentials SET revoked_at = ?1 WHERE install_id IS NOT NULL AND revoked_at IS NULL")
        .bind(hive_db::now())
        .execute(conn)
        .await
        .map_err(|e| StoreError::db("revoke all install credentials", e))
}

/// The install an actor belongs to, if the actor is an install's. This is how
/// a request carrying an install credential tells the predicate which install
/// is asking (invariant 14): from the credential's actor, never from a value
/// the caller supplied (invariant 11).
pub async fn install_of_actor(conn: &Connection, actor_id: Uuid) -> Result<Option<Uuid>> {
    query("SELECT id FROM installs WHERE actor_id = ?1")
        .bind(actor_id)
        .fetch_scalar_optional(conn)
        .await
        .map_err(|e| StoreError::db("look up install of actor", e))
}
