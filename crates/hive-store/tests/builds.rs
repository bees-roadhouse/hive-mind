//! Registering builds, the promotion view and provisioning app tables. Ported
//! from builds_test.go, promotion_test.go and appschema_test.go.

mod common;

use common::{World, cred, next_hash, user};
use hive_db::query;
use hive_identity::{Owner, PrincipalKind};
use hive_manifest::{
    Collection, CollectionPlan, Index, IndexMethod, Kind, Manifest, SchemaPlan, Storage,
};
use hive_store::{BuildSpec, StoreError, apply_schema_plan, drop_schema_plan, register_build};
use uuid::Uuid;

fn short_slug() -> String {
    format!("t{}", &Uuid::new_v4().to_string()[..8])
}

/// A real Prepared for one owner: one generated collection and no functions,
/// installable from a manifest with no wasm at all.
fn prepared_for(name: &str, owner: Owner) -> hive_registry::InstallSpec {
    let m = Manifest {
        kind: Some(Kind::App),
        name: name.into(),
        version: 1,
        storage: Storage {
            collections: vec![Collection {
                name: "links".into(),
                crud: true,
                indexes: vec!["btree(created)".into()],
            }],
            uses: vec![],
        },
        ..Default::default()
    };
    let p = hive_registry::prepare(&m, &hive_wasmhost::Exports::none()).expect("prepare");
    p.install_spec(owner.kind.as_str(), &owner.id.to_string())
        .expect("install_spec")
}

fn build_spec(spec: hive_registry::InstallSpec, owner: Owner) -> BuildSpec {
    BuildSpec {
        spec,
        owner: Some(owner),
        trust: String::new(),
    }
}

async fn register_in(
    w: &World,
    spec: &BuildSpec,
    by: &hive_identity::Credential,
) -> Result<hive_store::RegisteredBuild, StoreError> {
    let tx = w.store.begin().await.expect("begin");
    match register_build(&tx, spec, by).await {
        Ok(o) => {
            tx.commit().await.expect("commit");
            Ok(o)
        }
        Err(e) => Err(e),
    }
}

/// Whether any table under the install's prefix exists: the engine has no
/// schemas, so "the schema exists" means "its tables do".
async fn schema_exists(w: &World, schema: &str) -> bool {
    let n: i64 = query(
        "SELECT count(*) FROM sqlite_master
          WHERE type = 'table' AND substr(name, 1, length(?1)) = ?1",
    )
    .bind(format!("{schema}__"))
    .fetch_scalar(&*w.conn().await)
    .await
    .unwrap();
    n > 0
}

