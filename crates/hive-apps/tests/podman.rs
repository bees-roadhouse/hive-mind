//! The container tier (D53, #163 slice 4): a real container under rootless
//! Podman, started by the Podman launcher from the pinned test image, spoken
//! to over its unix socket, stopped, and nothing left behind. What the fake
//! launcher's suite cannot see is what this one asserts: the sandbox from
//! inside (no network, the uid, the variables the launcher set), the socket
//! actually crossing the bind mount, and the container going away.
//!
//! Skips, aloud, where there is no podman or no pinned image; CI's containers
//! job sets `HIVE_SANDBOX_REQUIRE_CONTAINER_TESTS=1` so the skip is a failure
//! there. Unix only: the transport is a unix socket.

#[cfg(unix)]
mod tier {
    use std::sync::Arc;
    use std::time::Duration;

    use hive_apps::{
        AppError, AppImage, AppImages, Apps, AppsConfig, Invoke, InvokeKind, PodmanAppLauncher,
        UnixTransport,
    };
    use hive_harness::EgressPin;
    use hive_identity::{Credential, PrincipalKind};
    use hive_manifest::{AppRuntime, Kind, Manifest, Process};
    use hive_store::Store;
    use hive_testdb::TestDb;
    use hive_trust::Level;
    use uuid::Uuid;

    const REQUIRE_ENV: &str = "HIVE_SANDBOX_REQUIRE_CONTAINER_TESTS";
    const PIN_PATH: &str = "docker/app-test/digest.json";

    fn skip_or_fail(test: &str, why: &str) {
        if std::env::var(REQUIRE_ENV).is_ok_and(|v| !v.is_empty()) {
            panic!("{REQUIRE_ENV} is set, so this must not skip: {why}");
        }
        println!("SKIPPED: {test} {why}");
    }

