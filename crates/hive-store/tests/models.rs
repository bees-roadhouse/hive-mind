//! The model job queue and the `models` capability (D42). Written before
//! the code: what a guest may ask, what it may read back, and what the
//! worker's claim, finish and reclaim guarantee.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{World, cred, user};
use hive_blob::{Catalog, CreateUpload, DiskDriver, Driver, Provenance, RefSpec, SourceKind};
use hive_db::query;
use hive_identity::{Credential, PrincipalKind};
use hive_manifest::{Collection, Kind, Manifest, Storage};
use hive_store::{
    BuildSpec, GuestModels, InstallSpec, JOB_CLAIMED, JOB_DONE, JOB_FAILED, JOB_FINISHED_EVENT,
    JOB_PENDING, JobInput, JobSpec, MAX_ATTEMPTS, ModelJobs, activate_install, register_build,
    stage_install,
};
use hive_trust::Level;
use hive_wasmhost::{Caller, Models as _, Request, Status};
use uuid::Uuid;

struct Fx {
    w: World,
    jobs: ModelJobs,
    guest: GuestModels,
    blobs: Arc<Catalog>,
    driver: DiskDriver,
    alice: Uuid,
    install: Uuid,
    _dir: tempfile::TempDir,
}

async fn install_app(w: &World, name: &str, owner: Uuid) -> Uuid {
    let by = cred(owner, PrincipalKind::User, owner);
    let m = Manifest {
        kind: Some(Kind::App),
        name: name.into(),
        version: 1,
        storage: Storage {
            collections: vec![Collection {
                name: "docs".into(),
                crud: true,
                indexes: vec![],
            }],
            uses: vec![],
        },
        capabilities: vec!["models".into(), "blob".into()],
        ..Default::default()
    };
    let p = hive_registry::prepare(&m, &hive_wasmhost::Exports::none()).expect("prepare");
    let spec = p.install_spec("user", &owner.to_string()).expect("spec");
    let tx = w.store.begin().await.unwrap();
    let reg = register_build(
        &tx,
        &BuildSpec {
            spec,
            owner: Some(user(owner)),
            trust: "builtin".into(),
        },
        &by,
    )
    .await
    .expect("register");
    tx.commit().await.unwrap();
    let conn = w.conn().await;
    let install = stage_install(
        &conn,
        &InstallSpec {
            build_id: reg.build_id,
            slug: name.into(),
            owner: user(owner),
        },
        &by,
    )
    .await
    .expect("stage");
    activate_install(&conn, install, &by)
        .await
        .expect("activate");
    install
}

async fn fixture(test: &str) -> Fx {
    let w = World::new(test).await;
    let alice = w.human("alice").await;
    let install = install_app(&w, "documents", alice).await;
    let dir = tempfile::tempdir().unwrap();
    let driver = DiskDriver::new(dir.path()).await.unwrap();
    let blobs = Arc::new(Catalog::new(
        w.db().clone(),
        Box::new(DiskDriver::new(dir.path()).await.unwrap()),
    ));
    Fx {
        jobs: ModelJobs::new(w.store.clone()),
        guest: GuestModels::new(w.store.clone(), blobs.clone()),
        w,
        blobs,
        driver,
        alice,
        install,
        _dir: dir,
    }
}

impl Fx {
    fn cred(&self) -> Credential {
        cred(self.alice, PrincipalKind::User, self.alice)
    }

    fn req(&self, c: Credential, install: Uuid, body: serde_json::Value) -> Request {
        Request {
            caller: Caller::new(c, install),
            app: String::new(),
            body: serde_json::to_vec(&body).unwrap(),
            trust: Level::Trusted,
            tainted_by: String::new(),
        }
    }