async fn table_exists(w: &World, schema: &str, table: &str) -> bool {
    let n: i64 = query("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
        .bind(format!("{schema}__{table}"))
        .fetch_scalar(&*w.conn().await)
        .await
        .unwrap();
    n > 0
}

/// Drops the collection tables a test provisioned. The file goes with the
/// test anyway; this keeps the uninstall path exercised.
async fn drop_plan(w: &World, plan: &SchemaPlan) {
    let tx = w.store.begin().await.unwrap();
    drop_schema_plan(&tx, plan).await.unwrap();
    tx.commit().await.unwrap();
}

/// Ported from `TestRegisterBuildWritesTheRowAndProvisionsTheSchema`.
#[tokio::test]
async fn register_build_writes_the_row_and_provisions_the_schema() {
    let w = World::new("register_build_writes_row").await;
    let alice = w.human("alice").await;
    let spec = prepared_for(&short_slug(), user(alice));
    let plan = spec.schema.clone();
    let out = register_in(
        &w,
        &BuildSpec {
            trust: "builtin".into(),
            ..build_spec(spec.clone(), user(alice))
        },
        &cred(alice, PrincipalKind::User, alice),
    )
    .await
    .expect("register_build");
    assert!(
        table_exists(&w, &out.schema_name, "links").await,
        "{}.links was not provisioned",
        out.schema_name
    );
    let row = query("SELECT surface_hash, derive_version FROM app_builds WHERE id = ?1")
        .bind(out.build_id)
        .fetch_one(&*w.conn().await)
        .await
        .unwrap();
    let (surface_hash, derive_version): (Option<String>, Option<i32>) =
        (row.get("surface_hash"), row.get("derive_version"));
    assert_eq!(surface_hash.as_deref(), Some(spec.surface_hash.as_str()));
    assert_eq!(derive_version, Some(hive_manifest::DERIVE_VERSION));
    drop_plan(&w, &plan).await;
}

/// Ported from `TestRegisterBuildCreatesNoInstall` (D19.4).
#[tokio::test]
async fn register_build_creates_no_install() {
    let w = World::new("register_build_no_install").await;
    let alice = w.human("alice").await;
    let slug = short_slug();
    let spec = prepared_for(&slug, user(alice));
    let plan = spec.schema.clone();
    register_in(
        &w,
        &build_spec(spec, user(alice)),
        &cred(alice, PrincipalKind::User, alice),
    )
    .await
    .expect("register");
    let installs: i64 = query("SELECT count(*) FROM installs WHERE slug = ?1")
        .bind(&slug)
        .fetch_scalar(&*w.conn().await)
        .await
        .unwrap();
    assert_eq!(installs, 0, "registering a build is not making it live");
    drop_plan(&w, &plan).await;
}

/// Ported from `TestTwoOwnersGetSeparateSchemas`: the bug that made the schema
/// name per-install rather than per-app.
#[tokio::test]
async fn two_owners_get_separate_schemas() {
    let w = World::new("two_owners_separate_schemas").await;
    let alice = w.human("alice").await;
    let bob = w.human("bob").await;
    let slug = short_slug();
    let a_spec = prepared_for(&slug, user(alice));
    let b_spec = prepared_for(&slug, user(bob));
    let (a_plan, b_plan) = (a_spec.schema.clone(), b_spec.schema.clone());
    let a = register_in(
        &w,
        &build_spec(a_spec, user(alice)),
        &cred(alice, PrincipalKind::User, alice),
    )
    .await
    .expect("alice");
    let b = register_in(
        &w,
        &build_spec(b_spec, user(bob)),
        &cred(bob, PrincipalKind::User, bob),
    )
    .await
    .expect("bob");
    assert_ne!(
        a.schema_name, b.schema_name,
        "one app, one schema, two people's documents"
    );
    assert_ne!(a.build_id, b.build_id, "two owners share one build row");
    for s in [&a.schema_name, &b.schema_name] {
        assert!(schema_exists(&w, s).await, "{s} was not created");
    }
    drop_plan(&w, &a_plan).await;
    drop_plan(&w, &b_plan).await;
}

/// Ported from `TestReRegisteringLandsOnTheSameSchema`.
#[tokio::test]
async fn re_registering_lands_on_the_same_schema() {
    let w = World::new("re_registering_same_schema").await;
    let alice = w.human("alice").await;
    let slug = short_slug();
    let by = cred(alice, PrincipalKind::User, alice);
    let spec = prepared_for(&slug, user(alice));
    let plan = spec.schema.clone();
    let first = register_in(&w, &build_spec(spec.clone(), user(alice)), &by)
        .await
        .expect("first");
    let second = register_in(&w, &build_spec(spec, user(alice)), &by)
        .await
        .expect("second");
    assert_eq!(first.schema_name, second.schema_name);
    assert_eq!(
        first.build_id, second.build_id,
        "identical registrations produced two build rows"
    );
    drop_plan(&w, &plan).await;
}

/// Ported from `TestFailedRegistrationLeavesNothing`.
#[tokio::test]
async fn failed_registration_leaves_nothing() {
    let w = World::new("failed_registration_leaves_nothing").await;
    let alice = w.human("alice").await;
    let slug = short_slug();
    let spec = prepared_for(&slug, user(alice));
    let schema = spec.schema.schema.clone();
    let tx = w.store.begin().await.unwrap();
    register_build(
        &tx,
        &build_spec(spec, user(alice)),
        &cred(alice, PrincipalKind::User, alice),
    )
    .await
    .expect("register inside tx");
    tx.rollback().await.unwrap(); // something later failed
    assert!(
        !schema_exists(&w, &schema).await,
        "{schema} survived a failed registration"
    );
    let builds: i64 = query("SELECT count(*) FROM app_builds WHERE slug = ?1")
        .bind(&slug)
        .fetch_scalar(&*w.conn().await)
        .await
        .unwrap();
    assert_eq!(builds, 0);
}

/// Ported from `TestRegisterBuildRefusesAnIncompleteIdentity`.
#[tokio::test]
async fn register_build_refuses_an_incomplete_identity() {
    let w = World::new("register_build_incomplete_identity").await;
    let alice = w.human("alice").await;
    let spec = prepared_for(&short_slug(), user(alice));
    let cases = [
        (
            "no credential",
            build_spec(spec.clone(), user(alice)),
            hive_identity::Credential::new(Uuid::nil(), PrincipalKind::User, Uuid::nil()),
        ),
        (
            "no owner",
            BuildSpec {
                spec: spec.clone(),
                owner: None,
                trust: String::new(),
            },
            cred(alice, PrincipalKind::User, alice),
        ),
    ];
    for (name, build, by) in cases {
        let tx = w.store.begin().await.unwrap();
        assert!(
            register_build(&tx, &build, &by).await.is_err(),
            "{name}: an incomplete identity was accepted"
        );
        tx.rollback().await.unwrap();
    }
}

// --- the promotion view (D25) -----------------------------------------------

const HASH_A: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const HASH_B: &str = "2222222222222222222222222222222222222222222222222222222222222222";

struct BuildRow {
    capabilities: Vec<&'static str>,
    surface_hash: Option<&'static str>,
    derive_version: Option<i32>,
}

struct PromotionRow {
    capability_change: Option<bool>,
    capabilities_gained: Vec<String>,
    surface_change: Option<bool>,
}

async fn build_row(w: &World, slug: &str, owner: Owner, by: Uuid, spec: &BuildRow) -> Uuid {
    let manifest = serde_json::json!({"capabilities": spec.capabilities});
    query(
        "INSERT INTO app_builds (slug, kind, impl, manifest, content_hash,
                                 author_actor, owner_kind, owner_id, visibility, trust, status,
                                 surface_hash, derive_version)
         VALUES (?1, 'app', 'host', ?2, ?3, ?4, ?5, ?6, 'private', 'builtin', 'registered', ?7, ?8)
         RETURNING id",
    )
    .bind(slug)
    .bind(&manifest)
    .bind(next_hash())
    .bind(by)
    .bind(owner.kind.as_str())
    .bind(owner.id)
    .bind(spec.surface_hash)
    .bind(spec.surface_hash.and(spec.derive_version))
    .fetch_scalar(&*w.conn().await)
    .await
    .expect("create build")
}

