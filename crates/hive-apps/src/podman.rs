//! The Podman launcher for app containers (D53 §1, #163 slice 4): carries
//! out what [`crate::Apps`] decided. It shells out to the podman CLI like the
//! harness launcher does, for the same reasons (one contract on the Linux
//! host and a dev box's machine VM, no native dependency), and the argument
//! list is `hive_harness::podman_app_args`, which the harness's tests assert
//! on, so nothing about the sandbox is decided here.

use std::path::Path;

use async_trait::async_trait;
use hive_harness::{AppSpec, is_no_such_container, podman_app_args};
use tokio::process::Command;
use uuid::Uuid;

use crate::{AppError, AppLauncher};

/// `podman run --detach` for start, `podman rm --force` for stop.
#[derive(Clone, Debug, Default)]
pub struct PodmanAppLauncher {
    /// The binary; empty means `podman`.
    pub binary: String,
    /// Extra `podman run` arguments the operator wants on every container
    /// (a `--log-driver`, say). Never a way to widen the sandbox: they are
    /// appended before the image and the spec's denies come first.
    pub extra_args: Vec<String>,
}

impl PodmanAppLauncher {
    fn binary(&self) -> &str {
        if self.binary.is_empty() {
            "podman"
        } else {
            &self.binary
        }
    }

    async fn podman(&self, args: &[String]) -> Result<std::process::Output, AppError> {
        Command::new(self.binary())
            .args(args)
            .output()
            .await
            .map_err(|e| AppError::Launcher(format!("{} {}: {e}", self.binary(), args.join(" "))))
    }

    /// Removes a container of this name if one exists: a previous daemon's,
    /// or one whose stop never completed. Nothing is running for the install
    /// afterwards, whatever was before.
    async fn remove(&self, name: &str) -> Result<(), AppError> {
        let out = self
            .podman(&[
                "rm".into(),
                "--force".into(),
                "--time".into(),
                "10".into(),
                name.to_string(),
            ])
            .await?;
        if out.status.success() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&out.stderr).to_string()
            + &String::from_utf8_lossy(&out.stdout);
        if is_no_such_container(&text) {
            return Ok(());
        }
        Err(AppError::Launcher(format!(
            "podman rm {name}: {}: {}",
            out.status,
            text.trim()
        )))
    }
}

#[async_trait]
impl AppLauncher for PodmanAppLauncher {
    async fn start(&self, spec: &AppSpec) -> Result<(), AppError> {
        let args = podman_app_args(spec, &self.extra_args)?;
        // The install's directory, for the one socket. Created here because
        // the host owns it and the container only writes the socket into it;
        // a stale socket from a previous container is removed so the app's
        // bind does not fail on it.
        let dir = Path::new(&spec.socket_dir);
        std::fs::create_dir_all(dir)
            .map_err(|e| AppError::Launcher(format!("socket dir {}: {e}", dir.display())))?;
        let sock = dir.join("app.sock");
        if sock.exists() {
            let _ = std::fs::remove_file(&sock);
        }
        self.remove(&spec.container_name()).await?;
        let out = self.podman(&args).await?;
        if !out.status.success() {
            return Err(AppError::Launcher(format!(
                "podman run {}: {}: {}",
                spec.container_name(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    async fn stop(&self, install_id: Uuid) -> Result<(), AppError> {
        // The same name the spec would give it; a stop needs no spec.
        self.remove(&format!("hive-sandbox-app-{install_id}")).await
    }
}
