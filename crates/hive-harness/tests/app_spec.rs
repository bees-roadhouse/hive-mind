//! The app container's isolation, asserted on the argument list (D53 §1,
//! #163 slice 1). Each test names the property it holds and the way it could
//! be lost; none needs Podman.

use std::collections::BTreeMap;
use std::time::Duration;

use hive_harness::{
    APP_ENV_CREDENTIAL, APP_ENV_ENTRY, APP_ENV_INSTALL, APP_ENV_SOCKET, AppRuntime, AppSpec,
    CONTAINER_APP_DIR, CONTAINER_APP_SOCK, Limits, podman_app_args, validate_entry,
};

fn spec() -> AppSpec {
    AppSpec {
        install_id: "inst_01HZX".into(),
        app: "notes".into(),
        runtime: Some(AppRuntime::Bun),
        entry: "src/app.ts".into(),
        image_digest: format!("sha256:{}", "a".repeat(64)),
        image_repository: "ghcr.io/bees-roadhouse/hive-app-notes".into(),
        env: BTreeMap::from([(APP_ENV_CREDENTIAL.to_string(), "hs_test_credential".into())]),
        socket_dir: "/var/lib/hive-sandbox/apps/inst_01HZX".into(),
        daemon_socket: "/run/hive-sandbox/api.sock".into(),
        egress_allow: vec![],
        limits: Limits::default_limits(),
        idle_timeout: Duration::from_secs(300),
    }
}

fn has_flag(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == value)
}

fn env_value<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    args.windows(2)
        .filter(|w| w[0] == "--env")
        .filter_map(|w| w[1].strip_prefix(prefix.as_str()))
        .next_back()
}