async fn promote(w: &World, slug: &str, owner: Owner, by: Uuid, build_id: Uuid) {
    query(
        "INSERT INTO installs (build_id, slug, owner_kind, owner_id, installed_by_actor, activated_by_actor, schema_name, state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, 'active')",
    )
    .bind(build_id)
    .bind(slug)
    .bind(owner.kind.as_str())
    .bind(owner.id)
    .bind(by)
    .bind(format!("app_{slug}_{}", &next_hash()[..8]))
    .execute(&*w.conn().await)
    .await
    .expect("promote");
}

async fn promotion_row(w: &World, build_id: Uuid) -> PromotionRow {
    let row = query(
        "SELECT capability_change, coalesce(capabilities_gained, '[]') AS gained, surface_change
           FROM builds_awaiting_promotion WHERE build_id = ?1",
    )
    .bind(build_id)
    .fetch_one(&*w.conn().await)
    .await
    .expect("read view");
    let (capability_change, gained, surface_change): (
        Option<bool>,
        serde_json::Value,
        Option<bool>,
    ) = (
        row.get("capability_change"),
        row.get("gained"),
        row.get("surface_change"),
    );
    PromotionRow {
        capability_change,
        capabilities_gained: serde_json::from_value(gained).unwrap(),
        surface_change,
    }
}

async fn two_builds(test: &str, live: BuildRow, candidate: BuildRow) -> (World, PromotionRow) {
    let w = World::new(test).await;
    let alice = w.human("alice").await;
    let slug = format!("app{}", &Uuid::new_v4().to_string()[..8]);
    let live_id = build_row(&w, &slug, user(alice), alice, &live).await;
    let cand_id = build_row(&w, &slug, user(alice), alice, &candidate).await;
    promote(&w, &slug, user(alice), alice, live_id).await;
    let row = promotion_row(&w, cand_id).await;
    (w, row)
}

/// Ported from `TestPromotionViewFlagsACapabilityGain`.
#[tokio::test]
async fn promotion_view_flags_a_capability_gain() {
    let (_w, row) = two_builds(
        "promotion_flags_capability_gain",
        BuildRow {
            capabilities: vec!["log"],
            surface_hash: Some(HASH_A),
            derive_version: Some(1),
        },
        BuildRow {
            capabilities: vec!["log", "egress"],
            surface_hash: Some(HASH_A),
            derive_version: Some(1),
        },
    )
    .await;
    assert_eq!(
        row.capability_change,
        Some(true),
        "an app gaining egress must be flagged"
    );
    assert_eq!(row.capabilities_gained, vec!["egress"]);
}

