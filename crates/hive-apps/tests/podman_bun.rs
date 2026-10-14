//! D53 scenario 1, the half that needs no daemon: the reference Bun app
//! (apps/hello-bun, on @hive/app) runs in a real container from the image
//! scripts/app-build.sh built and pinned, answers a tool call through the
//! dispatch socket with `{trust, data}`, and reports from inside that it
//! holds its credential and no network. The write that lands with the
//! install's trust and owner needs the daemon behind the mounted socket and
//! is #168 slice 4. Skips aloud without podman or the pin; CI's containers
//! job forbids the skip. Unix only: the transport is a unix socket.

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
    use hive_manifest::Manifest;
    use hive_store::Store;
    use hive_testdb::TestDb;
    use hive_trust::Level;
    use uuid::Uuid;

    const REQUIRE_ENV: &str = "HIVE_SANDBOX_REQUIRE_CONTAINER_TESTS";
    const PIN_PATH: &str = "apps/hello-bun/image.json";
    const MANIFEST_PATH: &str = "apps/hello-bun/manifest.json";

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

    /// A person and an active install of the checked-in manifest, with its
    /// actor. The manifest is the real file, validated as the registry would.
    async fn install(store: &Store, m: &Manifest) -> (Uuid, Uuid) {
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
        let build = Uuid::new_v4();
        let install = Uuid::new_v4();
        hive_db::query(
            "INSERT INTO app_builds (id, slug, kind, impl, manifest, content_hash, author_actor, owner_kind, owner_id, visibility, trust, status)
             VALUES (?1, ?2, 'app', 'host', ?3, ?4, ?5, 'user', ?5, 'private', 'builtin', 'registered')",
        )
        .bind(build)
        .bind(&m.name)
        .bind(serde_json::to_value(m).unwrap())
        .bind(format!("{:064x}", install.as_u128()))
        .bind(alice)
        .execute(&c)
        .await
        .unwrap();
        hive_db::query(
            "INSERT INTO installs (id, build_id, slug, owner_kind, owner_id, installed_by_actor, activated_by_actor, schema_name, state)
             VALUES (?1, ?2, ?3, 'user', ?4, ?4, ?4, ?5, 'active')",
        )
        .bind(install)
        .bind(build)
        .bind(&m.name)
        .bind(alice)
        .bind(format!("app_{}_{}", m.name, &install.simple().to_string()[..8]))
        .execute(&c)
        .await
        .unwrap();
        hive_store::ensure_install_actor(&c, install, &Credential::new(alice, PrincipalKind::User, alice))
            .await
            .unwrap();
        (alice, install)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_reference_bun_app_answers_a_tool_call_in_a_real_container() {
        const T: &str = "the_reference_bun_app_answers_a_tool_call_in_a_real_container";
        if !podman_ok(&["version"]) {
            return skip_or_fail(T, "podman not on PATH");
        }
        let pin = match EgressPin::load(repo_path(PIN_PATH)) {
            Ok(p) => p,
            Err(e) => {
                return skip_or_fail(T, &format!("no hello-bun pin ({e}); run scripts/app-build.sh apps/hello-bun"));
            }
        };
        let image = AppImage::parse(&pin.reference()).expect("the pin is a reference");
        let raw = std::fs::read(repo_path(MANIFEST_PATH)).expect("apps/hello-bun/manifest.json");
        let manifest: Manifest = serde_json::from_slice(&raw).expect("the manifest parses");
        manifest.validate().expect("the checked-in manifest validates");
        let process = manifest.process.clone().expect("hello-bun is a process app");

        let db = TestDb::new("apps_podman_bun").await;
        let store = Store::from_dbs(db.db().clone(), db.audit().clone());
        let (alice, install_id) = install(&store, &manifest).await;

        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("api.sock");
        let _daemon = tokio::net::UnixListener::bind(&api).unwrap();
        let apps = Apps::new(
            store.clone(),
            Arc::new(PodmanAppLauncher::default()),
            Arc::new(UnixTransport),
            Arc::new(Pinned(image)),
            AppsConfig {
                socket_root: dir.path().join("apps").to_string_lossy().into_owned(),
                daemon_socket: api.to_string_lossy().into_owned(),
                deadline: Duration::from_secs(10),
                start_timeout: Duration::from_secs(90),
                ..AppsConfig::default()
            },
        );
        let name = format!("hive-sandbox-app-{install_id}");
        let call = |tool: &str, data: serde_json::Value| Invoke {
            install_id,
            kind: InvokeKind::Tool,
            name: tool.into(),
            cred: Credential::new(alice, PrincipalKind::User, alice),
            trust: Level::Trusted,
            data,
            route: None,
        };

        // Scenario 1's tool call: through the dispatch socket, `{trust, data}` back.
        let out = apps.invoke(call("add", serde_json::json!({"a": 2, "b": 40}))).await.expect("add");
        assert_eq!(out.trust, Level::Trusted);
        assert_eq!(out.data, serde_json::json!({"sum": 42}));
        assert!(container_exists(&name));

        // What the app sees from inside: its install, the caller, the
        // credential by environment, the entry the manifest named.
        let out = apps.invoke(call("whoami", serde_json::Value::Null)).await.expect("whoami");
        let d = &out.data;
        assert_eq!(d["install"], install_id.to_string(), "{d}");
        assert_eq!(d["caller"]["actor"], alice.to_string(), "{d}");
        assert_eq!(d["has_credential"], true, "{d}");
        assert_eq!(d["entry"], process.entry, "{d}");
        assert_eq!(d["trust"], "trusted");

        // The SDK's refusal shape crosses as the host's named error.
        let err = apps
            .invoke(call("add", serde_json::json!({"a": "x"})))
            .await
            .expect_err("a bad argument was accepted");
        assert!(matches!(err, AppError::Refused(_, 400, _)), "{err}");
        assert!(err.to_string().contains("add wants"), "{err}");

        // A route call, with the HTTP facts the SDK hands the handler.
        let out = apps
            .invoke(Invoke {
                install_id,
                kind: InvokeKind::Route,
                name: "greet".into(),
                cred: Credential::new(alice, PrincipalKind::User, alice),
                trust: Level::Trusted,
                data: serde_json::json!({"method": "GET", "path": "/greet/nate", "params": {"name": "nate"}, "query": {}, "body": null}),
                route: Some(hive_apps::RouteFacts { method: "GET".into(), path: "/greet/nate".into(), query: None }),
            })
            .await
            .expect("greet");
        assert_eq!(out.data, serde_json::json!({"greeting": "hello, nate"}));

        apps.stop(install_id).await.expect("stop");
        assert!(!container_exists(&name), "the container outlived stop");
    }
}

#[cfg(not(unix))]
#[test]
fn unix_only() {
    println!("SKIPPED: the_reference_bun_app_answers_a_tool_call_in_a_real_container needs unix sockets");
}
