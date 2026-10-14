//! Process apps (D53 §1, #163 slices 3 and 4): the host side of "an app is a
//! process in a container, one per install".
//!
//! Two jobs, kept apart by two traits so the first can be tested without
//! Podman and the second can be swapped for it:
//!
//! - **The container's life.** [`Apps`] starts an install's container on the
//!   first invocation and stops it after the idle timeout the manifest set
//!   (or never, for `always_on`). Starting mints the install credential
//!   (hive-store's `issue_install_credential`) and hands it to the container
//!   by environment; stopping revokes it. Both are the launcher's to carry
//!   out ([`AppLauncher`]) and the host's to decide. A daemon that starts
//!   revokes every install credential first, because every container it was
//!   supervising died with it.
//! - **The dispatch.** One HTTP request per invocation over the one socket
//!   the app listens on ([`Transport`] connects to it), `{trust, data}` in and
//!   out ([`protocol`]), under a deadline the host enforces by dropping the
//!   request. A handler that overruns N times costs its container a restart
//!   and nothing else (D53 §4: a hung app costs only itself).
//!
//! What this crate does not decide: whether the caller may invoke (the
//! `Guard`, before this), what the app may reach (its credential, after this),
//! and the image it runs ([`AppImages`], the registry's). It is a dispatcher
//! and a supervisor of long-lived containers, and the pure argument list it
//! starts them with is `hive_harness::podman_app_args`.

pub mod images;
pub mod podman;
pub mod protocol;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hive_harness::{APP_ENV_CREDENTIAL, AppSpec, Limits};
use hive_identity::Credential;
use hive_manifest::{Manifest, Process};
use hive_store::Store;
use hive_trust::Level;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite};
use uuid::Uuid;

pub use images::ManifestImages;
pub use podman::PodmanAppLauncher;
pub use protocol::{InvokeKind, RouteFacts};

/// A connection to an app, whatever carries it. Unix sockets in production;
/// a test may hand back anything that reads and writes.
pub type Io = Box<dyn AsyncReadWrite + Send + Unpin>;

pub trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite> AsyncReadWrite for T {}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("app: no such install {0}")]
    NoInstall(Uuid),
    #[error("app: install {0} is not a process app")]
    NotAProcess(Uuid),
    #[error("app: {0}")]
    Store(String),
    #[error("app: launcher: {0}")]
    Launcher(String),
    #[error("app: image: {0}")]
    Image(String),
    #[error("app: container for install {0} did not answer /ready within {1:?}")]
    NotReady(Uuid, Duration),
    #[error("app: connect to install {0}: {1}")]
    Connect(Uuid, String),
    #[error("app: deadline of {1:?} exceeded by install {0}; the request was dropped")]
    DeadlineExceeded(Uuid, Duration),
    #[error("app: install {0} answered {1}: {2}")]
    Refused(Uuid, u16, String),
    #[error("app: install {0} answered something that is not the envelope: {1}")]
    BadAnswer(Uuid, String),
    #[error("app: {0}")]
    Spec(#[from] hive_harness::SpecError),
}

impl From<hive_store::StoreError> for AppError {
    fn from(e: hive_store::StoreError) -> AppError {
        AppError::Store(e.to_string())
    }
}

/// Starts and stops containers. The Podman implementation shells out with
/// `podman_app_args`; a test's records what it was asked.
#[async_trait]
pub trait AppLauncher: Send + Sync {
    /// Starts the container described, detached. The credential is in
    /// `spec.env` under [`APP_ENV_CREDENTIAL`] and goes nowhere else.
    async fn start(&self, spec: &AppSpec) -> Result<(), AppError>;
    /// Stops and removes the install's container. Safe when nothing runs.
    async fn stop(&self, install_id: Uuid) -> Result<(), AppError>;
}

/// Opens a connection to an install's socket. Production connects to
/// `<socket_dir>/app.sock`; a test may connect anywhere.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn connect(&self, install_id: Uuid, socket_dir: &str) -> Result<Io, AppError>;
}

/// The digest-pinned image an install runs. The registry's business (D53 §1:
/// built from the runtime or named by the manifest); this crate only asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppImage {
    pub repository: String,
    pub digest: String,
}

#[async_trait]
pub trait AppImages: Send + Sync {
    async fn image_for(&self, install_id: Uuid) -> Result<AppImage, AppError>;
}