/// Ported from `TestPromotionViewIgnoresCapabilityOrder`.
#[tokio::test]
async fn promotion_view_ignores_capability_order() {
    let (_w, row) = two_builds(
        "promotion_ignores_order",
        BuildRow {
            capabilities: vec!["log", "storage", "kv"],
            surface_hash: Some(HASH_A),
            derive_version: Some(1),
        },
        BuildRow {
            capabilities: vec!["kv", "log", "storage"],
            surface_hash: Some(HASH_A),
            derive_version: Some(1),
        },
    )
    .await;
    assert_eq!(row.capability_change, Some(false), "only the order changed");
}

/// Ported from `TestPromotionViewFlagsASurfaceChange`.
#[tokio::test]
async fn promotion_view_flags_a_surface_change() {
    let (_w, row) = two_builds(
        "promotion_flags_surface_change",
        BuildRow {
            capabilities: vec!["log"],
            surface_hash: Some(HASH_A),
            derive_version: Some(1),
        },
        BuildRow {
            capabilities: vec!["log"],
            surface_hash: Some(HASH_B),
            derive_version: Some(1),
        },
    )
    .await;
    assert_eq!(row.surface_change, Some(true));
}

/// Ported from `TestPromotionViewRefusesToCompareAcrossDerivers`: the null
/// case, and the reason derive_version exists at all.
#[tokio::test]
async fn promotion_view_refuses_to_compare_across_derivers() {
    let (_w, row) = two_builds(
        "promotion_refuses_across_derivers",
        BuildRow {
            capabilities: vec!["log"],
            surface_hash: Some(HASH_A),
            derive_version: Some(1),
        },
        BuildRow {
            capabilities: vec!["log"],
            surface_hash: Some(HASH_B),
            derive_version: Some(2),
        },
    )
    .await;
    assert_eq!(
        row.surface_change, None,
        "hashes from different derivers are not comparable"
    );
    assert!(
        row.capability_change.is_some(),
        "capabilities do not depend on the deriver"
    );
}

/// Ported from `TestPromotionViewIsNullWithoutARecordedSurface`.
#[tokio::test]
async fn promotion_view_is_null_without_a_recorded_surface() {
    let (_w, row) = two_builds(
        "promotion_null_without_surface",
        BuildRow {
            capabilities: vec!["log"],
            surface_hash: Some(HASH_A),
            derive_version: Some(1),
        },
        BuildRow {
            capabilities: vec!["log"],
            surface_hash: None,
            derive_version: None,
        },
    )
    .await;
    assert_eq!(row.surface_change, None);
}

/// Ported from `TestPromotionViewIsNullForAFirstInstall`.
#[tokio::test]
async fn promotion_view_is_null_for_a_first_install() {
    let w = World::new("promotion_null_first_install").await;
    let alice = w.human("alice").await;
    let slug = format!("app{}", &Uuid::new_v4().to_string()[..8]);
    let cand = build_row(
        &w,
        &slug,
        user(alice),
        alice,
        &BuildRow {
            capabilities: vec!["log", "egress"],
            surface_hash: Some(HASH_A),
            derive_version: Some(1),
        },
    )
    .await;
    let row = promotion_row(&w, cand).await;
    assert_eq!(row.capability_change, None);
    assert_eq!(row.surface_change, None);
}

/// Ported from `TestSurfaceHashAndDeriverAreBothOrNeither`.
#[tokio::test]
async fn surface_hash_and_deriver_are_both_or_neither() {
    let w = World::new("surface_hash_both_or_neither").await;
    let alice = w.human("alice").await;
    assert!(
        query(
            "INSERT INTO app_builds (slug, kind, impl, manifest, content_hash,
                                     author_actor, owner_kind, owner_id, visibility, trust, status,
                                     surface_hash, derive_version)
             VALUES ('halfrecorded', 'app', 'host', '{}', ?1, ?2, 'user', ?2, 'private', 'builtin', 'registered', ?3, NULL)",
        )
        .bind(next_hash())
        .bind(alice)
        .bind(HASH_A)
        .execute(&*w.conn().await)
        .await
        .is_err(),
        "a surface hash with no deriver was accepted"
    );
}

