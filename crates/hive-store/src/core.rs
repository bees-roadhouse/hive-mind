//! The core install per owner (D32, #86).
//!
//! The core kinds, `entries`, `tasks`, `lists`, `contacts` and `decisions`,
//! live in an install of the built-in `core` app that every principal gets
//! when it is created. Nate's contacts and Maggie's are different owners, so
//! the key stays complete (invariant 14): `core/contacts` resolves against
//! the credential's owner, and there is no spelling of somebody else's.
//!
//! The install is made the way any other is, through `register_build`,
//! `stage_install` and `activate_install`, by a credential that acts for the
//! owner; nothing here bypasses the seam those three make.

use hive_db::{Connection, query};
use hive_identity::{Credential, Owner};
use hive_registry::{CORE_APP, core_manifest};
use uuid::Uuid;

use crate::builds::{BuildSpec, register_build};
use crate::installs::{InstallSpec, activate_install, stage_install};
use crate::{Result, StoreError};

/// The owner's core install, if it has one.
pub async fn core_install_id(conn: &Connection, owner: Owner) -> Result<Option<Uuid>> {
    query(
        "SELECT id FROM installs
          WHERE slug = ?1 AND owner_kind = ?2 AND owner_id = ?3 AND state <> 'uninstalling'
          ORDER BY created_at LIMIT 1",
    )
    .bind(CORE_APP)
    .bind(owner.kind.as_str())
    .bind(owner.id)
    .fetch_scalar_optional(conn)
    .await
    .map_err(|e| StoreError::db("look up core install", e))
}

/// Makes sure `owner` has an active core install and returns it. Idempotent:
/// an owner that has one gets it back. `by` must act for the owner and be
/// human, because activating is a person's act (D19.4) and the core app's
/// activation is no exception.
///
/// Runs in the caller's transaction when it has one; the build registration
/// provisions the five collection tables in the owner's file (D39), so the
/// whole thing commits or rolls back together.
pub async fn ensure_core_install(conn: &Connection, owner: Owner, by: &Credential) -> Result<Uuid> {
    if let Some(id) = core_install_id(conn, owner).await? {
        return Ok(id);
    }
    let m = core_manifest();
    let prepared = hive_registry::prepare(&m, &hive_wasmhost::Exports::none())
        .map_err(|e| StoreError::Other(format!("core manifest: {e}")))?;
    let spec = prepared
        .install_spec(owner.kind.as_str(), &owner.id.to_string())
        .map_err(|e| StoreError::Other(format!("core install spec: {e}")))?;
    let reg = register_build(
        conn,
        &BuildSpec {
            spec,
            owner: Some(owner),
            trust: "builtin".into(),
        },
        by,
    )
    .await?;
    let install = stage_install(
        conn,
        &InstallSpec {
            build_id: reg.build_id,
            slug: CORE_APP.into(),
            owner,
        },
        by,
    )
    .await?;
    activate_install(conn, install, by).await?;
    Ok(install)
}