    fn repo_path(rel: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel)
    }

    fn podman_ok(args: &[&str]) -> bool {
        std::process::Command::new("podman")
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn container_exists(name: &str) -> bool {
        std::process::Command::new("podman")
            .args(["container", "exists", name])
            .status()
            .is_ok_and(|s| s.success())
    }

    struct Pinned(AppImage);
    #[async_trait::async_trait]
    impl AppImages for Pinned {
        async fn image_for(&self, _: Uuid) -> Result<AppImage, AppError> {
            Ok(self.0.clone())
        }
    }

    /// A person and an active process-app install with its actor, written
    /// straight into the store (the registry's path is proved in its own
    /// suites).
    async fn install(store: &Store) -> (Uuid, Uuid) {
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
        let m = Manifest {
            kind: Some(Kind::App),
            name: "echo".into(),
            version: 1,
            process: Some(Process {
                runtime: Some(AppRuntime::Python),
                entry: "app.py".into(),
                idle_timeout_secs: Some(60),
                always_on: false,
                image: None,
            }),
            ..Default::default()
        };
        let build = Uuid::new_v4();
        let install = Uuid::new_v4();
        hive_db::query(
            "INSERT INTO app_builds (id, slug, kind, impl, manifest, content_hash, author_actor, owner_kind, owner_id, visibility, trust, status)
             VALUES (?1, 'echo', 'app', 'host', ?2, ?3, ?4, 'user', ?4, 'private', 'builtin', 'registered')",
        )
        .bind(build)
        .bind(serde_json::to_value(&m).unwrap())
        .bind(format!("{:064x}", install.as_u128()))
        .bind(alice)
        .execute(&c)
        .await
        .unwrap();
        hive_db::query(
            "INSERT INTO installs (id, build_id, slug, owner_kind, owner_id, installed_by_actor, activated_by_actor, schema_name, state)
             VALUES (?1, ?2, 'echo', 'user', ?3, ?3, ?3, ?4, 'active')",
        )
        .bind(install)
        .bind(build)
        .bind(alice)
        .bind(format!("app_echo_{}", &install.simple().to_string()[..8]))
        .execute(&c)
        .await
        .unwrap();
        hive_store::ensure_install_actor(
            &c,
            install,
            &Credential::new(alice, PrincipalKind::User, alice),
        )
        .await
        .unwrap();
        (alice, install)
    }

    /// One container, end to end: the first call starts it from the pinned
    /// image, the app answers over the bind-mounted socket and reports the
    /// sandbox from inside, a hung handler is cut at the deadline, stop
    /// removes the container and revokes the credential, and nothing is left.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_real_container_serves_the_socket_and_sees_no_network() {
        const T: &str = "a_real_container_serves_the_socket_and_sees_no_network";
        if !podman_ok(&["version"]) {
            return skip_or_fail(T, "podman not on PATH");
        }
        let pin = match EgressPin::load(repo_path(PIN_PATH)) {
            Ok(p) => p,
            Err(e) => {
                return skip_or_fail(
                    T,
                    &format!("no app-test pin ({e}); run scripts/app-test-build.sh"),
                );
            }
        };
        let image = AppImage::parse(&pin.reference()).expect("the pin is a reference");

        let db = TestDb::new("apps_podman").await;
        let store = Store::from_dbs(db.db().clone(), db.audit().clone());
        let (alice, install_id) = install(&store).await;

        // A stand-in for the daemon's socket: the container gets it mounted
        // and the test asserts nothing about it beyond the mount working.
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("api.sock");
        let _daemon = tokio::net::UnixListener::bind(&api).unwrap();

        let cfg = AppsConfig {
            socket_root: dir.path().join("apps").to_string_lossy().into_owned(),
            daemon_socket: api.to_string_lossy().into_owned(),
            deadline: Duration::from_millis(1500),
            max_overruns: 2,
            start_timeout: Duration::from_secs(60),
            ..AppsConfig::default()
        };
        let apps = Apps::new(
            store.clone(),
            Arc::new(PodmanAppLauncher::default()),
            Arc::new(UnixTransport),
            Arc::new(Pinned(image)),
            cfg,
        );
        let name = format!("hive-sandbox-app-{install_id}");
        let call = |data: serde_json::Value| Invoke {
            install_id,
            kind: InvokeKind::Tool,
            name: "echo".into(),
            cred: Credential::new(alice, PrincipalKind::User, alice),
            trust: Level::Trusted,
            data,
            route: None,
        };

        let out = apps
            .invoke(call(serde_json::json!({"n": 7})))
            .await
            .expect("first call");
        assert!(
            container_exists(&name),
            "the container is not running after a successful call"
        );
        assert_eq!(out.trust, Level::Trusted);
        let d = &out.data;
        assert_eq!(d["echo"], serde_json::json!({"n": 7}), "{d}");
        assert_eq!(d["kind"], "tool");
        assert_eq!(d["name"], "echo");
        assert_eq!(d["caller"]["actor"], alice.to_string());
        assert_eq!(
            d["has_credential"], true,
            "the credential did not arrive by environment: {d}"
        );
        assert_eq!(d["entry"], "app.py");
        assert_eq!(d["install"], install_id.to_string());
        assert_eq!(d["uid"], 1001, "{d}");
        assert_eq!(
            d["network"], false,
            "the container reached the network: {d}"
        );

        let c = store.conn().await.unwrap();
        let live = |c: &hive_db::Conn| async move {
            hive_db::query(
                "SELECT count(*) FROM credentials WHERE install_id = ?1 AND revoked_at IS NULL",
            )
            .bind(install_id)
            .fetch_scalar::<i64>(&**c)
            .await
            .unwrap()
        };
        assert_eq!(
            live(&c).await,
            1,
            "one live credential while the container runs"
        );

        // A hung handler is cut off; the container survives one overrun.
        let err = apps
            .invoke(call(serde_json::json!({"sleep": 5})))
            .await
            .expect_err("a 5s handler returned");
        assert!(matches!(err, AppError::DeadlineExceeded(_, _)), "{err}");
        assert!(apps.is_running(install_id));

        apps.stop(install_id).await.expect("stop");
        assert!(!container_exists(&name), "the container outlived stop");
        assert_eq!(live(&c).await, 0, "the credential outlived the container");
        assert!(!apps.is_running(install_id));
    }
}

#[cfg(not(unix))]
#[test]
fn unix_only() {
    println!("SKIPPED: a_real_container_serves_the_socket_and_sees_no_network needs unix sockets");
}