/// What happened to a container, for the events table and the storage page
/// (D53 §1: starting and stopping are events). The daemon wires the sink;
/// the default drops them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppEvent {
    Started { install: Uuid },
    Stopped { install: Uuid, reason: StopReason },
    Overrun { install: Uuid, count: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Idle,
    Overruns,
    Requested,
}

pub trait AppEvents: Send + Sync {
    fn emit(&self, event: AppEvent);
}

struct NoEvents;
impl AppEvents for NoEvents {
    fn emit(&self, _: AppEvent) {}
}

/// The host's knobs. Every one has a platform default; the manifest may set
/// only the idle timeout and `always_on` (D53 §1).
#[derive(Clone, Debug)]
pub struct AppsConfig {
    /// The host directory under which each install gets `<id>/` for its
    /// socket.
    pub socket_root: String,
    /// The daemon's own API socket, mounted into every container.
    pub daemon_socket: String,
    /// The wall clock one invocation gets before the host drops it.
    pub deadline: Duration,
    /// Overruns before the container is restarted.
    pub max_overruns: u32,
    /// How long a freshly started container has to answer `/ready`.
    pub start_timeout: Duration,
    /// The idle timeout for a manifest that set none.
    pub default_idle_timeout: Duration,
    pub limits: Limits,
}

impl Default for AppsConfig {
    fn default() -> Self {
        AppsConfig {
            socket_root: "/run/hive-sandbox/apps".into(),
            daemon_socket: "/run/hive-sandbox/api.sock".into(),
            deadline: Duration::from_secs(30),
            max_overruns: 3,
            start_timeout: Duration::from_secs(20),
            default_idle_timeout: Duration::from_secs(300),
            limits: Limits::default_limits(),
        }
    }
}

/// One invocation as the host sees it, before the wire.
#[derive(Clone, Debug)]
pub struct Invoke {
    pub install_id: Uuid,
    pub kind: InvokeKind,
    pub name: String,
    /// Who is calling. The app learns the actor and the principal, never
    /// the credential.
    pub cred: Credential,
    pub trust: Level,
    pub data: serde_json::Value,
    pub route: Option<RouteFacts>,
}

/// What came back, with the host's verdict on its trust.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub data: serde_json::Value,
    pub trust: Level,
}

struct Running {
    socket_dir: String,
    last_used: Instant,
    overruns: u32,
    idle_timeout: Duration,
}

/// The service. One per daemon; `Clone` is `Arc`-cheap through the daemon
/// holding it in an `Arc`.
pub struct Apps {
    store: Store,
    launcher: Arc<dyn AppLauncher>,
    transport: Arc<dyn Transport>,
    images: Arc<dyn AppImages>,
    events: Arc<dyn AppEvents>,
    cfg: AppsConfig,
    running: Mutex<HashMap<Uuid, Running>>,
    /// One start at a time per install: two first calls racing would start
    /// two containers under one name and mint two credentials.
    starting: tokio::sync::Mutex<()>,
}

impl Apps {
    pub fn new(
        store: Store,
        launcher: Arc<dyn AppLauncher>,
        transport: Arc<dyn Transport>,
        images: Arc<dyn AppImages>,
        cfg: AppsConfig,
    ) -> Apps {
        Apps {
            store,
            launcher,
            transport,
            images,
            events: Arc::new(NoEvents),
            cfg,
            running: Mutex::new(HashMap::new()),
            starting: tokio::sync::Mutex::new(()),
        }
    }

    pub fn with_events(mut self, events: Arc<dyn AppEvents>) -> Apps {
        self.events = events;
        self
    }

    /// Whether the install's container is up as far as this process knows.
    pub fn is_running(&self, install_id: Uuid) -> bool {
        self.running.lock().contains_key(&install_id)
    }

    /// The daemon's first act: every container this process was supervising
    /// died with it, so every install credential is dead too. Returns how
    /// many were revoked.
    pub async fn revoke_stale_credentials(&self) -> Result<u64, AppError> {
        let c = self.store.conn().await?;
        Ok(hive_store::revoke_all_install_credentials(&c).await?)
    }

