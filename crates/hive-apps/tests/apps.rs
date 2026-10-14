//! The dispatcher and the container's life, against a fake launcher and a
//! fake app on a loopback TCP port (#163 slice 3). No Podman: the Podman
//! launcher is slice 4's and the container tier tests it. What is asserted
//! here is everything the host decides: when a container starts and stops,
//! what crosses the wire, how trust folds, what a deadline costs, and that the
//! credential the container was handed lives and dies with it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use std::time::Duration;

use async_trait::async_trait;
use axum::{Json, Router, extract::State, routing::get, routing::post};
use hive_apps::{
    AppError, AppEvent, AppEvents, AppImage, AppImages, AppLauncher, Apps, AppsConfig, Invoke,
    InvokeKind, Io, Transport, protocol,
};
use hive_harness::{APP_ENV_CREDENTIAL, AppSpec};
use hive_identity::{Credential, PrincipalKind};
use hive_manifest::{AppRuntime, Collection, Kind, Manifest, Process, Storage};
use hive_store::{Store, resolve_credential};
use hive_testdb::TestDb;
use hive_trust::Level;
use parking_lot::Mutex;
use uuid::Uuid;

/// What the fake app does with an invocation.
#[derive(Clone)]
enum Behaviour {
    /// Echo `data` back under the given trust word (`None` omits the field).
    Echo(Option<&'static str>),
    /// Sleep this long, then echo trusted.
    Sleep(Duration),
    /// Answer 500 with a message.
    Fail(&'static str),
    /// Answer 200 with a body that is not the envelope.
    Garbage,
}

#[derive(Clone)]
struct FakeApp {
    behaviour: Arc<Mutex<Behaviour>>,
    /// Every request the app received, as the host sent it.
    seen: Arc<Mutex<Vec<protocol::Request>>>,
    /// The credential the launcher handed the container, as the app would
    /// read it from its environment.
    credential: Arc<Mutex<Option<String>>>,
}

impl FakeApp {
    fn new() -> FakeApp {
        FakeApp {
            behaviour: Arc::new(Mutex::new(Behaviour::Echo(Some("trusted")))),
            seen: Arc::new(Mutex::new(Vec::new())),
            credential: Arc::new(Mutex::new(None)),
        }
    }

    async fn serve(self) -> SocketAddr {
        let app = Router::new()
            .route(protocol::READY_PATH, get(|| async { "ok" }))
            .route(protocol::INVOKE_PATH, post(invoke_handler))
            .with_state(self);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }
}

async fn invoke_handler(
    State(app): State<FakeApp>,
    Json(req): Json<protocol::Request>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    app.seen.lock().push(req.clone());
    let b = app.behaviour.lock().clone();
    match b {
        Behaviour::Echo(trust) => Json(serde_json::json!({
            "trust": trust,
            "data": req.data,
        }))
        .into_response(),
        Behaviour::Sleep(d) => {
            tokio::time::sleep(d).await;
            Json(serde_json::json!({ "trust": "trusted", "data": req.data })).into_response()
        }
        Behaviour::Fail(msg) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": msg })),
        )
            .into_response(),
        Behaviour::Garbage => "not json at all".into_response(),
    }
}

/// Records what the host asked and routes each install to its fake app.
#[derive(Default)]
struct FakeLauncher {
    starts: Mutex<Vec<AppSpec>>,
    stops: Mutex<Vec<Uuid>>,
    /// Which fake app answers for which install, set by the test.
    apps: Mutex<HashMap<Uuid, (SocketAddr, FakeApp)>>,
    /// Pretend the container never comes up.
    never_ready: Mutex<bool>,
}

#[async_trait]
impl AppLauncher for FakeLauncher {
    async fn start(&self, spec: &AppSpec) -> Result<(), AppError> {
        let id: Uuid = spec.install_id.parse().unwrap();
        if let Some((_, app)) = self.apps.lock().get(&id) {
            *app.credential.lock() = spec.env.get(APP_ENV_CREDENTIAL).cloned();
        }
        self.starts.lock().push(spec.clone());
        Ok(())
    }
    async fn stop(&self, install_id: Uuid) -> Result<(), AppError> {
        self.stops.lock().push(install_id);
        Ok(())
    }
}

#[async_trait]
impl Transport for FakeLauncher {
    async fn connect(&self, install_id: Uuid, _socket_dir: &str) -> Result<Io, AppError> {
        if *self.never_ready.lock() {
            return Err(AppError::Connect(install_id, "nothing listening".into()));
        }
        let addr = self
            .apps
            .lock()
            .get(&install_id)
            .map(|(a, _)| *a)
            .ok_or_else(|| AppError::Connect(install_id, "no fake app".into()))?;
        let s = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| AppError::Connect(install_id, e.to_string()))?;
        Ok(Box::new(s))
    }
}

