//! The models worker against a fake OpenAI-shaped server (D42). What is
//! under test is the seam: the request each capability sends, the result
//! each one lands, the closed-list rule, the failure path, and that a job
//! with no endpoint fails at once when this is the only worker.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use hive_blob::{Catalog, CreateUpload, DiskDriver, Driver, Provenance, RefSpec, SourceKind};
use hive_identity::{Credential, Owner, PrincipalKind};
use hive_manifest::{Collection, Kind, Manifest, Storage};
use hive_models::{Config, Endpoint, Endpoints, Worker};
use hive_store::{
    BootstrapConfig, BuildSpec, InstallSpec, JOB_DONE, JOB_FAILED, JobInput, JobSpec, ModelJobs,
    Store, activate_install, register_build, stage_install,
};
use hive_testdb::TestDb;
use hive_trust::Level;
use uuid::Uuid;

/// What the fake server saw and what it will say.
#[derive(Default)]
struct Seen {
    requests: Vec<(String, HeaderMap, Vec<u8>)>,
    /// The next chat answer's content.
    chat_content: String,
    /// Fail every request with 500 when set.
    broken: bool,
}

type Shared = Arc<Mutex<Seen>>;

async fn chat(
    State(s): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let mut seen = s.lock().unwrap();
    seen.requests
        .push(("/v1/chat/completions".into(), headers, body.to_vec()));
    if seen.broken {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
    }
    let content = seen.chat_content.clone();
    axum::Json(serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": content}}]
    }))
    .into_response()
}

async fn embeddings(
    State(s): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    s.lock()
        .unwrap()
        .requests
        .push(("/v1/embeddings".into(), headers, body.to_vec()));
    axum::Json(serde_json::json!({
        "data": [{"embedding": [0.1, 0.2, 0.3, 0.4]}]
    }))
    .into_response()
}

async fn transcriptions(
    State(s): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    s.lock()
        .unwrap()
        .requests
        .push(("/v1/audio/transcriptions".into(), headers, body.to_vec()));
    axum::Json(serde_json::json!({
        "text": "hello from the recording",
        "language": "en",
        "segments": [{"start": 0.0, "end": 1.2, "text": "hello from the recording"}]
    }))
    .into_response()
}

use axum::response::IntoResponse;

async fn fake_server() -> (SocketAddr, Shared) {
    let seen: Shared = Arc::new(Mutex::new(Seen::default()));
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/audio/transcriptions", post(transcriptions))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, seen)
}

struct Fx {
    _db: TestDb,
    store: Store,
    jobs: ModelJobs,
    blobs: Arc<Catalog>,
    driver: DiskDriver,
    cred: Credential,
    install: Uuid,
    _dir: tempfile::TempDir,
}