/// The sandbox a run gets, an app gets: read-only root, every capability
/// dropped, no new privileges, scratch on tmpfs, the limits on the cgroup.
#[test]
fn an_app_container_has_the_run_sandbox() {
    let args = podman_app_args(&spec(), &[]).expect("args");
    for flag in ["--read-only", "--detach"] {
        assert!(args.contains(&flag.to_string()), "{flag} missing: {args:?}");
    }
    assert!(has_flag(&args, "--cap-drop", "ALL"));
    assert!(has_flag(&args, "--security-opt", "no-new-privileges"));
    assert!(has_flag(&args, "--memory", &format!("{}b", 2u64 << 30)));
    assert!(has_flag(&args, "--cpus", "2"));
    assert!(has_flag(&args, "--pids-limit", "512"));
    assert!(
        args.iter().any(|a| a.starts_with("/tmp:rw,nosuid,nodev")),
        "no tmpfs scratch: {args:?}"
    );
    assert!(
        !args.iter().any(|a| a.contains(":/workspace")),
        "an app has no workspace; its state is the store's: {args:?}"
    );
    assert_eq!(
        args.last().map(String::as_str),
        Some(
            "ghcr.io/bees-roadhouse/hive-app-notes@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ),
        "the image is the digest-pinned reference and the last argument"
    );
}

/// No network by default, and the two sockets are the only way in or out:
/// the install's directory for the app's one socket, the daemon's socket for
/// everything the app does (invariant 13).
#[test]
fn no_network_and_exactly_two_sockets() {
    let args = podman_app_args(&spec(), &[]).expect("args");
    assert!(has_flag(&args, "--network", "none"), "{args:?}");
    assert!(
        has_flag(
            &args,
            "--volume",
            &format!("/var/lib/hive-sandbox/apps/inst_01HZX:{CONTAINER_APP_DIR}:rw")
        ),
        "the install's socket directory is not mounted: {args:?}"
    );
    assert!(
        has_flag(
            &args,
            "--volume",
            "/run/hive-sandbox/api.sock:/run/hive-sandbox/api.sock:rw"
        ),
        "the daemon's socket is not mounted: {args:?}"
    );
    let volumes = args.windows(2).filter(|w| w[0] == "--volume").count();
    assert_eq!(
        volumes, 2,
        "an app container mounts exactly two things: {args:?}"
    );
    assert_eq!(env_value(&args, APP_ENV_SOCKET), Some(CONTAINER_APP_SOCK));
    assert_eq!(
        env_value(&args, "HIVE_SANDBOX_API_SOCKET"),
        Some("/run/hive-sandbox/api.sock")
    );
    assert!(
        !args.iter().any(|a| a.contains("HTTP_PROXY")),
        "no proxy without hosts"
    );
}

/// Hosts in the manifest mean the install's own proxy network, never a
/// route: the proxy variables point at the install's proxy and the network
/// is the install's, so two installs never share an allowlist (invariant 14).
#[test]
fn named_hosts_mean_the_installs_own_proxy_network() {
    let mut s = spec();
    s.egress_allow = vec!["api.example.com:443".into()];
    let args = podman_app_args(&s, &[]).expect("args");
    assert!(
        has_flag(&args, "--network", "hive-sandbox-app-inst_01HZX-net"),
        "{args:?}"
    );
    assert!(!has_flag(&args, "--network", "none"));
    assert_eq!(
        env_value(&args, "HTTPS_PROXY"),
        Some("http://hive-sandbox-app-inst_01HZX-proxy:3128")
    );
    assert_eq!(env_value(&args, "NO_PROXY"), Some("localhost,127.0.0.1"));
}

/// The container is the install's, in its name and its labels, and never the
/// app's alone: two installs of one app are two containers.
#[test]
fn the_container_is_keyed_on_the_install() {
    let args = podman_app_args(&spec(), &[]).expect("args");
    assert!(
        has_flag(&args, "--name", "hive-sandbox-app-inst_01HZX"),
        "{args:?}"
    );
    assert!(has_flag(
        &args,
        "--label",
        "org.beesroadhouse.hive-sandbox.install-id=inst_01HZX"
    ));
    assert!(has_flag(
        &args,
        "--label",
        "org.beesroadhouse.hive-sandbox.app=notes"
    ));
    assert!(has_flag(
        &args,
        "--label",
        "org.beesroadhouse.hive-sandbox.app-runtime=bun"
    ));
    assert_eq!(env_value(&args, APP_ENV_INSTALL), Some("inst_01HZX"));
    assert_eq!(env_value(&args, APP_ENV_ENTRY), Some("src/app.ts"));
    let mut other = spec();
    other.install_id = "inst_02".into();
    let other = podman_app_args(&other, &[]).expect("args");
    assert_ne!(args, other, "two installs of one app produced one command");
}

/// The credential arrives by environment and nothing the launcher sets can be
/// overridden from the spec: Podman takes the last `--env` for a key, so a
/// spec key would win.
#[test]
fn the_credential_arrives_and_launcher_variables_are_refused() {
    let args = podman_app_args(&spec(), &[]).expect("args");
    assert_eq!(
        env_value(&args, APP_ENV_CREDENTIAL),
        Some("hs_test_credential")
    );
    for key in [
        "HOME",
        "HIVE_SANDBOX_API_SOCKET",
        APP_ENV_SOCKET,
        APP_ENV_ENTRY,
        APP_ENV_INSTALL,
        "HTTPS_PROXY",
        "no_proxy",
    ] {
        let mut s = spec();
        s.env.insert(key.into(), "/elsewhere".into());
        let err = podman_app_args(&s, &[]).expect_err(&format!("{key} was accepted"));
        assert!(
            err.0.contains("set by the launcher"),
            "{key}: wrong refusal: {err}"
        );
    }
}

/// The refusals, each with its message, so a bad spec names its field rather
/// than failing inside podman.
#[test]
fn a_spec_that_cannot_be_a_sandbox_is_refused_by_name() {
    type Mutation = Box<dyn Fn(&mut AppSpec)>;
    let cases: Vec<(&str, Mutation, &str)> = vec![
        (
            "no runtime",
            Box::new(|s| s.runtime = None),
            "runtime is required",
        ),
        (
            "no entry",
            Box::new(|s| s.entry.clear()),
            "entry is required",
        ),
        (
            "absolute entry",
            Box::new(|s| s.entry = "/app/src/app.ts".into()),
            "relative to the app root",
        ),
        (
            "dotdot entry",
            Box::new(|s| s.entry = "src/../../etc/passwd".into()),
            "`..` segment",
        ),
        (
            "tag not pin",
            Box::new(|s| s.image_digest = "latest".into()),
            "a tag is not a pin",
        ),
        (
            "no repository",
            Box::new(|s| s.image_repository.clear()),
            "image_repository is required",
        ),
        (
            "socket dir is a volume name",
            Box::new(|s| s.socket_dir = "appsock".into()),
            "is a volume name, not a host path",
        ),
        (
            "no daemon socket",
            Box::new(|s| s.daemon_socket.clear()),
            "daemon_socket is required",
        ),
        (
            "bad install id",
            Box::new(|s| s.install_id = "../x".into()),
            "install_id",
        ),
        (
            "host with a path",
            Box::new(|s| s.egress_allow = vec!["example.com/x".into()]),
            "is not a host",
        ),
        (
            "zero memory",
            Box::new(|s| s.limits.memory_bytes = 0),
            "memory_bytes must be positive",
        ),
    ];
    for (name, mutate, message) in cases {
        let mut s = spec();
        mutate(&mut s);
        let err = podman_app_args(&s, &[]).expect_err(&format!("{name} was accepted"));
        assert!(
            err.0.contains(message),
            "{name}: refused for the wrong reason: {err}"
        );
    }
}

/// `entry` has one rule in two places (the manifest and the spec); this is
/// the spec's copy of it.
#[test]
fn entry_rule() {
    for ok in [
        "app.ts",
        "src/app.ts",
        "src/handlers/main.py",
        "a.b/c-d_e.ts",
    ] {
        validate_entry(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    for bad in [
        "",
        "/app.ts",
        "./app.ts",
        "src//app.ts",
        "src/./app.ts",
        "..",
        "a\nb",
    ] {
        assert!(validate_entry(bad).is_err(), "{bad:?} accepted");
    }
}

/// Zero idle timeout is `always_on`; anything else is a stop after idleness.
#[test]
fn idle_timeout_zero_means_always_on() {
    let mut s = spec();
    assert!(!s.always_on());
    s.idle_timeout = Duration::ZERO;
    assert!(s.always_on());
}
