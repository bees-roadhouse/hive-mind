use hive_db::{Connection, query};
use hive_identity::{Credential, Owner, PrincipalKind};
use uuid::Uuid;

use crate::core::ensure_core_install;
use crate::{Result, StoreError};

/// Read from config or environment at first boot, never from a request. D19.1:
/// a system whose first authorization can be requested over the network does
/// not have a root.
#[derive(Clone, Debug, Default)]
pub struct BootstrapConfig {
    /// The first human actor.
    pub root_handle: String,
    pub root_name: String,
    /// The first org, created by the root, who becomes its first admin. Leave
    /// empty to create only the root.
    pub org_handle: String,
    pub org_name: String,
}

/// What bootstrap found or created.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BootstrapResult {
    pub root_actor_id: Uuid,
    pub org_actor_id: Option<Uuid>,
    pub created: bool,
}

/// Seeds the root actor and its org out of band (D19.1).
///
/// This function must never be reachable from an HTTP handler, an MCP tool, a
/// workflow step or a guest. It takes no credential precisely because there is
/// nobody to authenticate yet: it is the one write in the platform that is not
/// authorized. Exactly one caller exists, in the daemon, reading config and
/// environment at startup.
///
/// Two caps, and only the first is enforced by the schema:
///
/// - The ROOT is capped at one row by `actors_single_root`, so a second root is
///   refused by the database rather than by trusting this code.
/// - The ORG is capped HERE. Nothing in the schema stops a caller creating org
///   after org by passing the existing root handle with a new org handle, so
///   this refuses any org beyond the one it seeded.
///
/// Run it in a transaction. The three writes have to be atomic: a failure
/// between the org insert and the admin seat used to leave an org with no
/// members that a later call skipped over and never repaired.
pub async fn bootstrap(conn: &Connection, cfg: &BootstrapConfig) -> Result<BootstrapResult> {
    let mut res = BootstrapResult::default();
    if cfg.root_handle.is_empty() {
        return Err(StoreError::Other("bootstrap needs a root handle".into()));
    }

    let existing = query("SELECT id, handle FROM actors WHERE created_by_actor IS NULL")
        .fetch_optional(conn)
        .await
        .map_err(|e| StoreError::db("look up root actor", e))?;
    match existing {
        Some(row) => {
            let handle: String = row.get("handle");
            if handle != cfg.root_handle {
                return Err(StoreError::AlreadyBootstrapped(format!(
                    "as {handle:?}, config names {:?}",
                    cfg.root_handle
                )));
            }
            res.root_actor_id = row.get("id");
        }
        None => {
            // A human actor is its own principal, and the CHECK is immediate, so
            // the id is chosen here rather than by the column default.
            let root_id = Uuid::new_v4();
            query(
                "INSERT INTO actors (id, kind, handle, display_name, principal_kind, principal_id, created_by_actor)
                 VALUES (?1, 'human', ?2, ?3, 'user', ?1, NULL)",
            )
            .bind(root_id)
            .bind(&cfg.root_handle)
            .bind(&cfg.root_name)
            .execute(conn)
            .await
            .map_err(|e| StoreError::db("create root actor", e))?;
            res.root_actor_id = root_id;
            res.created = true;
        }
    }

    // The root's core install (D32): every principal gets one when it is
    // created, and the root is created here. Idempotent, so a second boot
    // finds it rather than making another.
    let as_root = Credential::new(res.root_actor_id, PrincipalKind::User, res.root_actor_id);
    ensure_core_install(conn, Owner::user(res.root_actor_id), &as_root).await?;

    if cfg.org_handle.is_empty() {
        return Ok(res);
    }

    // The org this bootstrap seeded, if any. Asking "did I make one" rather
    // than "does this handle exist" is what caps it: the second form lets a
    // caller create one org per call, forever, with no credential.
    let existing = query(
        "SELECT id, handle FROM actors
          WHERE kind = 'org' AND created_by_actor = ?1
          ORDER BY created_at LIMIT 1",
    )
    .bind(res.root_actor_id)
    .fetch_optional(conn)
    .await
    .map_err(|e| StoreError::db("look up root org", e))?;
    if let Some(row) = existing {
        let handle: String = row.get("handle");
        if handle != cfg.org_handle {
            return Err(StoreError::AlreadyBootstrapped(format!(
                "with org {handle:?}, config names {:?}",
                cfg.org_handle
            )));
        }
        let org_id: Uuid = row.get("id");
        res.org_actor_id = Some(org_id);
        ensure_core_install(
            conn,
            Owner::new(PrincipalKind::Org, org_id),
            &Credential::new(res.root_actor_id, PrincipalKind::Org, org_id),
        )
        .await?;
        return Ok(res);
    }

    let org_id = Uuid::new_v4();
    query(
        "INSERT INTO actors (id, kind, handle, display_name, principal_kind, principal_id, created_by_actor)
         VALUES (?1, 'org', ?2, ?3, 'org', ?1, ?4)",
    )
    .bind(org_id)
    .bind(&cfg.org_handle)
    .bind(&cfg.org_name)
    .bind(res.root_actor_id)
    .execute(conn)
    .await
    .map_err(|e| StoreError::db("create root org", e))?;
    query("INSERT INTO org_members (org_id, user_id, role, added_by_actor) VALUES (?1, ?2, 'admin', ?2)")
        .bind(org_id)
        .bind(res.root_actor_id)
        .execute(conn)
        .await
        .map_err(|e| StoreError::db("seat root as org admin", e))?;
    res.org_actor_id = Some(org_id);
    res.created = true;
    // The org's core install, activated by the root acting as its admin.
    ensure_core_install(
        conn,
        Owner::new(PrincipalKind::Org, org_id),
        &Credential::new(res.root_actor_id, PrincipalKind::Org, org_id),
    )
    .await?;
    Ok(res)
}

impl crate::Store {
    /// Runs [`bootstrap`] atomically. This is the form the daemon uses.
    pub async fn bootstrap_in_tx(&self, cfg: &BootstrapConfig) -> Result<BootstrapResult> {
        let tx = self.begin().await?;
        let res = bootstrap(&tx, cfg).await?;
        crate::commit(tx, "bootstrap").await?;
        Ok(res)
    }
}
