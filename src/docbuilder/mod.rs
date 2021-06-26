mod crates;
mod limits;
mod queue;
mod rustwide_builder;

pub(crate) use self::limits::Limits;
pub(crate) use self::rustwide_builder::{BuildResult, DocCoverage};
pub use self::rustwide_builder::{PackageKind, RustwideBuilder};

use crate::error::Result;
use crate::BuildQueue;
use std::fs;
use std::sync::Arc;

/// chroot based documentation builder
pub struct DocBuilder {
    pub(crate) build_queue: Arc<BuildQueue>,
}

impl DocBuilder {
    pub fn new(build_queue: Arc<BuildQueue>) -> DocBuilder {
        DocBuilder { build_queue }
    }

    /// Creates a lock file. Daemon will check this lock file and stop operating if its exists.
    pub fn lock(&self) -> Result<()> {
        let path = self.build_queue.lock_path();
        if !path.exists() {
            fs::OpenOptions::new().write(true).create(true).open(path)?;
        }

        Ok(())
    }

    /// Removes lock file.
    pub fn unlock(&self) -> Result<()> {
        let path = self.build_queue.lock_path();
        if path.exists() {
            fs::remove_file(path)?;
        }

        Ok(())
    }
}