    /// Publishes bytes under `c`'s reference and returns the hash.
    async fn hold(&self, c: Credential, bytes: &[u8]) -> String {
        let mut up = self
            .driver
            .create_upload(CreateUpload::default())
            .await
            .unwrap();
        up.write(bytes).await.unwrap();
        let sealed = up.seal().await.unwrap();
        let tx = self.w.store.begin().await.unwrap();
        let (desc, _) = self
            .blobs
            .publish(
                &tx,
                sealed,
                "image/png",
                &Provenance::original(),
                &RefSpec {
                    cred: c,
                    source_kind: SourceKind::Upload,
                    source_id: Uuid::new_v4().to_string(),
                    trust: Level::Trusted,
                },
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        desc.hash.to_string()
    }

    async fn state(&self, job: Uuid) -> String {
        query("SELECT state FROM model_jobs WHERE id = ?1")
            .bind(job)
            .fetch_scalar(&*self.w.conn().await)
            .await
            .unwrap()
    }
}

/// The whole round trip as a guest sees it: submit text, the worker claims
/// and answers, the finished event lands under the owner, and the result
/// comes back untrusted.
#[tokio::test]
async fn submit_claim_finish_and_read_back() {
    let f = fixture("models_round_trip").await;
    let res = f
        .guest
        .submit(f.req(
            f.cred(),
            f.install,
            serde_json::json!({
                "capability": "generate",
                "text": "Summarise this.",
                "options": {"prompt": "one line"}
            }),
        ))
        .await
        .expect("submit");
    let v: serde_json::Value = serde_json::from_slice(&res.data).unwrap();
    let job: Uuid = v["job"].as_str().unwrap().parse().unwrap();
    assert_eq!(f.state(job).await, JOB_PENDING);

    // Pending: readable, no result, nothing untrusted about it yet.
    let res = f
        .guest
        .result(f.req(f.cred(), f.install, serde_json::json!({"job": job})))
        .await
        .expect("result while pending");
    assert_eq!(res.trust, Level::Trusted);
    let v: serde_json::Value = serde_json::from_slice(&res.data).unwrap();
    assert_eq!(v["state"], JOB_PENDING);
    assert!(v.get("result").is_none());

    let claim = f
        .jobs
        .claim("w1", Duration::from_secs(60))
        .await
        .expect("claim")
        .expect("a pending job");
    assert_eq!(claim.id, job);
    assert_eq!(claim.capability, "generate");
    assert_eq!(claim.input_text.as_deref(), Some("Summarise this."));
    assert_eq!(claim.options["prompt"], "one line");
    assert_eq!(claim.install_id, f.install);
    assert_eq!(claim.owner, user(f.alice));
    assert_eq!(claim.attempts, 1);
    assert_eq!(f.state(job).await, JOB_CLAIMED);
    assert!(
        f.jobs
            .claim("w2", Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "a claimed job was claimed twice"
    );

    let before: i64 = query("SELECT count(*) FROM events WHERE kind = ?1")
        .bind(JOB_FINISHED_EVENT)
        .fetch_scalar(&*f.w.conn().await)
        .await
        .unwrap();
    let landed = f
        .jobs
        .finish(
            job,
            "w1",
            Ok(serde_json::json!({"text": "A short summary."})),
        )
        .await
        .expect("finish");
    assert!(landed);
    assert_eq!(f.state(job).await, JOB_DONE);
    let events: Vec<(String, String)> =
        query("SELECT owner_id, body FROM events WHERE kind = ?1 ORDER BY id DESC LIMIT 1")
            .bind(JOB_FINISHED_EVENT)
            .fetch_all(&*f.w.conn().await)
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get("owner_id"), r.get("body")))
            .collect();
    let after: i64 = query("SELECT count(*) FROM events WHERE kind = ?1")
        .bind(JOB_FINISHED_EVENT)
        .fetch_scalar(&*f.w.conn().await)
        .await
        .unwrap();
    assert_eq!(after - before, 1, "finishing did not append one event");
    let (owner, body) = &events[0];
    assert_eq!(
        owner,
        &f.alice.to_string(),
        "the event is not under the asking owner"
    );
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["job"], job.to_string());
    assert_eq!(body["state"], JOB_DONE);

    let res = f
        .guest
        .result(f.req(f.cred(), f.install, serde_json::json!({"job": job})))
        .await
        .expect("result");
    assert_eq!(
        res.trust,
        Level::Untrusted,
        "a model's words came back trusted"
    );
    let v: serde_json::Value = serde_json::from_slice(&res.data).unwrap();
    assert_eq!(v["state"], JOB_DONE);
    assert_eq!(v["result"]["text"], "A short summary.");

    // A second finish from anyone changes nothing.
    assert!(!f.jobs.finish(job, "w1", Err("late".into())).await.unwrap());
    assert_eq!(f.state(job).await, JOB_DONE);
}

