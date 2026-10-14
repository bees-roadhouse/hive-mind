//! The fixture the Go tree calls `world`, over the typed store API.
//!
//! `tests/invariants.rs` keeps its own raw-SQL copy on purpose: those tests
//! prove the MIGRATION holds with no Rust behaviour behind it. Everything
//! else goes through this one, so a test here exercises the same code a
//! daemon would.

#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use hive_db::{Conn, Db, query};
use hive_identity::{Credential, Owner, PrincipalKind};
use hive_store::{Access, BootstrapConfig, Guard, Reason, Store, StoreError, Subject};
use hive_testdb::TestDb;
use uuid::Uuid;

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A distinct 64-hex content hash per call, the way the Go fixtures mint them.
pub fn next_hash() -> String {
    format!(
        "{:064x}",
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
    )
}

pub fn cred(actor: Uuid, kind: PrincipalKind, principal: Uuid) -> Credential {
    Credential::new(actor, kind, principal)
}

pub fn user(id: Uuid) -> Owner {
    Owner::user(id)
}

pub fn org(id: Uuid) -> Owner {
    Owner::org(id)
}

/// The host clock in the schema's unit, for a test that writes a timestamp
/// column by hand.
pub fn now() -> DateTime<Utc> {
    hive_db::now()
}

/// `store` is declared before `db` so its clones of the files drop first and
/// the fixture can delete them.
pub struct World {
    pub store: Store,
    db: TestDb,
    pub root: Uuid,
}

impl World {
    /// Migrates a private file and bootstraps a root.
    pub async fn new(test: &str) -> World {
        let db = TestDb::new(test).await;
        let store = Store::from_dbs(db.db().clone(), db.audit().clone());
        let res = store
            .bootstrap_in_tx(&BootstrapConfig {
                root_handle: "root".into(),
                root_name: "Root".into(),
                ..Default::default()
            })
            .await
            .expect("bootstrap");
        World {
            store,
            db,
            root: res.root_actor_id,
        }
    }

    /// A migrated file with no root, for the bootstrap tests.
    pub async fn bare(test: &str) -> World {
        let db = TestDb::new(test).await;
        let store = Store::from_dbs(db.db().clone(), db.audit().clone());
        World {
            store,
            db,
            root: Uuid::nil(),
        }
    }

    pub fn db(&self) -> &Db {
        self.db.db()
    }

    pub fn guard(&self) -> Guard {
        self.store.guard()
    }

    pub async fn conn(&self) -> Conn {
        self.store.conn().await.expect("open connection")
    }

    /// A connection on the override audit's file.
    pub async fn audit(&self) -> Conn {
        self.store
            .audit()
            .conn()
            .await
            .expect("open audit connection")
    }

    /// A person. Every actor after the root names its creator.
    pub async fn human(&self, handle: &str) -> Uuid {
        let id = Uuid::new_v4();
        query(
            "INSERT INTO actors (id, kind, handle, display_name, principal_kind, principal_id, created_by_actor)
             VALUES (?1, 'human', ?2, ?2, 'user', ?1, ?3)",
        )
        .bind(id)
        .bind(handle)
        .bind(self.root)
        .execute(&*self.conn().await)
        .await
        .unwrap_or_else(|e| panic!("create human {handle}: {e}"));
        id
    }

    /// An org with `creator` as its first admin.
    pub async fn org(&self, handle: &str, creator: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        query(
            "INSERT INTO actors (id, kind, handle, display_name, principal_kind, principal_id, created_by_actor)
             VALUES (?1, 'org', ?2, ?2, 'org', ?1, ?3)",
        )
        .bind(id)
        .bind(handle)
        .bind(creator)
        .execute(&*self.conn().await)
        .await
        .unwrap_or_else(|e| panic!("create org {handle}: {e}"));
        self.member(id, creator, "admin", creator).await;
        id
    }

    pub async fn member(&self, org: Uuid, user: Uuid, role: &str, by: Uuid) {
        query(
            "INSERT INTO org_members (org_id, user_id, role, added_by_actor) VALUES (?1,?2,?3,?4)
             ON CONFLICT (org_id, user_id) DO UPDATE SET role = ?3",
        )
        .bind(org)
        .bind(user)
        .bind(role)
        .bind(by)
        .execute(&*self.conn().await)
        .await
        .unwrap_or_else(|e| panic!("add member: {e}"));
    }

