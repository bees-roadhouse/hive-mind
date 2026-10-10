//! An owner's file, and the one way it is reached (D39).
//!
//! The control plane (`hive.db`) keeps every row the predicate and the
//! triggers read. What an install stores for its owner, the collection
//! tables, lives in a file per owner beside it, under
//! `<control plane stem>-owners/<kind>-<uuid>.db`, and a caller reaches
//! those tables by attaching that file on the connection it already holds
//! and addressing every table through the alias this module returns.
//!
//! The resolver takes an owner and nothing else (invariant 14): never an
//! install, never an app, never the request's claim. Every caller gets the
//! owner off the `installs` row its target resolved to.
//!
//! Three facts measured before this was written, in D39:
//!
//! - a file attaches inside `BEGIN IMMEDIATE`, so the document and its
//!   `entities` row commit together;
//! - a file cannot be switched to WAL from inside a transaction, so a new
//!   file is initialised on a connection of its own, once per process;
//! - a connection returning to the pool with an attachment would be keyed on
//!   something the next caller did not ask for, so `hive-db` detaches on the
//!   way back and closes the connection if it cannot.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use hive_db::{Connection, Db, query, quote_ident};
use hive_identity::{Owner, PrincipalKind};

use crate::{Result, StoreError};

/// The suffix on the control plane file's stem that names the directory its
/// owner files live in: `hive.db` keeps them in `hive-owners/`.
pub const OWNERS_DIR_SUFFIX: &str = "-owners";

/// Where the owner files of the control plane at `control_plane` live.
/// Derived from that file's path rather than configured, so an owner file
/// cannot belong to a different control plane than the one beside it.
pub fn owners_dir(control_plane: &Path) -> PathBuf {
    let stem = control_plane
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hive".into());
    control_plane
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join(format!("{stem}{OWNERS_DIR_SUFFIX}"))
}

/// The file for `owner` under `dir`: `<kind>-<uuid>.db`.
pub fn owner_file(dir: &Path, owner: Owner) -> PathBuf {
    dir.join(format!("{}-{}.db", owner.kind.as_str(), owner.id))
}

/// The alias an owner's file is attached under: `o_<kind>_<uuid, no
/// hyphens>`. Deterministic, so one owner is one attachment per connection
/// however many times it is asked for; an identifier, so it never needs
/// quoting to be safe and is quoted anyway.
pub fn owner_alias(owner: Owner) -> String {
    format!("o_{}_{}", owner.kind.as_str(), owner.id.simple())
}

/// The quoted name of a collection table in an owner's file, for a caller
/// that holds the alias `attach_owner` returned.
pub fn owner_table(alias: &str, table: &str) -> String {
    format!("{}.{}", quote_ident(alias), quote_ident(table))
}

/// Attaches `owner`'s file on `conn` and returns the alias to address it by.
///
/// The file is created and initialised on first use in this process; the
/// attach runs inside whatever transaction `conn` has open, so DDL and rows
/// written through the alias commit or roll back with the control plane's.
/// The file's own marker is checked through the alias every time, and a
/// file that says it belongs to somebody else is refused.
pub async fn attach_owner(conn: &Connection, owner: Owner) -> Result<String> {
    let alias = owner_alias(owner);
    if conn.attached().iter().any(|a| a == &alias) {
        return Ok(alias);
    }
    // An owner file is a SQLite file beside the control plane file. Under
    // Postgres there is no file to be beside (D43 §5: the org's database is
    // the tenant), and D43's phase 6 removes this layer; until then the
    // refusal is explicit rather than a path that does not exist.
    let Some(control_plane) = conn.path() else {
        return Err(StoreError::Other(format!(
            "owner files exist only on the sqlite engine; this connection is {}",
            conn.engine()
        )));
    };
    let path = owner_file(&owners_dir(control_plane), owner);
    initialise(&path, owner).await?;
    conn.attach(&path, &alias)
        .map_err(|e| StoreError::db(format!("attach {}", path.display()), e))?;
    if let Err(e) = verify_marker(conn, &alias, owner, &path).await {
        // A refused file does not stay reachable under the alias it was
        // refused for. If the detach itself fails (a transaction already
        // holds the file), the connection is closed on its way back rather
        // than pooled, which ends the attachment the other way.
        let _ = conn.detach(&alias);
        return Err(e);
    }
    Ok(alias)
}