struct FixedImage;
#[async_trait]
impl AppImages for FixedImage {
    async fn image_for(&self, _: Uuid) -> Result<AppImage, AppError> {
        Ok(AppImage {
            repository: "ghcr.io/bees-roadhouse/hive-app-test".into(),
            digest: format!("sha256:{}", "b".repeat(64)),
        })
    }
}

#[derive(Default)]
struct Recorded(Mutex<Vec<AppEvent>>);
impl AppEvents for Recorded {
    fn emit(&self, e: AppEvent) {
        self.0.lock().push(e);
    }
}

/// A store with a person, a process-app install with its actor, and the
/// service wired to the fakes.
struct World {
    _db: TestDb,
    store: Store,
    alice: Uuid,
    install: Uuid,
    launcher: Arc<FakeLauncher>,
    app: FakeApp,
    events: Arc<Recorded>,
    apps: Arc<Apps>,
}

async fn world(test: &str, cfg: AppsConfig) -> World {
    let db = TestDb::new(test).await;
    let store = Store::from_dbs(db.db().clone(), db.audit().clone());
    let c = store.conn().await.unwrap();
    let alice = Uuid::new_v4();
    hive_db::query(
        "INSERT INTO actors (id, kind, handle, display_name, principal_kind, principal_id, created_by_actor)
         VALUES (?1, 'human', 'alice', 'Alice', 'user', ?1, NULL)",
    )
    .bind(alice)
    .execute(&c)
    .await
    .unwrap();
    let install = process_install(&c, "notes", alice, Duration::from_secs(300), false).await;
    let launcher = Arc::new(FakeLauncher::default());
    let app = FakeApp::new();
    let addr = app.clone().serve().await;
    launcher.apps.lock().insert(install, (addr, app.clone()));
    let events = Arc::new(Recorded::default());
    let apps = Arc::new(
        Apps::new(
            store.clone(),
            launcher.clone(),
            launcher.clone(),
            Arc::new(FixedImage),
            cfg,
        )
        .with_events(events.clone()),
    );
    World {
        _db: db,
        store,
        alice,
        install,
        launcher,
        app,
        events,
        apps,
    }
}

/// A build whose manifest is a process app, an active install of it owned by
/// `owner`, and the install's actor.
async fn process_install(
    c: &hive_db::Connection,
    slug: &str,
    owner: Uuid,
    idle: Duration,
    always_on: bool,
) -> Uuid {
    let m = Manifest {
        kind: Some(Kind::App),
        name: slug.into(),
        version: 1,
        storage: Storage {
            collections: vec![Collection {
                name: "items".into(),
                crud: true,
                indexes: vec![],
            }],
            uses: vec![],
        },
        process: Some(Process {
            runtime: Some(AppRuntime::Bun),
            entry: "src/app.ts".into(),
            idle_timeout_secs: if always_on {
                None
            } else {
                Some(idle.as_secs())
            },
            always_on,
            image: None,
        }),
        ..Default::default()
    };
    let build = Uuid::new_v4();
    let install = Uuid::new_v4();
    hive_db::query(
        "INSERT INTO app_builds (id, slug, kind, impl, manifest, content_hash, author_actor, owner_kind, owner_id, visibility, trust, status)
         VALUES (?1, ?2, 'app', 'host', ?3, ?4, ?5, 'user', ?5, 'private', 'builtin', 'registered')",
    )
    .bind(build)
    .bind(slug)
    .bind(serde_json::to_value(&m).unwrap())
    .bind(format!("{:064x}", install.as_u128()))
    .bind(owner)
    .execute(c)
    .await
    .unwrap();
    hive_db::query(
        "INSERT INTO installs (id, build_id, slug, owner_kind, owner_id, installed_by_actor, activated_by_actor, schema_name, state)
         VALUES (?1, ?2, ?3, 'user', ?4, ?4, ?4, ?5, 'active')",
    )
    .bind(install)
    .bind(build)
    .bind(slug)
    .bind(owner)
    .bind(format!("app_{slug}_{}", &install.simple().to_string()[..8]))
    .execute(c)
    .await
    .unwrap();
    hive_store::ensure_install_actor(
        c,
        install,
        &Credential::new(owner, PrincipalKind::User, owner),
    )
    .await
    .unwrap();
    install
}

