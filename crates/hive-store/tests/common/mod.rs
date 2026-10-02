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
        self.store.audit().conn().await.expect("open audit connection")
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
        let row = query(&hive_store::__predicate_sql_for_tests())
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
        query("DROP TRIGGER grants_issue_policy")
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