async fn fixture(test: &str) -> Fx {
    let db = TestDb::new(test).await;
    let store = Store::from_dbs(db.db().clone(), db.audit().clone());
    let res = store
        .bootstrap_in_tx(&BootstrapConfig {
            root_handle: "root".into(),
            root_name: "Root".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let root = res.root_actor_id;
    let cred = Credential::new(root, PrincipalKind::User, root);
    let m = Manifest {
        kind: Some(Kind::App),
        name: "documents".into(),
        version: 1,
        storage: Storage {
            collections: vec![Collection {
                name: "docs".into(),
                crud: true,
                indexes: vec![],
            }],
            uses: vec![],
        },
        capabilities: vec!["models".into()],
        ..Default::default()
    };
    let p = hive_registry::prepare(&m, &hive_wasmhost::Exports::none()).unwrap();
    let spec = p.install_spec("user", &root.to_string()).unwrap();
    let tx = store.begin().await.unwrap();
    let reg = register_build(
        &tx,
        &BuildSpec {
            spec,
            owner: Some(Owner::user(root)),
            trust: "builtin".into(),
        },
        &cred,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let conn = store.conn().await.unwrap();
    let install = stage_install(
        &conn,
        &InstallSpec {
            build_id: reg.build_id,
            slug: "documents".into(),
            owner: Owner::user(root),
        },
        &cred,
    )
    .await
    .unwrap();
    activate_install(&conn, install, &cred).await.unwrap();
    drop(conn);
    let dir = tempfile::tempdir().unwrap();
    let driver = DiskDriver::new(dir.path()).await.unwrap();
    let blobs = Arc::new(Catalog::new(
        db.db().clone(),
        Box::new(DiskDriver::new(dir.path()).await.unwrap()),
    ));
    Fx {
        jobs: ModelJobs::new(store.clone()),
        _db: db,
        store,
        blobs,
        driver,
        cred,
        install,
        _dir: dir,
    }
}

impl Fx {
    async fn hold(&self, bytes: &[u8], mime: &str) -> hive_blob::Hash {
        let mut up = self
            .driver
            .create_upload(CreateUpload::default())
            .await
            .unwrap();
        up.write(bytes).await.unwrap();
        let sealed = up.seal().await.unwrap();
        let tx = self.store.begin().await.unwrap();
        let (desc, _) = self
            .blobs
            .publish(
                &tx,
                sealed,
                mime,
                &Provenance::original(),
                &RefSpec {
                    cred: self.cred,
                    source_kind: SourceKind::Upload,
                    source_id: Uuid::new_v4().to_string(),
                    trust: Level::Trusted,
                },
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        desc.hash
    }

    fn worker(&self, addr: SocketAddr) -> Worker {
        let ep = |model: &str| {
            Some(Endpoint {
                url: format!("http://{addr}/v1"),
                model: model.into(),
                api_key: "local-key".into(),
            })
        };
        Worker::new(
            self.store.clone(),
            self.blobs.clone(),
            Config {
                name: "test-worker".into(),
                endpoints: Endpoints {
                    read: ep("vision-1"),
                    transcribe: ep("whisper-1"),
                    generate: ep("text-1"),
                    embed: None,
                },
                call_timeout: Duration::from_secs(10),
                poll_interval: Duration::ZERO,
                concurrency: 1,
            },
        )
        .unwrap()
    }

    async fn submit(&self, capability: &str, input: JobInput, options: serde_json::Value) -> Uuid {
        self.jobs
            .submit(
                &self.cred,
                self.install,
                &JobSpec {
                    capability: capability.into(),
                    input,
                    options,
                },
            )
            .await
            .unwrap()
    }

    async fn view(&self, job: Uuid) -> hive_store::JobView {
        self.jobs
            .result(&self.cred, self.install, job)
            .await
            .unwrap()
            .unwrap()
    }
}

/// `generate` sends the prompt and the text as one user message with the
/// bearer key and the configured model, and lands the content as text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generate_sends_prompt_and_text_and_lands_the_answer() {
    let f = fixture("models_worker_generate").await;
    let (addr, seen) = fake_server().await;
    seen.lock().unwrap().chat_content = "A tidy summary.".into();
    let job = f
        .submit(
            "generate",
            JobInput::Text("Long document text.".into()),
            serde_json::json!({"prompt": "Summarise in one line.", "system": "You are terse."}),
        )
        .await;
    let w = f.worker(addr);
    assert!(w.run_one().await.unwrap());
    let v = f.view(job).await;
    assert_eq!(v.state, JOB_DONE, "{v:?}");
    assert_eq!(v.result.unwrap()["text"], "A tidy summary.");

    let seen = seen.lock().unwrap();
    let (path, headers, body) = &seen.requests[0];
    assert_eq!(path, "/v1/chat/completions");
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some("Bearer local-key")
    );
    let body: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(body["model"], "text-1");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(
        body["messages"][1]["content"],
        "Summarise in one line.\n\nLong document text."
    );
}

/// A closed list of choices is validated: an answer on the list lands as
/// the choice, an answer off it fails the job rather than inventing a
/// category.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_choice_off_the_list_is_a_failure() {
    let f = fixture("models_worker_choices").await;
    let (addr, seen) = fake_server().await;
    let w = f.worker(addr);
    let opts = serde_json::json!({
        "prompt": "Pick the category.",
        "choices": ["Receipts", "Letters", "Medical"]
    });

    seen.lock().unwrap().chat_content = "letters".into();
    let job = f
        .submit("generate", JobInput::Text("Dear sir".into()), opts.clone())
        .await;
    assert!(w.run_one().await.unwrap());
    let v = f.view(job).await;
    assert_eq!(v.state, JOB_DONE, "{v:?}");
    assert_eq!(
        v.result.unwrap()["choice"],
        "Letters",
        "the list's own spelling is the answer"
    );

    seen.lock().unwrap().chat_content = "Poetry".into();
    let job = f
        .submit("generate", JobInput::Text("Roses".into()), opts)
        .await;
    assert!(w.run_one().await.unwrap());
    let v = f.view(job).await;
    assert_eq!(v.state, JOB_FAILED, "{v:?}");
    assert!(v.error.unwrap().contains("not one of the choices"));
}

/// `read` sends the page as a data URL in an image message; `transcribe`
/// sends the audio as a multipart file; both land text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_and_transcribe_send_the_bytes_and_land_text() {
    let f = fixture("models_worker_bytes").await;
    let (addr, seen) = fake_server().await;
    seen.lock().unwrap().chat_content = "The words on the page.".into();
    let page = f.hold(b"\x89PNG not really", "image/png").await;
    let audio = f.hold(b"RIFF not really", "audio/wav").await;
    let w = f.worker(addr);

    let job = f
        .submit(
            "read",
            JobInput::Blob(page, "image/png".into()),
            serde_json::json!({}),
        )
        .await;
    assert!(w.run_one().await.unwrap());
    let v = f.view(job).await;
    assert_eq!(v.state, JOB_DONE, "{v:?}");
    assert_eq!(v.result.unwrap()["text"], "The words on the page.");

    let job = f
        .submit(
            "transcribe",
            JobInput::Blob(audio, "audio/wav".into()),
            serde_json::json!({}),
        )
        .await;
    assert!(w.run_one().await.unwrap());
    let v = f.view(job).await;
    assert_eq!(v.state, JOB_DONE, "{v:?}");
    let r = v.result.unwrap();
    assert_eq!(r["text"], "hello from the recording");
    assert_eq!(r["language"], "en");

    let seen = seen.lock().unwrap();
    let (_, _, body) = &seen.requests[0];
    let body: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(body["model"], "vision-1");
    let image = body["messages"][0]["content"][1]["image_url"]["url"]
        .as_str()
        .unwrap();
    assert!(image.starts_with("data:image/png;base64,"), "{image}");
    let (path, headers, body) = &seen.requests[1];
    assert_eq!(path, "/v1/audio/transcriptions");
    assert!(
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.starts_with("multipart/form-data")),
        "{headers:?}"
    );
    let text = String::from_utf8_lossy(body);
    assert!(
        text.contains("whisper-1") && text.contains("RIFF not really"),
        "{text}"
    );
}

/// A server error fails the job with the status in the error, and a job
/// for a capability this worker has no endpoint for fails at once, naming
/// the variable to set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_land_as_failed_with_a_reason() {
    let f = fixture("models_worker_failures").await;
    let (addr, seen) = fake_server().await;
    seen.lock().unwrap().broken = true;
    let w = f.worker(addr);
    let job = f
        .submit(
            "generate",
            JobInput::Text("x".into()),
            serde_json::json!({}),
        )
        .await;
    assert!(w.run_one().await.unwrap());
    let v = f.view(job).await;
    assert_eq!(v.state, JOB_FAILED);
    assert!(v.error.as_deref().unwrap().contains("500"), "{v:?}");

    let job = f
        .submit("embed", JobInput::Text("x".into()), serde_json::json!({}))
        .await;
    assert!(w.run_one().await.unwrap());
    let v = f.view(job).await;
    assert_eq!(v.state, JOB_FAILED);
    assert!(
        v.error
            .as_deref()
            .unwrap()
            .contains("HIVE_SANDBOX_MODELS_EMBED_MODEL"),
        "the error does not say what to configure: {v:?}"
    );
}