fn call(w: &World, data: serde_json::Value) -> Invoke {
    Invoke {
        install_id: w.install,
        kind: InvokeKind::Tool,
        name: "add".into(),
        cred: Credential::new(w.alice, PrincipalKind::User, w.alice),
        trust: Level::Trusted,
        data,
        route: None,
    }
}

fn quick() -> AppsConfig {
    AppsConfig {
        deadline: Duration::from_millis(400),
        max_overruns: 2,
        start_timeout: Duration::from_secs(3),
        socket_root: "/tmp/hive-apps-test".into(),
        daemon_socket: "/tmp/hive-apps-test/api.sock".into(),
        ..AppsConfig::default()
    }
}

/// The first invocation starts the container with a fresh credential in its
/// environment and nothing else about the spec left to the app; the
/// envelope crosses with the caller's facts and never the credential; the
/// answer's trust is the host's fold. A second invocation starts nothing.
#[tokio::test]
async fn first_call_starts_the_container_and_the_envelope_crosses() {
    let w = world("apps_first_call", quick()).await;
    assert!(!w.apps.is_running(w.install));

    let out = w
        .apps
        .invoke(call(&w, serde_json::json!({"n": 1})))
        .await
        .expect("invoke");
    assert_eq!(out.data, serde_json::json!({"n": 1}));
    assert_eq!(out.trust, Level::Trusted);
    assert!(w.apps.is_running(w.install));

    let starts = w.launcher.starts.lock().clone();
    assert_eq!(starts.len(), 1, "one start: {starts:?}");
    let spec = &starts[0];
    assert_eq!(spec.install_id, w.install.to_string());
    assert_eq!(spec.app, "notes");
    assert_eq!(spec.runtime, Some(AppRuntime::Bun));
    assert_eq!(spec.entry, "src/app.ts");
    assert_eq!(spec.idle_timeout, Duration::from_secs(300));
    assert!(
        spec.socket_dir.ends_with(&w.install.to_string()),
        "{}",
        spec.socket_dir
    );
    let token = spec
        .env
        .get(APP_ENV_CREDENTIAL)
        .expect("no credential in the environment");
    assert_eq!(
        spec.env.len(),
        1,
        "something besides the credential crossed: {:?}",
        spec.env
    );

    // The credential the container holds is the install's actor acting for
    // the owner: minted by this start, not reused.
    let c = w.store.conn().await.unwrap();
    let cred = resolve_credential(&c, token)
        .await
        .expect("the container's credential resolves");
    assert_eq!(cred.principal_id, w.alice);
    assert_eq!(
        hive_store::install_of_actor(&c, cred.actor_id)
            .await
            .unwrap(),
        Some(w.install)
    );

    let seen = w.app.seen.lock().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].kind, InvokeKind::Tool);
    assert_eq!(seen[0].name, "add");
    assert_eq!(seen[0].install, w.install);
    assert_eq!(seen[0].caller.actor, w.alice);
    assert_eq!(seen[0].caller.principal_kind, "user");
    let raw = serde_json::to_string(&seen[0]).unwrap();
    assert!(
        !raw.contains(token),
        "the caller's credential crossed to the app"
    );

    w.apps
        .invoke(call(&w, serde_json::json!({"n": 2})))
        .await
        .expect("second");
    assert_eq!(
        w.launcher.starts.lock().len(),
        1,
        "a second call started a second container"
    );
    assert_eq!(
        w.events.0.lock().clone(),
        vec![AppEvent::Started { install: w.install }]
    );
}

/// Trust only goes down, and the host decides: an app that omits it or says
/// anything but `trusted` yields untrusted; untrusted input yields untrusted
/// whatever the app claims (invariant 12).
#[tokio::test]
async fn trust_folds_at_the_host() {
    let w = world("apps_trust", quick()).await;
    *w.app.behaviour.lock() = Behaviour::Echo(None);
    let out = w.apps.invoke(call(&w, serde_json::json!(1))).await.unwrap();
    assert_eq!(
        out.trust,
        Level::Untrusted,
        "a missing trust was read as trusted"
    );

    *w.app.behaviour.lock() = Behaviour::Echo(Some("very trusted"));
    let out = w.apps.invoke(call(&w, serde_json::json!(1))).await.unwrap();
    assert_eq!(out.trust, Level::Untrusted);

    *w.app.behaviour.lock() = Behaviour::Echo(Some("trusted"));
    let mut c = call(&w, serde_json::json!(1));
    c.trust = Level::Untrusted;
    let out = w.apps.invoke(c).await.unwrap();
    assert_eq!(
        out.trust,
        Level::Untrusted,
        "the app laundered untrusted input"
    );
    // And the app was told the input was untrusted.
    assert_eq!(w.app.seen.lock().last().unwrap().trust, Level::Untrusted);
}

