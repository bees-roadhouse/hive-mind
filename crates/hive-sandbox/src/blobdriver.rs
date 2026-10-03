//! How the operator points the daemon at its blob root (D11: the driver is
//! chosen at config time, not per call; D40: there is one driver, disk, and
//! replication is the mount's job).
//!
//! The root is a path. Whether that path is a local disk, a ZFS dataset or a
//! JuiceFS mount is not the daemon's concern, which is exactly the property
//! the seam exists to keep: nothing above it changes when the mount does.

use hive_blob::{BlobError, DiskDriver, Driver};

#[derive(Clone, Debug)]
pub struct BlobConfig {
    /// Where `<hh>/<sha256>` lands, and the uploads spool beside it.
    pub root: String,
}

/// Builds the configured driver.
pub async fn blob_driver(cfg: &BlobConfig) -> Result<Box<dyn Driver>, BlobError> {
    if cfg.root.trim().is_empty() {
        return Err(BlobError::Invalid("blob root is empty".into()));
    }
    Ok(Box::new(DiskDriver::new(&cfg.root).await?))
}
