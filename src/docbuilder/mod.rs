mod crates;
mod limits;
mod queue;
mod rustwide_builder;

pub(crate) use self::limits::Limits;
pub(crate) use self::rustwide_builder::{BuildResult, DocCoverage};
pub use self::rustwide_builder::{PackageKind, RustwideBuilder};

use crate::db::Pool;
use crate::error::Result;
use crate::{BuildQueue, Config};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

/// chroot based documentation builder
pub struct DocBuilder {
    config: Arc<Config>,
    db: Pool,
    build_queue: Arc<BuildQueue>,
}

impl DocBuilder {
    pub fn new(config: Arc<Config>, db: Pool, build_queue: Arc<BuildQueue>) -> DocBuilder {
        DocBuilder {
            config,
            db,
            build_queue,
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.config.prefix.join("docsrs.lock")
    }

    /// Creates a lock file. Daemon will check this lock file and stop operating if its exists.
    pub fn lock(&self) -> Result<()> {
        let path = self.lock_path();
        if !path.exists() {
            fs::OpenOptions::new().write(true).create(true).open(path)?;
        }

        Ok(())
    }

    /// Removes lock file.
    pub fn unlock(&self) -> Result<()> {
        let path = self.lock_path();
        if path.exists() {
            fs::remove_file(path)?;
        }

        Ok(())
    }

    /// Checks for the lock file and returns whether it currently exists.
    pub fn is_locked(&self) -> bool {
        self.lock_path().exists()
    }
}