    /// Dispatches one invocation, starting the container if it is not up.
    pub async fn invoke(&self, call: Invoke) -> Result<Outcome, AppError> {
        let socket_dir = self.ensure_running(call.install_id).await?;
        let io = self.transport.connect(call.install_id, &socket_dir).await?;
        let req = protocol::Request {
            kind: call.kind,
            name: call.name.clone(),
            install: call.install_id,
            caller: protocol::CallerFacts::from(&call.cred),
            trust: call.trust,
            data: call.data,
            route: call.route,
        };
        let body = serde_json::to_vec(&req)
            .map_err(|e| AppError::BadAnswer(call.install_id, e.to_string()))?;

        let fut = send(io, protocol::INVOKE_PATH, Some(body));
        let answer = match tokio::time::timeout(self.cfg.deadline, fut).await {
            Ok(r) => r.map_err(|e| AppError::Connect(call.install_id, e))?,
            Err(_) => {
                // Dropping `fut` closed the connection, which is how the host
                // cuts a handler off. The container pays for repeated ones.
                self.overrun(call.install_id).await;
                return Err(AppError::DeadlineExceeded(
                    call.install_id,
                    self.cfg.deadline,
                ));
            }
        };
        self.touch(call.install_id);
        if answer.status != 200 {
            let msg = serde_json::from_slice::<protocol::ErrorBody>(&answer.body)
                .map(|e| e.error)
                .unwrap_or_else(|_| String::from_utf8_lossy(&answer.body).to_string());
            return Err(AppError::Refused(call.install_id, answer.status, msg));
        }
        let resp: protocol::Response = serde_json::from_slice(&answer.body)
            .map_err(|e| AppError::BadAnswer(call.install_id, e.to_string()))?;
        Ok(Outcome {
            data: resp.data,
            trust: protocol::fold_trust(call.trust, resp.trust.as_deref()),
        })
    }

    /// Stops the install's container and revokes its credential. Safe when
    /// nothing runs.
    pub async fn stop(&self, install_id: Uuid) -> Result<(), AppError> {
        self.stop_with(install_id, StopReason::Requested).await
    }