    /// An AI persona instance owned by one principal (D13.9).
    pub async fn ai(&self, handle: &str, persona: &str, principal: Owner, creator: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        query(
            "INSERT INTO actors (id, kind, handle, display_name, persona, principal_kind, principal_id, created_by_actor)
             VALUES (?1, 'ai', ?2, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(id)
        .bind(handle)
        .bind(persona)
        .bind(principal.kind.as_str())
        .bind(principal.id)
        .bind(creator)
        .execute(&*self.conn().await)
        .await
        .unwrap_or_else(|e| panic!("create ai {handle}: {e}"));
        id
    }

    /// A minimal app build plus an ACTIVE install owned by `owner`.
    pub async fn install(&self, slug: &str, owner: Owner, by: Uuid) -> Uuid {
        let sum = next_hash();
        let c = self.conn().await;
        let build_id: Uuid = query(
            "INSERT INTO app_builds (id, slug, kind, impl, manifest, content_hash,
                                     author_actor, owner_kind, owner_id, visibility, trust, status)
             VALUES (?1, ?2, 'app', 'host', '{}', ?3, ?4, ?5, ?6, 'private', 'builtin', 'registered')
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(slug)
        .bind(&sum)
        .bind(by)
        .bind(owner.kind.as_str())
        .bind(owner.id)
        .fetch_scalar(&c)
        .await
        .unwrap_or_else(|e| panic!("create build: {e}"));
        query(
            "INSERT INTO installs (id, build_id, slug, owner_kind, owner_id, installed_by_actor,
                                   activated_by_actor, schema_name, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, 'active')
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(build_id)
        .bind(slug)
        .bind(owner.kind.as_str())
        .bind(owner.id)
        .bind(by)
        .bind(format!("app_{slug}_{}", &sum[..8]))
        .fetch_scalar(&c)
        .await
        .unwrap_or_else(|e| panic!("create install: {e}"))
    }

    /// One owned row.
    pub async fn entity(
        &self,
        install: Uuid,
        collection: &str,
        r#ref: &str,
        owner: Owner,
        author: Uuid,
    ) -> Uuid {
        query(
            "INSERT INTO entities (id, kind, install_id, collection, ref, owner_kind, owner_id, author_actor)
             VALUES (?1, 'entry', ?2, ?3, ?4, ?5, ?6, ?7)
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(install)
        .bind(collection)
        .bind(r#ref)
        .bind(owner.kind.as_str())
        .bind(owner.id)
        .bind(author)
        .fetch_scalar(&*self.conn().await)
        .await
        .unwrap_or_else(|e| panic!("create entity: {e}"))
    }

    /// `authorize` with `Denied` mapped to `None`, so a test can assert on the
    /// branch that fired without treating a denial as a failure. It is the
    /// auditing entry point on purpose: there is no other.
    pub async fn reason_of(
        &self,
        c: &Credential,
        subj: &Subject,
        access: Access,
    ) -> Option<Reason> {
        let conn = self.conn().await;
        match self.guard().authorize(&conn, c, subj, access, "").await {
            Ok(r) => Some(r),
            Err(StoreError::Denied) => None,
            Err(e) => panic!("authorize: {e}"),
        }
    }

    /// What the predicate will say at some offset from now, so a test can
    /// cross a break-glass window without sleeping and without mutating a
    /// grant, which the schema refuses. It binds the predicate's own clock
    /// argument; nothing in the daemon can, which is the point of the
    /// hidden accessor it uses.
    pub async fn reason_at(
        &self,
        c: &Credential,
        subj: &Subject,
        access: Access,
        offset: Duration,
    ) -> Option<Reason> {
        let at = now() + chrono::Duration::from_std(offset).unwrap_or_default();
        let row = query(&hive_store::__predicate_sql_for_tests(self.db().engine()))
            .bind(subj.kind.as_str())
            .bind(subj.id)
            .bind(subj.name.as_deref())
            .bind(c.principal_kind.as_str())
            .bind(c.principal_id)
            .bind(c.actor_id)
            .bind(access.as_str())
            .bind(at)
            .bind(None::<Uuid>)
            .fetch_one(&*self.conn().await)
            .await
            .unwrap_or_else(|e| panic!("access_decision at +{offset:?}: {e}"));
        let reason: Option<String> = row.get("reason");
        reason.as_deref().and_then(Reason::parse)
    }

    /// Disables the grant write policy so the READ predicate can be tested
    /// against rows a bug would have written. The file is deleted with the
    /// test, so nothing re-enables it.
    pub async fn issue_policy_off(&self) {
        // Postgres scopes a trigger to its table and wants the table named;
        // SQLite refuses the ON clause.
        let sql = match self.db().engine() {
            hive_db::Engine::Sqlite => "DROP TRIGGER grants_issue_policy",
            hive_db::Engine::Postgres => "DROP TRIGGER grants_issue_policy ON grants",
        };
        query(sql)
            .execute(&*self.conn().await)
            .await
            .expect("disable issue policy");
    }

    pub async fn count(&self, sql: &str) -> i64 {
        query(sql)
            .fetch_scalar(&*self.conn().await)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    /// Registers a build authored by `author` and stages a DISABLED install of
    /// it for `owner`. Both halves are the unprivileged ones.
    pub async fn stage_build(&self, slug: &str, author: Uuid, owner: Owner) -> Uuid {
        let conn = self.conn().await;
        let build_id: Uuid = query(
            "INSERT INTO app_builds (id, slug, kind, impl, manifest, content_hash,
                                     author_actor, owner_kind, owner_id, visibility, trust, status)
             VALUES (?1, ?2, 'tool', 'host', '{}', ?3, ?4, ?5, ?6, 'private', 'local', 'registered')
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(slug)
        .bind(next_hash())
        .bind(author)
        .bind(owner.kind.as_str())
        .bind(owner.id)
        .fetch_scalar(&conn)
        .await
        .unwrap_or_else(|e| panic!("register build: {e}"));
        hive_store::stage_install(
            &conn,
            &hive_store::InstallSpec {
                build_id,
                slug: slug.into(),
                owner,
            },
            &cred(author, owner.kind, owner.id),
        )
        .await
        .unwrap_or_else(|e| panic!("stage install: {e}"))
    }

    pub async fn install_state(&self, install_id: Uuid) -> String {
        query("SELECT state FROM installs WHERE id = ?1")
            .bind(install_id)
            .fetch_scalar(&*self.conn().await)
            .await
            .expect("read install state")
    }
}

// --- the catalogue, read the way each engine keeps it -----------------------
//
// A collection's table lives in the owner's file as `<schema>__<collection>`
// on SQLite and in the install's own schema as `<collection>` on Postgres
// (hive-store's `appschema` module doc says why). These helpers answer the
// questions the suites ask about what provisioning left behind, in terms of
// collection names, so a test reads the same on either engine and the engine
// difference lives here once. `alias` is what `attach_owner` returned on the
// connection; Postgres ignores it, as the store does.

/// The collections provisioned under `schema`, by collection name, sorted.
pub async fn collection_tables(
    conn: &hive_db::Connection,
    alias: &str,
    schema: &str,
) -> Vec<String> {
    match conn.engine() {
        hive_db::Engine::Sqlite => {
            let prefix = format!("{schema}__");
            let names: Vec<String> = query(&format!(
                "SELECT name FROM {}.sqlite_master
                  WHERE type = 'table' AND substr(name, 1, length(?1)) = ?1 ORDER BY name",
                hive_db::quote_ident(alias)
            ))
            .bind(&prefix)
            .fetch_scalars(conn)
            .await
            .unwrap();
            // The virtual tables an index provisions sit under the same
            // prefix (`<schema>__<collection>_<n>_fts`); only the collection
            // tables themselves are wanted here, and those carry no digit
            // suffix after a second `_`.
            names
                .into_iter()
                .filter(|n| !is_virtual_name(n))
                .map(|n| n[prefix.len()..].to_string())
                .collect()
        }
        hive_db::Engine::Postgres => query(
            "SELECT table_name::text FROM information_schema.tables
              WHERE table_schema = ?1 AND table_type = 'BASE TABLE' ORDER BY table_name",
        )
        .bind(schema)
        .fetch_scalars(conn)
        .await
        .unwrap(),
    }
}

/// `<schema>__<collection>_<n>_fts` or `_vec`: a virtual table's name on
/// SQLite, told apart from a collection's by its last two segments.
fn is_virtual_name(name: &str) -> bool {
    let mut parts = name.rsplitn(3, '_');
    matches!(
        (parts.next(), parts.next()),
        (Some("fts") | Some("vec"), Some(n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
    )
}

/// Whether anything was provisioned for `schema`: at least one table in the
/// owner's file under its prefix, or the schema itself on Postgres.
pub async fn schema_provisioned(conn: &hive_db::Connection, alias: &str, schema: &str) -> bool {
    match conn.engine() {
        hive_db::Engine::Sqlite => !collection_tables(conn, alias, schema).await.is_empty(),
        hive_db::Engine::Postgres => {
            let n: i64 =
                query("SELECT count(*) FROM information_schema.schemata WHERE schema_name = ?1")
                    .bind(schema)
                    .fetch_scalar(conn)
                    .await
                    .unwrap();
            n > 0
        }
    }
}

pub async fn collection_exists(
    conn: &hive_db::Connection,
    alias: &str,
    schema: &str,
    collection: &str,
) -> bool {
    collection_tables(conn, alias, schema)
        .await
        .iter()
        .any(|c| c == collection)
}

/// Whether the collection's table has a column named `col`.
pub async fn collection_column_exists(
    conn: &hive_db::Connection,
    alias: &str,
    schema: &str,
    collection: &str,
    col: &str,
) -> bool {
    let n: i64 = match conn.engine() {
        hive_db::Engine::Sqlite => {
            query("SELECT count(*) FROM pragma_table_info(?1, ?2) WHERE name = ?3")
                .bind(format!("{schema}__{collection}"))
                .bind(alias)
                .bind(col)
                .fetch_scalar(conn)
                .await
                .unwrap()
        }
        hive_db::Engine::Postgres => query(
            "SELECT count(*) FROM information_schema.columns
              WHERE table_schema = ?1 AND table_name = ?2 AND column_name = ?3",
        )
        .bind(schema)
        .bind(collection)
        .bind(col)
        .fetch_scalar(conn)
        .await
        .unwrap(),
    };
    n > 0
}

/// The names of everything an index provisions for a collection: the indexes
/// on its table (the one behind the primary key included) and the triggers on
/// it, plus the virtual tables beside it on SQLite or the `fts_<n>` and
/// `vec_<n>` columns on it on Postgres (D41, D43 §3). Sorted.
pub async fn collection_index_names(
    conn: &hive_db::Connection,
    alias: &str,
    schema: &str,
    collection: &str,
) -> Vec<String> {
    match conn.engine() {
        hive_db::Engine::Sqlite => {
            let base = format!("{schema}__{collection}");
            query(&format!(
                "SELECT name FROM {}.sqlite_master
                  WHERE (type IN ('index', 'trigger') AND tbl_name = ?1)
                     OR (type IN ('table', 'trigger') AND name GLOB ?2)
                  ORDER BY name",
                hive_db::quote_ident(alias)
            ))
            .bind(&base)
            .bind(format!("{base}_[0-9]*_[fv]*"))
            .fetch_scalars(conn)
            .await
            .unwrap()
        }
        hive_db::Engine::Postgres => {
            let mut names: Vec<String> = query(
                "SELECT indexname::text FROM pg_indexes WHERE schemaname = ?1 AND tablename = ?2",
            )
            .bind(schema)
            .bind(collection)
            .fetch_scalars(conn)
            .await
            .unwrap();
            let triggers: Vec<String> = query(
                "SELECT t.tgname::text FROM pg_trigger t
                   JOIN pg_class c ON c.oid = t.tgrelid
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = ?1 AND c.relname = ?2 AND NOT t.tgisinternal",
            )
            .bind(schema)
            .bind(collection)
            .fetch_scalars(conn)
            .await
            .unwrap();
            let columns: Vec<String> = query(
                "SELECT column_name::text FROM information_schema.columns
                  WHERE table_schema = ?1 AND table_name = ?2
                    AND (column_name LIKE 'fts\\_%' OR column_name LIKE 'vec\\_%')",
            )
            .bind(schema)
            .bind(collection)
            .fetch_scalars(conn)
            .await
            .unwrap();
            names.extend(triggers);
            names.extend(columns);
            names.sort();
            names
        }
    }
}

/// Whether a platform table (one migration one made, not a collection's)
/// exists on this connection's default schema.
pub async fn platform_table_exists(conn: &hive_db::Connection, name: &str) -> bool {
    let n: i64 = match conn.engine() {
        hive_db::Engine::Sqlite => {
            query("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
                .bind(name)
                .fetch_scalar(conn)
                .await
                .unwrap()
        }
        hive_db::Engine::Postgres => query(
            "SELECT count(*) FROM information_schema.tables
              WHERE table_schema = current_schema() AND table_name = ?1",
        )
        .bind(name)
        .fetch_scalar(conn)
        .await
        .unwrap(),
    };
    n > 0
}

/// The text of a named CHECK constraint on a platform table, as the engine
/// holds it: cut from the table's `CREATE` text on SQLite, from the
/// constraint's own definition on Postgres.
pub async fn check_constraint_text(
    conn: &hive_db::Connection,
    table: &str,
    constraint: &str,
) -> String {
    match conn.engine() {
        hive_db::Engine::Sqlite => {
            let ddl: String =
                query("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1")
                    .bind(table)
                    .fetch_scalar(conn)
                    .await
                    .expect("read table definition");
            ddl.split(constraint)
                .nth(1)
                .and_then(|rest| rest.split("CONSTRAINT").next())
                .unwrap_or_default()
                .to_string()
        }
        hive_db::Engine::Postgres => query(
            "SELECT pg_get_constraintdef(c.oid) FROM pg_constraint c
               JOIN pg_class t ON t.oid = c.conrelid
               JOIN pg_namespace n ON n.oid = t.relnamespace
              WHERE n.nspname = current_schema() AND t.relname = ?1 AND c.conname = ?2",
        )
        .bind(table)
        .bind(constraint)
        .fetch_scalar(conn)
        .await
        .expect("read constraint definition"),
    }
}
