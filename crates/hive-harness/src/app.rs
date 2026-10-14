//! A process app's container (D53 §1): one per install, long-lived, the same
//! sandbox shape as an agent run with two differences the record names. The
//! app listens on ONE unix socket the host created, bind-mounted from the
//! install's directory on the host, and the host dispatches into it; and the
//! app reaches the host over the daemon's socket with an install credential
//! that arrives by environment and dies with the container.
//!
//! Everything here is pure, like [`crate::podman_run_args`]: the argument list
//! is a function of the spec, so the isolation properties are asserted in a
//! unit test rather than only observed by running a container. The launcher
//! that starts and stops these containers, the dispatcher that speaks to the
//! socket, and the credential's minting are later slices of #163; this is the
//! shape they build on.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::podman::{
    CONTAINER_DAEMON_SOCK, CONTAINER_GID, CONTAINER_HOME, CONTAINER_HOME_TMP_SIZE,
    CONTAINER_TMP_SIZE, CONTAINER_UID, format_cpus,
};
use crate::spec::{Limits, SpecError};

/// The runtime and the `entry` rule are the manifest's (one place, so the
/// manifest and the spec refuse the same strings with the same words); the
/// harness re-exports them so a caller building a spec has one import.
pub use hive_manifest::{AppRuntime, validate_entry};

/// Where the app's own socket lives inside its container. The directory is
/// the install's on the host, bind-mounted; the app creates the socket file,
/// the host connects to it. Fixed: an app does not choose where the host
/// finds it.
pub const CONTAINER_APP_DIR: &str = "/run/app";
pub const CONTAINER_APP_SOCK: &str = "/run/app/app.sock";

/// The app's code, read-only, where the generated image put it and where
/// `entry` is resolved.
pub const CONTAINER_APP_ROOT: &str = "/app";

/// The variables the launcher sets for an app and refuses from a spec, for
/// the same reason the run launcher refuses its own: Podman takes the last
/// `--env` for a key, so a spec's value would win over the sandbox's.
pub const APP_ENV_SOCKET: &str = "HIVE_APP_SOCKET";
pub const APP_ENV_ENTRY: &str = "HIVE_APP_ENTRY";
pub const APP_ENV_INSTALL: &str = "HIVE_APP_INSTALL";
/// The install credential (D53 §1): minted when the container starts, pinned
/// to the install's AI-shaped actor acting for the install's owner, revoked
/// when the container stops. It is the one secret an app container carries
/// and it arrives here and nowhere else.
pub const APP_ENV_CREDENTIAL: &str = "HIVE_APP_CREDENTIAL";

static INSTALL_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,62}$").expect("static regex"));

/// What an app container is. The whole input, like [`crate::RunSpec`]: the
/// launcher reads no ambient configuration and the container inherits no host
/// state.
#[derive(Clone, Debug, Default)]
pub struct AppSpec {
    /// The install this container serves. It names the container and its
    /// labels, and it is the install and never the app: two installs of one
    /// app are two containers with two credentials, or the second install
    /// reaches the first one's data through a shared process (invariant 14,
    /// the seventh instance was a name that carried the app and not the
    /// owner).
    pub install_id: String,

    /// Informational, on the label beside the install.
    pub app: String,

    pub runtime: Option<AppRuntime>,

    /// The handler file the runtime starts, relative to the app's root inside
    /// the image. Validated here the way the manifest validates it, because a
    /// spec is built from more than a manifest.
    pub entry: String,

    /// Pins the image. Required: a tag is not a pin.
    pub image_digest: String,

    /// Required: there is no default repository for apps, because the digest
    /// alone does not say where the registry built it.
    pub image_repository: String,

    /// Injected at spawn, sorted. The install credential arrives here under
    /// [`APP_ENV_CREDENTIAL`]; the launcher sets the socket, entry and install
    /// variables itself and refuses them from the spec.
    pub env: BTreeMap<String, String>,

    /// The install's directory on the host, bind-mounted at
    /// [`CONTAINER_APP_DIR`]. The app creates its socket in it; nothing else
    /// of the app's outlives the container.
    pub socket_dir: String,