/// A blob input is proved through the caller's references: alice's upload
/// goes in, bob's does not, and the two refusals are the same not-found.
#[tokio::test]
async fn a_blob_input_must_be_held_by_the_caller() {
    let f = fixture("models_blob_held").await;
    let bob = f.w.human("bob").await;
    let mine = f.hold(f.cred(), b"a scan").await;
    let theirs = f
        .hold(cred(bob, PrincipalKind::User, bob), b"a private scan")
        .await;

    let res = f
        .guest
        .submit(f.req(
            f.cred(),
            f.install,
            serde_json::json!({"capability": "read", "blob": mine}),
        ))
        .await
        .expect("my own upload");
    let v: serde_json::Value = serde_json::from_slice(&res.data).unwrap();
    let job: Uuid = v["job"].as_str().unwrap().parse().unwrap();
    let claim = f
        .jobs
        .claim("w1", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.id, job);
    assert_eq!(claim.input_blob.map(|h| h.to_string()), Some(mine));

    let err = f
        .guest
        .submit(f.req(
            f.cred(),
            f.install,
            serde_json::json!({"capability": "read", "blob": theirs}),
        ))
        .await
        .expect_err("bob's upload was accepted by its hash");
    assert_eq!(err.status(), Status::NotFound, "{err}");
    let err = f
        .guest
        .submit(f.req(
            f.cred(),
            f.install,
            serde_json::json!({"capability": "read", "blob": "0".repeat(64)}),
        ))
        .await
        .expect_err("a hash that was never stored was accepted");
    assert_eq!(err.status(), Status::NotFound, "{err}");

    for body in [
        serde_json::json!({"capability": "paint", "text": "x"}),
        serde_json::json!({"capability": "read"}),
        serde_json::json!({"capability": "read", "text": "x", "blob": "1".repeat(64)}),
    ] {
        let err = f
            .guest
            .submit(f.req(f.cred(), f.install, body.clone()))
            .await
            .expect_err("a malformed submit was accepted");
        assert_eq!(err.status(), Status::Invalid, "{body}: {err}");
    }
}

/// A job is keyed on the install that asked: another of the same owner's
/// installs gets not-found for it (invariant 14).
#[tokio::test]
async fn a_result_is_the_asking_installs() {
    let f = fixture("models_keyed_on_install").await;
    let other = install_app(&f.w, "journal", f.alice).await;
    let res = f
        .guest
        .submit(f.req(
            f.cred(),
            f.install,
            serde_json::json!({"capability": "embed", "text": "hello"}),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&res.data).unwrap();
    let job: Uuid = v["job"].as_str().unwrap().parse().unwrap();
    let err = f
        .guest
        .result(f.req(f.cred(), other, serde_json::json!({"job": job})))
        .await
        .expect_err("another install read a job it did not submit");
    assert_eq!(err.status(), Status::NotFound, "{err}");
}

/// A lapsed lease hands the job back; after MAX_ATTEMPTS it is failed as
/// abandoned; and the worker that lost the lease cannot finish it.
#[tokio::test]
async fn a_lapsed_lease_is_reclaimed_then_failed() {
    let f = fixture("models_reclaim").await;
    let job = f
        .jobs
        .submit(
            &f.cred(),
            f.install,
            &JobSpec {
                capability: "transcribe".into(),
                input: JobInput::Text("x".into()),
                options: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
    for attempt in 1..=MAX_ATTEMPTS {
        let claim = f
            .jobs
            .claim("flaky", Duration::from_millis(1))
            .await
            .unwrap()
            .expect("the job is pending again");
        assert_eq!(claim.id, job);
        assert_eq!(claim.attempts, attempt);
        tokio::time::sleep(Duration::from_millis(10)).await;
        let (requeued, failed) = f.jobs.reclaim_lapsed().await.unwrap();
        if attempt < MAX_ATTEMPTS {
            assert_eq!((requeued, failed), (1, 0), "attempt {attempt}");
            assert_eq!(f.state(job).await, JOB_PENDING);
            assert!(
                !f.jobs
                    .finish(job, "flaky", Ok(serde_json::json!({})))
                    .await
                    .unwrap(),
                "a worker whose lease lapsed finished the job"
            );
        } else {
            assert_eq!((requeued, failed), (0, 1), "the last attempt");
            assert_eq!(f.state(job).await, JOB_FAILED);
        }
    }
    let view = f
        .jobs
        .result(&f.cred(), f.install, job)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(view.state, JOB_FAILED);
    assert!(view.error.unwrap().contains("abandoned"));

    // A live lease is extended by its holder and by nobody else.
    let job2 = f
        .jobs
        .submit(
            &f.cred(),
            f.install,
            &JobSpec {
                capability: "embed".into(),
                input: JobInput::Text("y".into()),
                options: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
    f.jobs
        .claim("steady", Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap();
    assert!(
        f.jobs
            .extend_lease(job2, "steady", Duration::from_secs(5))
            .await
            .unwrap()
    );
    assert!(
        !f.jobs
            .extend_lease(job2, "other", Duration::from_secs(5))
            .await
            .unwrap()
    );
}