/// A handler past the deadline is cut off with the deadline's message; after
/// the configured overruns the container is stopped, its credential revoked,
/// and the next call starts a fresh one. Other work is not affected: the
/// app's own clock keeps running and the host did not wait for it.
#[tokio::test]
async fn a_hung_handler_costs_only_its_container() {
    let w = world("apps_deadline", quick()).await;
    w.apps.invoke(call(&w, serde_json::json!(0))).await.unwrap();
    let first_token = w.app.credential.lock().clone().unwrap();

    *w.app.behaviour.lock() = Behaviour::Sleep(Duration::from_secs(5));
    let started = std::time::Instant::now();
    let err = w
        .apps
        .invoke(call(&w, serde_json::json!(1)))
        .await
        .expect_err("a 5s handler returned");
    assert!(
        matches!(err, AppError::DeadlineExceeded(_, _)),
        "wrong refusal: {err}"
    );
    assert!(err.to_string().contains("deadline"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the host waited for the handler: {:?}",
        started.elapsed()
    );
    assert!(
        w.apps.is_running(w.install),
        "one overrun stopped the container"
    );

    let err = w
        .apps
        .invoke(call(&w, serde_json::json!(2)))
        .await
        .expect_err("second overrun returned");
    assert!(matches!(err, AppError::DeadlineExceeded(_, _)));
    assert!(
        !w.apps.is_running(w.install),
        "two overruns did not stop the container"
    );
    assert_eq!(w.launcher.stops.lock().clone(), vec![w.install]);
    let c = w.store.conn().await.unwrap();
    assert!(
        resolve_credential(&c, &first_token).await.is_err(),
        "the stopped container's credential still resolves"
    );
    let events = w.events.0.lock().clone();
    assert!(
        events.contains(&AppEvent::Overrun {
            install: w.install,
            count: 1
        }),
        "{events:?}"
    );
    assert!(
        events.contains(&AppEvent::Overrun {
            install: w.install,
            count: 2
        }),
        "{events:?}"
    );
    assert!(
        events.contains(&AppEvent::Stopped {
            install: w.install,
            reason: hive_apps::StopReason::Overruns
        }),
        "{events:?}"
    );

    // The next call brings a fresh container with a fresh credential.
    *w.app.behaviour.lock() = Behaviour::Echo(Some("trusted"));
    w.apps
        .invoke(call(&w, serde_json::json!(3)))
        .await
        .expect("fresh container");
    assert_eq!(w.launcher.starts.lock().len(), 2);
    let second_token = w.app.credential.lock().clone().unwrap();
    assert_ne!(
        first_token, second_token,
        "the credential was reused across containers"
    );
    assert!(resolve_credential(&c, &second_token).await.is_ok());
}

/// Idle past the manifest's timeout, the container is stopped and its
/// credential revoked; `always_on` is never swept.
#[tokio::test]
async fn idle_stop_revokes_the_credential_and_always_on_is_not_swept() {
    let w = world("apps_idle", quick()).await;
    // A second install, always_on, beside the first.
    let c = w.store.conn().await.unwrap();
    let forever = process_install(&c, "ticker", w.alice, Duration::ZERO, true).await;
    let ticker = FakeApp::new();
    let addr = ticker.clone().serve().await;
    w.launcher
        .apps
        .lock()
        .insert(forever, (addr, ticker.clone()));

    w.apps.invoke(call(&w, serde_json::json!(1))).await.unwrap();
    let mut c2 = call(&w, serde_json::json!(1));
    c2.install_id = forever;
    w.apps.invoke(c2).await.unwrap();
    let token = w.app.credential.lock().clone().unwrap();

    // Nothing is idle yet.
    assert!(w.apps.sweep_idle().await.unwrap().is_empty());

    // The test cannot move Instant, so a third install gets the smallest
    // timeout the manifest can express (one second; zero would mean
    // always_on) and the test outwaits it.
    let short = process_install(&c, "blink", w.alice, Duration::from_secs(1), false).await;
    let blink = FakeApp::new();
    let addr = blink.clone().serve().await;
    w.launcher.apps.lock().insert(short, (addr, blink.clone()));
    let mut c3 = call(&w, serde_json::json!(1));
    c3.install_id = short;
    w.apps.invoke(c3).await.unwrap();
    let blink_token = blink.credential.lock().clone().unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let swept = w.apps.sweep_idle().await.unwrap();
    assert_eq!(swept, vec![short], "swept {swept:?}");
    assert!(!w.apps.is_running(short));
    assert!(
        w.apps.is_running(w.install),
        "a container inside its timeout was swept"
    );
    assert!(w.apps.is_running(forever), "always_on was swept");
    assert!(
        resolve_credential(&c, &blink_token).await.is_err(),
        "the swept container's credential lives"
    );
    assert!(
        resolve_credential(&c, &token).await.is_ok(),
        "a running container's credential was revoked"
    );
    assert!(w.events.0.lock().contains(&AppEvent::Stopped {
        install: short,
        reason: hive_apps::StopReason::Idle
    }));
}

/// A container that never answers `/ready` is stopped and its credential
/// revoked before the caller hears the refusal, so a broken image leaves no
/// live token behind.
#[tokio::test]
async fn a_container_that_never_comes_up_leaves_no_credential() {
    let mut cfg = quick();
    cfg.start_timeout = Duration::from_millis(300);
    let w = world("apps_not_ready", cfg).await;
    *w.launcher.never_ready.lock() = true;
    let err = w
        .apps
        .invoke(call(&w, serde_json::json!(1)))
        .await
        .expect_err("invoked a dead container");
    assert!(
        matches!(err, AppError::NotReady(_, _)),
        "wrong refusal: {err}"
    );
    assert_eq!(w.launcher.stops.lock().clone(), vec![w.install]);
    let c = w.store.conn().await.unwrap();
    let live: i64 = hive_db::query(
        "SELECT count(*) FROM credentials WHERE install_id = ?1 AND revoked_at IS NULL",
    )
    .bind(w.install)
    .fetch_scalar(&c)
    .await
    .unwrap();
    assert_eq!(live, 0, "a live credential survived a failed start");
    assert!(!w.apps.is_running(w.install));
}

/// The app's refusals and nonsense come back as the host's errors, named.
#[tokio::test]
async fn refusals_and_garbage_are_named() {
    let w = world("apps_refusals", quick()).await;
    *w.app.behaviour.lock() = Behaviour::Fail("no such tool");
    let err = w
        .apps
        .invoke(call(&w, serde_json::json!(1)))
        .await
        .expect_err("a 500 was accepted");
    assert!(matches!(err, AppError::Refused(_, 500, _)), "{err}");
    assert!(err.to_string().contains("no such tool"), "{err}");

    *w.app.behaviour.lock() = Behaviour::Garbage;
    let err = w
        .apps
        .invoke(call(&w, serde_json::json!(1)))
        .await
        .expect_err("garbage was accepted");
    assert!(matches!(err, AppError::BadAnswer(_, _)), "{err}");
    // Neither cost the container: they are the app answering, not hanging.
    assert!(w.apps.is_running(w.install));
}

/// A guest install (no process) is refused by name before anything starts,
/// and a daemon's first act revokes what the last one left.
#[tokio::test]
async fn a_guest_install_is_not_a_process_and_a_restart_revokes() {
    let w = world("apps_guest", quick()).await;
    let c = w.store.conn().await.unwrap();
    let guest = Uuid::new_v4();
    let build = Uuid::new_v4();
    hive_db::query(
        "INSERT INTO app_builds (id, slug, kind, impl, manifest, content_hash, author_actor, owner_kind, owner_id, visibility, trust, status)
         VALUES (?1, 'hello', 'app', 'host', '{}', ?2, ?3, 'user', ?3, 'private', 'builtin', 'registered')",
    )
    .bind(build)
    .bind(format!("{:064x}", guest.as_u128()))
    .bind(w.alice)
    .execute(&c)
    .await
    .unwrap();
    hive_db::query(
        "INSERT INTO installs (id, build_id, slug, owner_kind, owner_id, installed_by_actor, activated_by_actor, schema_name, state)
         VALUES (?1, ?2, 'hello', 'user', ?3, ?3, ?3, ?4, 'active')",
    )
    .bind(guest)
    .bind(build)
    .bind(w.alice)
    .bind(format!("app_hello_{}", &guest.simple().to_string()[..8]))
    .execute(&c)
    .await
    .unwrap();
    let mut g = call(&w, serde_json::json!(1));
    g.install_id = guest;
    let err = w
        .apps
        .invoke(g)
        .await
        .expect_err("a guest install started a container");
    assert!(matches!(err, AppError::NotAProcess(_)), "{err}");
    assert!(w.launcher.starts.lock().is_empty());

    // A running container, then "the daemon restarts": its credential is dead.
    w.apps.invoke(call(&w, serde_json::json!(1))).await.unwrap();
    let token = w.app.credential.lock().clone().unwrap();
    let n = w.apps.revoke_stale_credentials().await.unwrap();
    assert_eq!(n, 1);
    assert!(resolve_credential(&c, &token).await.is_err());
}
