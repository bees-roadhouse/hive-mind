//! Where an install's image comes from (D53 §1: "the registry builds it into
//! a digest-pinned image ... or pulls a reference the manifest names"). This
//! is the second path, the one that exists before the build pipeline does:
//! the manifest's `process.image` names a digest-pinned reference, the
//! manifest validated it at install, and this provider reads it back.
//!
//! The first path, building an image from `runtime` and the app's source, is
//! the registry's and comes with D53 step 2's first Bun app; it will be a
//! second `AppImages` beside this one, and `Apps` will not know which.

use async_trait::async_trait;
use hive_manifest::Manifest;
use hive_store::Store;
use uuid::Uuid;

use crate::{AppError, AppImage, AppImages};

impl AppImage {
    /// Splits `repository@sha256:<64 hex>`. The same rule the manifest
    /// applies to `process.image`, so a reference that reaches here parses.
    pub fn parse(reference: &str) -> Result<AppImage, String> {
        let Some((repo, digest)) = reference.rsplit_once('@') else {
            return Err(format!("{reference:?} has no digest; a tag is not a pin"));
        };
        if repo.is_empty() || repo.contains(char::is_whitespace) || repo.contains('@') {
            return Err(format!("{reference:?}: repository is not a repository"));
        }
        let Some(hex) = digest.strip_prefix("sha256:") else {
            return Err(format!("{reference:?}: digest is not sha256"));
        };
        if hex.len() != 64
            || !hex
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(format!(
                "{reference:?}: digest is not 64 lowercase hex digits"
            ));
        }
        Ok(AppImage {
            repository: repo.to_string(),
            digest: digest.to_string(),
        })
    }

    pub fn reference(&self) -> String {
        format!("{}@{}", self.repository, self.digest)
    }
}

/// Reads `process.image` from the install's build manifest.
pub struct ManifestImages {
    store: Store,
}

impl ManifestImages {
    pub fn new(store: Store) -> ManifestImages {
        ManifestImages { store }
    }
}

#[async_trait]
impl AppImages for ManifestImages {
    async fn image_for(&self, install_id: Uuid) -> Result<AppImage, AppError> {
        let c = self.store.conn().await?;
        let manifest: Option<serde_json::Value> = hive_db::query(
            "SELECT b.manifest FROM installs i JOIN app_builds b ON b.id = i.build_id WHERE i.id = ?1",
        )
        .bind(install_id)
        .fetch_scalar_optional(&c)
        .await
        .map_err(|e| AppError::Store(e.to_string()))?;
        let manifest = manifest.ok_or(AppError::NoInstall(install_id))?;
        let m: Manifest = serde_json::from_value(manifest)
            .map_err(|e| AppError::Store(format!("manifest of install {install_id}: {e}")))?;
        let reference = m.process.and_then(|p| p.image).ok_or_else(|| {
            AppError::Image(format!(
                "install {install_id} names no image; a built image is D53 step 2"
            ))
        })?;
        AppImage::parse(&reference).map_err(AppError::Image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_is_a_repository_and_a_sha256_digest() {
        let d = "a".repeat(64);
        let img = AppImage::parse(&format!("ghcr.io/x/y@sha256:{d}")).unwrap();
        assert_eq!(img.repository, "ghcr.io/x/y");
        assert_eq!(img.reference(), format!("ghcr.io/x/y@sha256:{d}"));
        for bad in [
            "ghcr.io/x/y:latest",
            &format!("@sha256:{d}"),
            &format!("ghcr.io/x/y@md5:{d}"),
            "ghcr.io/x/y@sha256:abc",
            &format!("ghcr.io/x/y@sha256:{}", "A".repeat(64)),
            &format!("ghcr.io/x y@sha256:{d}"),
        ] {
            assert!(AppImage::parse(bad).is_err(), "{bad:?} accepted");
        }
    }
}