    /// Stops every container idle past its timeout. The daemon runs this on a
    /// timer; a test calls it.
    pub async fn sweep_idle(&self) -> Result<Vec<Uuid>, AppError> {
        let now = Instant::now();
        let idle: Vec<Uuid> = self
            .running
            .lock()
            .iter()
            .filter(|(_, r)| {
                !r.idle_timeout.is_zero() && now.duration_since(r.last_used) >= r.idle_timeout
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &idle {
            self.stop_with(*id, StopReason::Idle).await?;
        }
        Ok(idle)
    }

    async fn ensure_running(&self, install_id: Uuid) -> Result<String, AppError> {
        if let Some(r) = self.running.lock().get(&install_id) {
            return Ok(r.socket_dir.clone());
        }
        let _one_at_a_time = self.starting.lock().await;
        if let Some(r) = self.running.lock().get(&install_id) {
            return Ok(r.socket_dir.clone());
        }
        let (spec, idle_timeout) = self.spec_for(install_id).await?;
        let socket_dir = spec.socket_dir.clone();
        self.launcher.start(&spec).await?;
        if let Err(e) = self.wait_ready(install_id, &socket_dir).await {
            // A container that never answered is stopped and its credential
            // revoked, so a broken image does not leave a live token behind.
            let _ = self.launcher.stop(install_id).await;
            let _ = self.revoke(install_id).await;
            return Err(e);
        }
        self.running.lock().insert(
            install_id,
            Running {
                socket_dir: socket_dir.clone(),
                last_used: Instant::now(),
                overruns: 0,
                idle_timeout,
            },
        );
        self.events.emit(AppEvent::Started {
            install: install_id,
        });
        Ok(socket_dir)
    }

    /// The container's spec from the install's row and its build's manifest,
    /// with a fresh credential in its environment.
    async fn spec_for(&self, install_id: Uuid) -> Result<(AppSpec, Duration), AppError> {
        let c = self.store.conn().await?;
        let row = hive_db::query(
            "SELECT i.slug, b.manifest FROM installs i JOIN app_builds b ON b.id = i.build_id
              WHERE i.id = ?1 AND i.state = 'active'",
        )
        .bind(install_id)
        .fetch_optional(&c)
        .await
        .map_err(|e| AppError::Store(e.to_string()))?
        .ok_or(AppError::NoInstall(install_id))?;
        let slug: String = row.get("slug");
        let manifest: serde_json::Value = row.get("manifest");
        let m: Manifest = serde_json::from_value(manifest)
            .map_err(|e| AppError::Store(format!("manifest of install {install_id}: {e}")))?;
        let p: Process = m.process.ok_or(AppError::NotAProcess(install_id))?;
        let idle_timeout = if p.always_on {
            Duration::ZERO
        } else {
            p.idle_timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(self.cfg.default_idle_timeout)
        };
        let image = self.images.image_for(install_id).await?;
        // Minted now, for this container, and revoked when it stops.
        let (token, _) = hive_store::issue_install_credential(&c, install_id).await?;
        let mut env = std::collections::BTreeMap::new();
        env.insert(APP_ENV_CREDENTIAL.to_string(), token);
        let spec = AppSpec {
            install_id: install_id.to_string(),
            app: slug,
            runtime: p.runtime,
            entry: p.entry,
            image_digest: image.digest,
            image_repository: image.repository,
            env,
            socket_dir: format!("{}/{install_id}", self.cfg.socket_root),
            daemon_socket: self.cfg.daemon_socket.clone(),
            // The hosts an install may reach come from its capabilities; the
            // mapping is D53's next slice and until then an app has no egress.
            egress_allow: vec![],
            limits: self.cfg.limits,
            idle_timeout,
        };
        spec.validate()?;
        Ok((spec, idle_timeout))
    }

    async fn wait_ready(&self, install_id: Uuid, socket_dir: &str) -> Result<(), AppError> {
        let deadline = Instant::now() + self.cfg.start_timeout;
        loop {
            if let Ok(io) = self.transport.connect(install_id, socket_dir).await
                && let Ok(answer) = tokio::time::timeout(
                    Duration::from_secs(2),
                    send(io, protocol::READY_PATH, None),
                )
                .await
                && let Ok(answer) = answer
                && answer.status == 200
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(AppError::NotReady(install_id, self.cfg.start_timeout));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn touch(&self, install_id: Uuid) {
        if let Some(r) = self.running.lock().get_mut(&install_id) {
            r.last_used = Instant::now();
        }
    }

    async fn overrun(&self, install_id: Uuid) {
        let count = {
            let mut g = self.running.lock();
            match g.get_mut(&install_id) {
                Some(r) => {
                    r.overruns += 1;
                    r.last_used = Instant::now();
                    r.overruns
                }
                None => return,
            }
        };
        self.events.emit(AppEvent::Overrun {
            install: install_id,
            count,
        });
        if count >= self.cfg.max_overruns {
            // The next call starts a fresh container.
            let _ = self.stop_with(install_id, StopReason::Overruns).await;
        }
    }

    async fn stop_with(&self, install_id: Uuid, reason: StopReason) -> Result<(), AppError> {
        let was_running = self.running.lock().remove(&install_id).is_some();
        self.launcher.stop(install_id).await?;
        self.revoke(install_id).await?;
        if was_running {
            self.events.emit(AppEvent::Stopped {
                install: install_id,
                reason,
            });
        }
        Ok(())
    }

    async fn revoke(&self, install_id: Uuid) -> Result<u64, AppError> {
        let c = self.store.conn().await?;
        Ok(hive_store::revoke_install_credentials(&c, install_id).await?)
    }
}

struct Answer {
    status: u16,
    body: Vec<u8>,
}

/// One HTTP/1.1 exchange over a connection the transport opened. The whole
/// exchange is one future, so a caller's `timeout` dropping it closes the
/// connection: that is the deadline's teeth.
async fn send(io: Io, path: &str, body: Option<Vec<u8>>) -> Result<Answer, String> {
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .map_err(|e| format!("handshake: {e}"))?;
    let conn = tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = match body {
        Some(b) => http::Request::builder()
            .method(http::Method::POST)
            .uri(path)
            .header(http::header::HOST, "app")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(b))),
        None => http::Request::builder()
            .method(http::Method::GET)
            .uri(path)
            .header(http::header::HOST, "app")
            .body(Full::new(Bytes::new())),
    }
    .map_err(|e| format!("request: {e}"))?;
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| format!("send: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("read answer: {e}"))?
        .to_bytes()
        .to_vec();
    conn.abort();
    Ok(Answer { status, body })
}

/// Production transport: the install's socket under its directory.
#[cfg(unix)]
pub struct UnixTransport;

#[cfg(unix)]
#[async_trait]
impl Transport for UnixTransport {
    async fn connect(&self, install_id: Uuid, socket_dir: &str) -> Result<Io, AppError> {
        let path = format!("{socket_dir}/app.sock");
        let s = tokio::net::UnixStream::connect(&path)
            .await
            .map_err(|e| AppError::Connect(install_id, format!("{path}: {e}")))?;
        Ok(Box::new(s))
    }
}