    /// The daemon's API socket on the host, bind-mounted at the same path a
    /// run gets (invariant 13). Required: an app with no way to the host is
    /// an app with no storage.
    pub daemon_socket: String,

    /// The hosts the manifest's capabilities name, one `host[:port]` per
    /// entry. Empty means `--network=none`; non-empty means the install's own
    /// proxy network, the proxy owning the allowlist, as a proxied run.
    pub egress_allow: Vec<String>,

    pub limits: Limits,

    /// How long the container stays up with no invocation before the launcher
    /// stops it. Zero means `always_on` (the manifest said so): never stopped
    /// for idleness.
    pub idle_timeout: Duration,
}

impl AppSpec {
    pub fn container_name(&self) -> String {
        format!("hive-sandbox-app-{}", self.install_id)
    }

    pub fn proxy_container_name(&self) -> String {
        format!("hive-sandbox-app-{}-proxy", self.install_id)
    }

    pub fn egress_network_name(&self) -> String {
        format!("hive-sandbox-app-{}-net", self.install_id)
    }

    pub fn proxy_url(&self) -> String {
        format!("http://{}:3128", self.proxy_container_name())
    }

    pub fn image_ref(&self) -> String {
        format!("{}@{}", self.image_repository, self.image_digest)
    }

    pub fn runtime(&self) -> Result<AppRuntime, SpecError> {
        self.runtime
            .ok_or_else(|| SpecError("runtime is required".into()))
    }

    pub fn always_on(&self) -> bool {
        self.idle_timeout.is_zero()
    }

    pub fn validate(&self) -> Result<(), SpecError> {
        if !INSTALL_ID.is_match(&self.install_id) {
            return Err(SpecError(format!(
                "install_id {:?} must match {}",
                self.install_id,
                INSTALL_ID.as_str()
            )));
        }
        self.runtime()?;
        validate_entry(&self.entry).map_err(SpecError)?;
        if !self.image_digest.starts_with("sha256:") || self.image_digest.len() != 71 {
            return Err(SpecError(
                "image_digest must be a sha256 digest (a tag is not a pin)".into(),
            ));
        }
        if self.image_repository.is_empty() || self.image_repository.contains('@') {
            return Err(SpecError(
                "image_repository is required and carries no digest of its own".into(),
            ));
        }
        for (name, value) in [
            ("socket_dir", &self.socket_dir),
            ("daemon_socket", &self.daemon_socket),
        ] {
            if value.is_empty() {
                return Err(SpecError(format!("{name} is required")));
            }
            // Podman reads a source with no slash as a named volume, which is
            // a different thing with a different lifetime (the run spec learnt
            // this on the person's config volume). A host path has one.
            if !value.contains('/') && !value.contains('\\') {
                return Err(SpecError(format!(
                    "{name} {value:?} is a volume name, not a host path"
                )));
            }
        }
        for host in &self.egress_allow {
            if host.is_empty() || host.contains(char::is_whitespace) || host.contains('/') {
                return Err(SpecError(format!(
                    "egress_allow entry {host:?} is not a host"
                )));
            }
        }
        self.limits.validate()
    }
}

/// Variables the launcher sets and a spec may not. The sandbox depends on
/// them: the socket paths are how the two sides find each other, HOME is the
/// tmpfs, the proxy variables are the only egress.
static RESERVED_APP_ENV: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "HOME",
        "HIVE_SANDBOX_API_SOCKET",
        APP_ENV_SOCKET,
        APP_ENV_ENTRY,
        APP_ENV_INSTALL,
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "NO_PROXY",
        "no_proxy",
    ])
});