/// The paths this process has already initialised. Keyed on the path, which
/// carries both the control plane and the owner.
fn initialised() -> &'static Mutex<HashSet<PathBuf>> {
    static SET: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Creates the file if it is new, puts it in WAL, applies the owner file's
/// migrations and writes the marker. Once per path per process; a second
/// process doing the same work at the same time serialises on the file's
/// write lock inside `migrate_owner` and finds the marker already there.
async fn initialise(path: &Path, owner: Owner) -> Result<()> {
    if initialised().lock().expect("owner init set").contains(path) {
        return Ok(());
    }
    let db = Db::open(path)
        .await
        .map_err(|e| StoreError::db(format!("open owner file {}", path.display()), e))?;
    hive_schema::migrate_owner(&db).await?;
    let c = db
        .conn()
        .await
        .map_err(|e| StoreError::db("owner file connect", e))?;
    query("INSERT OR IGNORE INTO owner_file (lock, kind, id, created_at) VALUES (1, ?1, ?2, ?3)")
        .bind(owner.kind.as_str())
        .bind(owner.id)
        .bind(hive_db::now())
        .execute(&c)
        .await
        .map_err(|e| StoreError::db("write owner marker", e))?;
    drop(c);
    db.close();
    initialised()
        .lock()
        .expect("owner init set")
        .insert(path.to_path_buf());
    Ok(())
}

async fn verify_marker(conn: &Connection, alias: &str, owner: Owner, path: &Path) -> Result<()> {
    let row = query(&format!(
        "SELECT kind, id FROM {}.owner_file WHERE lock = 1",
        quote_ident(alias)
    ))
    .fetch_optional(conn)
    .await
    .map_err(|e| StoreError::db(format!("read owner marker of {}", path.display()), e))?;
    let Some(row) = row else {
        return Err(StoreError::Other(format!(
            "owner file {} carries no marker; it was not created by this store",
            path.display()
        )));
    };
    let kind: String = row.get("kind");
    let id: uuid::Uuid = row.get("id");
    let found = PrincipalKind::parse(&kind).map(|k| Owner::new(k, id));
    if found != Some(owner) {
        return Err(StoreError::Other(format!(
            "owner file {} belongs to {kind} {id}, not to {} {}; refusing to attach it",
            path.display(),
            owner.kind.as_str(),
            owner.id
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn the_directory_is_keyed_on_the_control_plane_file() {
        let dir = owners_dir(Path::new("/var/lib/hive/data/hive.db"));
        assert_eq!(dir, PathBuf::from("/var/lib/hive/data/hive-owners"));
        let dir = owners_dir(Path::new("/tmp/t_thing_1234.db"));
        assert_eq!(dir, PathBuf::from("/tmp/t_thing_1234-owners"));
    }

    #[test]
    fn the_file_and_the_alias_carry_kind_and_id() {
        let id = Uuid::parse_str("4b0d6c9e-4a8c-4e4a-9d2f-0a2c6a1b7e55").unwrap();
        let owner = Owner::new(PrincipalKind::User, id);
        assert_eq!(
            owner_file(Path::new("x"), owner),
            PathBuf::from("x").join("user-4b0d6c9e-4a8c-4e4a-9d2f-0a2c6a1b7e55.db")
        );
        assert_eq!(
            owner_alias(owner),
            "o_user_4b0d6c9e4a8c4e4a9d2f0a2c6a1b7e55"
        );
        assert_ne!(
            owner_alias(owner),
            owner_alias(Owner::new(PrincipalKind::Org, id)),
            "one id as a user and as an org are two files"
        );
        assert_eq!(
            owner_table("o_user_ab", "app_x__entries"),
            "\"o_user_ab\".\"app_x__entries\""
        );
    }
}