// --- apply_schema_plan -------------------------------------------------------
//
// An install's collections are tables under its prefix in the one file. Each
// test gets a unique app name and drops its own tables.

fn unique_app() -> String {
    format!("t_{}", &Uuid::new_v4().simple().to_string()[..12])
}

fn plan_for(app: &str, collections: Vec<Collection>) -> SchemaPlan {
    let m = Manifest {
        kind: Some(Kind::App),
        name: app.into(),
        version: 1,
        storage: Storage {
            collections,
            uses: vec![],
        },
        functions: vec![hive_manifest::Function {
            name: "noop".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    m.validate().expect("validate");
    m.schema_plan("user", app).expect("schema_plan")
}

fn coll(name: &str, indexes: &[&str]) -> Collection {
    Collection {
        name: name.into(),
        crud: false,
        indexes: indexes.iter().map(|s| s.to_string()).collect(),
    }
}

/// Applies in its own transaction and commits.
async fn apply(w: &World, plan: &SchemaPlan) -> Result<(), StoreError> {
    let tx = w.store.begin().await.unwrap();
    apply_schema_plan(&tx, plan).await?;
    tx.commit()
        .await
        .map_err(|e| StoreError::Other(e.to_string()))
}

async fn column_exists(w: &World, schema: &str, table: &str, col: &str) -> bool {
    let n: i64 = query("SELECT count(*) FROM pragma_table_info(?1) WHERE name = ?2")
        .bind(format!("{schema}__{table}"))
        .bind(col)
        .fetch_scalar(&*w.conn().await)
        .await
        .unwrap();
    n > 0
}

/// The names of every index on a collection table, the autoindex behind the
/// primary key included.
async fn index_names(w: &World, schema: &str, table: &str) -> Vec<String> {
    query("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 ORDER BY name")
        .bind(format!("{schema}__{table}"))
        .fetch_scalars(&*w.conn().await)
        .await
        .unwrap()
}

/// Ported from `TestApplySchemaPlanProvisionsCollections`.
#[tokio::test]
async fn apply_schema_plan_provisions_collections() {
    let w = World::bare("apply_schema_plan_provisions").await;
    let plan = plan_for(
        &unique_app(),
        vec![
            coll("entries", &["btree(entry_date)", "gin(tags)", "fts(body)"]),
            Collection {
                name: "drafts".into(),
                crud: true,
                indexes: vec![],
            },
        ],
    );
    apply(&w, &plan).await.expect("apply_schema_plan");
    for table in ["entries", "drafts"] {
        assert!(
            table_exists(&w, &plan.schema, table).await,
            "{}.{table} was not created",
            plan.schema
        );
    }
    for col in [
        "id",
        "doc",
        "trust",
        "tainted_by",
        "created_at",
        "updated_at",
    ] {
        assert!(
            column_exists(&w, &plan.schema, "entries", col).await,
            "entries is missing {col}"
        );
    }
    // Ownership is NOT here: it lives on the entities row alone.
    for col in ["owner_kind", "owner_id", "author_actor"] {
        assert!(
            !column_exists(&w, &plan.schema, "entries", col).await,
            "entries carries {col}"
        );
    }
    let indexes = index_names(&w, &plan.schema, "entries").await.len();
    assert!(
        indexes >= 4,
        "entries has {indexes} indexes, want at least 4"
    );
    drop_plan(&w, &plan).await;
}

/// Ported from `TestLongCollectionNameStillGetsItsIndexes`: Postgres truncated
/// an over-long identifier rather than rejecting it, and IF NOT EXISTS turned
/// the resulting collision into a NOTICE nobody surfaced. The engine here
/// does not truncate; the test stays because the bound is still the
/// manifest's and a name at it still has to get its index.
#[tokio::test]
async fn long_collection_name_still_gets_its_indexes() {
    let w = World::bare("long_collection_name_indexes").await;
    let long = format!("c{}", "x".repeat(hive_manifest::MAX_COLLECTION_NAME - 1));
    let plan = plan_for(&unique_app(), vec![coll(&long, &["btree(entry_date)"])]);
    apply(&w, &plan).await.expect("apply");
    let names = index_names(&w, &plan.schema, &long).await;
    assert!(
        names.len() >= 2,
        "collection {long:?} has indexes {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("btree")),
        "the declared index is missing on {long:?}: {names:?}"
    );
    drop_plan(&w, &plan).await;
}

/// Ported from `TestDerivedIndexNameIsRefusedRatherThanTruncated`. The engine
/// does not truncate, so the hazard that test guarded is gone; what remains
/// is the check at the point of use, that a name past the manifest's bound
/// is refused here whatever an earlier layer did.
#[tokio::test]
async fn derived_index_name_is_refused_rather_than_truncated() {
    let w = World::bare("derived_index_name_refused").await;
    let plan = SchemaPlan {
        schema: format!("app_{}", unique_app()),
        collections: vec![CollectionPlan {
            name: format!("c{}", "x".repeat(63)),
            crud: false,
            indexes: vec![],
        }],
    };
    let tx = w.store.begin().await.unwrap();
    let err = apply_schema_plan(&tx, &plan)
        .await
        .expect_err("a truncating name was accepted");
    assert!(matches!(err, StoreError::UnsafeIdentifier(_)), "{err}");
    tx.rollback().await.unwrap();
}

/// Ported from `TestUpdatedAtIsMaintainedWithoutTheWriter`.
#[tokio::test]
async fn updated_at_is_maintained_without_the_writer() {
    let w = World::bare("updated_at_maintained").await;
    let plan = plan_for(&unique_app(), vec![coll("entries", &[])]);
    apply(&w, &plan).await.expect("apply");
    let table = format!("\"{}__entries\"", plan.schema);
    let conn = w.conn().await;
    let row = query(&format!(
        "INSERT INTO {table} (id, doc) VALUES (?1, '{{\"a\":1}}') RETURNING id, created_at, updated_at"
    ))
    .bind(Uuid::new_v4())
    .fetch_one(&conn)
    .await
    .unwrap();
    let (id, created, first): (
        Uuid,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) = (row.get("id"), row.get("created_at"), row.get("updated_at"));
    // The trigger reads a millisecond clock; give it a tick to move.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    query(&format!(
        "UPDATE {table} SET doc = '{{\"a\":2}}' WHERE id = ?1"
    ))
    .bind(id)
    .execute(&conn)
    .await
    .unwrap();
    let second: chrono::DateTime<chrono::Utc> =
        query(&format!("SELECT updated_at FROM {table} WHERE id = ?1"))
            .bind(id)
            .fetch_scalar(&conn)
            .await
            .unwrap();
    assert!(
        second > first,
        "updated_at did not move on an update that ignored it: {first} -> {second}"
    );
    let after: chrono::DateTime<chrono::Utc> =
        query(&format!("SELECT created_at FROM {table} WHERE id = ?1"))
            .bind(id)
            .fetch_scalar(&conn)
            .await
            .unwrap();
    assert_eq!(after, created, "created_at moved");
    drop_plan(&w, &plan).await;
}

/// Ported from `TestTouchFunctionLivesInTheAppSchema`: the touch trigger is
/// the collection table's own, so dropping the table takes it along and
/// uninstall stays one statement per table (D3.2).
#[tokio::test]
async fn touch_trigger_lives_on_the_collection_table() {
    let w = World::bare("touch_trigger_on_table").await;
    let plan = plan_for(&unique_app(), vec![coll("entries", &[])]);
    apply(&w, &plan).await.expect("apply");
    let table = format!("{}__entries", plan.schema);
    let triggers: Vec<String> =
        query("SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1")
            .bind(&table)
            .fetch_scalars(&*w.conn().await)
            .await
            .unwrap();
    assert!(
        triggers.iter().any(|t| t.ends_with("_touch")),
        "no touch trigger on {table}: {triggers:?}"
    );
    drop_plan(&w, &plan).await;
    let left: i64 = query("SELECT count(*) FROM sqlite_master WHERE tbl_name = ?1")
        .bind(&table)
        .fetch_scalar(&*w.conn().await)
        .await
        .unwrap();
    assert_eq!(left, 0, "dropping the table left {left} objects behind");
}

/// Ported from `TestApplySchemaPlanIsIdempotent` (D3.3).
#[tokio::test]
async fn apply_schema_plan_is_idempotent() {
    let w = World::bare("apply_schema_plan_idempotent").await;
    let plan = plan_for(&unique_app(), vec![coll("entries", &["btree(entry_date)"])]);
    apply(&w, &plan).await.expect("first apply");
    apply(&w, &plan).await.expect("second apply");
    drop_plan(&w, &plan).await;
}

/// Ported from `TestApplySchemaPlanRollsBackWholly` and
/// `TestApplySchemaPlanComposesWithOtherWork`: a failed install leaves nothing.
#[tokio::test]
async fn apply_schema_plan_rolls_back_wholly() {
    let w = World::bare("apply_schema_plan_rolls_back").await;
    let plan = plan_for(&unique_app(), vec![coll("entries", &[])]);
    let tx = w.store.begin().await.unwrap();
    apply_schema_plan(&tx, &plan).await.expect("apply");
    tx.rollback().await.unwrap(); // something else in the same unit of work fails
    assert!(
        !schema_exists(&w, &plan.schema).await,
        "{} survived a rolled-back transaction",
        plan.schema
    );
}

/// Ported from `TestDropSchemaPlanRemovesEverything` (D3.2).
#[tokio::test]
async fn drop_schema_plan_removes_everything() {
    let w = World::bare("drop_schema_plan_removes").await;
    let plan = plan_for(&unique_app(), vec![coll("entries", &[])]);
    apply(&w, &plan).await.expect("apply");
    drop_plan(&w, &plan).await;
    assert!(
        !schema_exists(&w, &plan.schema).await,
        "{} survived a drop",
        plan.schema
    );
}

/// Ported from `TestVectorIndexIsRefusedRatherThanSkipped`.
#[tokio::test]
async fn vector_index_is_refused_rather_than_skipped() {
    let w = World::bare("vector_index_refused").await;
    let plan = plan_for(
        &unique_app(),
        vec![coll("entries", &["vector(embedding, 1536)"])],
    );
    let tx = w.store.begin().await.unwrap();
    let err = apply_schema_plan(&tx, &plan)
        .await
        .expect_err("a vector index was silently accepted");
    assert!(matches!(err, StoreError::NotImplemented(_)), "{err}");
    assert!(
        err.to_string().contains("vector"),
        "the error should name what is missing: {err}"
    );
    tx.rollback().await.unwrap();
}

/// Ported from `TestApplySchemaPlanRefusesUnsafeIdentifiers`: the check at the
/// point of use.
#[tokio::test]
async fn apply_schema_plan_refuses_unsafe_identifiers() {
    let w = World::bare("apply_schema_plan_unsafe_idents").await;
    let plans = [
        SchemaPlan {
            schema: "app_x\"; DROP SCHEMA public; --".into(),
            collections: vec![],
        },
        SchemaPlan {
            schema: "app_x".into(),
            collections: vec![CollectionPlan {
                name: "entries\"; DROP SCHEMA public; --".into(),
                crud: false,
                indexes: vec![],
            }],
        },
        SchemaPlan {
            schema: "a".repeat(64),
            collections: vec![],
        },
        SchemaPlan {
            schema: String::new(),
            collections: vec![],
        },
    ];
    for plan in plans {
        let tx = w.store.begin().await.unwrap();
        let err = apply_schema_plan(&tx, &plan)
            .await
            .err()
            .unwrap_or_else(|| panic!("plan {:?} accepted", plan.schema));
        assert!(
            matches!(err, StoreError::UnsafeIdentifier(_)),
            "plan {:?}: {err}",
            plan.schema
        );
        tx.rollback().await.unwrap();
    }
    let actors: i64 =
        query("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'actors'")
            .fetch_scalar(&*w.conn().await)
            .await
            .unwrap();
    assert_eq!(
        actors, 1,
        "a rejected identifier still executed; actors is gone"
    );
}

/// Ported from `TestIndexExpressionCannotBeEscaped`.
#[tokio::test]
async fn index_expression_cannot_be_escaped() {
    let w = World::bare("index_expression_cannot_escape").await;
    let plan = SchemaPlan {
        schema: format!("app_{}", unique_app()),
        collections: vec![CollectionPlan {
            name: "entries".into(),
            crud: false,
            indexes: vec![Index {
                method: IndexMethod::BTree,
                path: vec!["body'); DROP SCHEMA public; --".into()],
                dim: 0,
            }],
        }],
    };
    let tx = w.store.begin().await.unwrap();
    let err = apply_schema_plan(&tx, &plan)
        .await
        .expect_err("an escaping path was accepted");
    assert!(matches!(err, StoreError::UnsafeIdentifier(_)), "{err}");
    tx.rollback().await.unwrap();
}