/// Builds `podman run` for one app container. Pure, so every deny below is
/// asserted by a test a spec cannot talk its way past.
///
/// What differs from a run, and why: no `/workspace` (an app's state is the
/// store's, through the API; a writable directory it expects to find again
/// is the broken app D53 §2 describes); the install's directory mounted for
/// the one socket; `--network=none` unless the manifest named hosts; a
/// `--stop-timeout` so idle-stop is a SIGTERM the app may finish a request
/// under; and no `--rm`-and-terminate dance, because the launcher owns the
/// container's life across many invocations.
pub fn podman_app_args(spec: &AppSpec, extra: &[String]) -> Result<Vec<String>, SpecError> {
    spec.validate()?;
    let s = |v: &str| v.to_string();
    let runtime = spec.runtime()?;

    let mut args: Vec<String> = vec![
        s("run"),
        s("--detach"),
        s("--rm"),
        s("--name"),
        spec.container_name(),
        s("--workdir"),
        s(CONTAINER_APP_ROOT),
        s("--userns"),
        format!("keep-id:uid={CONTAINER_UID},gid={CONTAINER_GID}"),
        s("--user"),
        format!("{CONTAINER_UID}:{CONTAINER_GID}"),
        s("--read-only"),
        s("--cap-drop"),
        s("ALL"),
        s("--security-opt"),
        s("no-new-privileges"),
        s("--tmpfs"),
        format!("/tmp:rw,nosuid,nodev,size={CONTAINER_TMP_SIZE}"),
        s("--tmpfs"),
        format!("{CONTAINER_HOME}:rw,nosuid,nodev,size={CONTAINER_HOME_TMP_SIZE}"),
        s("--memory"),
        format!("{}b", spec.limits.memory_bytes),
        s("--cpus"),
        format_cpus(spec.limits.cpus),
        s("--pids-limit"),
        spec.limits.pids_limit.to_string(),
        // SIGTERM, then ten seconds, then SIGKILL: an idle stop lets a
        // request in flight finish; a hung app does not get to refuse.
        s("--stop-timeout"),
        s("10"),
        s("--label"),
        format!(
            "org.beesroadhouse.hive-sandbox.install-id={}",
            spec.install_id
        ),
        s("--label"),
        format!("org.beesroadhouse.hive-sandbox.app={}", spec.app),
        s("--label"),
        format!("org.beesroadhouse.hive-sandbox.app-runtime={runtime}"),
        // The install's directory, for the one socket the app may hold.
        s("--volume"),
        format!("{}:{CONTAINER_APP_DIR}:rw", spec.socket_dir),
        // The daemon's API, as a run gets it (invariant 13): a unix socket is
        // the only shape that reaches the host without a route.
        s("--volume"),
        format!("{}:{CONTAINER_DAEMON_SOCK}:rw", spec.daemon_socket),
        s("--env"),
        format!("HIVE_SANDBOX_API_SOCKET={CONTAINER_DAEMON_SOCK}"),
    ];

    if spec.egress_allow.is_empty() {
        args.extend([s("--network"), s("none")]);
    } else {
        // The install's own internal network reaches its proxy and nothing
        // else; the proxy owns the allowlist. As for a run: direct egress has
        // no route, so a client library that ignores the proxy variables
        // fails rather than escapes.
        let proxy = spec.proxy_url();
        args.extend([
            s("--network"),
            spec.egress_network_name(),
            s("--env"),
            format!("HTTP_PROXY={proxy}"),
            s("--env"),
            format!("HTTPS_PROXY={proxy}"),
            s("--env"),
            format!("http_proxy={proxy}"),
            s("--env"),
            format!("https_proxy={proxy}"),
            s("--env"),
            s("NO_PROXY=localhost,127.0.0.1"),
            s("--env"),
            s("no_proxy=localhost,127.0.0.1"),
        ]);
    }

    args.extend([
        s("--env"),
        format!("HOME={CONTAINER_HOME}"),
        s("--env"),
        format!("{APP_ENV_SOCKET}={CONTAINER_APP_SOCK}"),
        s("--env"),
        format!("{APP_ENV_ENTRY}={}", spec.entry),
        s("--env"),
        format!("{APP_ENV_INSTALL}={}", spec.install_id),
    ]);

    for (key, value) in &spec.env {
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            return Err(SpecError(format!("invalid env key {key:?}")));
        }
        if RESERVED_APP_ENV.contains(key.as_str()) {
            return Err(SpecError(format!(
                "{key:?} is set by the launcher and cannot be overridden"
            )));
        }
        args.extend([s("--env"), format!("{key}={value}")]);
    }

    args.extend(extra.iter().cloned());
    args.push(spec.image_ref());
    Ok(args)
}
