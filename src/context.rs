use crate::db::Pool;
use crate::repositories::RepositoryStatsUpdater;
use crate::{BuildQueue, Config, Index, Metrics, Storage};
use failure::Error;
use std::sync::Arc;

pub trait Context {
    fn config(&self) -> Result<Arc<Config>, Error>;
    fn build_queue(&self) -> Result<Arc<BuildQueue>, Error>;
    fn storage(&self) -> Result<Arc<Storage>, Error>;
    fn pool(&self) -> Result<Pool, Error>;
    fn metrics(&self) -> Result<Arc<Metrics>, Error>;
    fn index(&self) -> Result<Arc<Index>, Error>;
    fn repository_stats_updater(&self) -> Result<Arc<RepositoryStatsUpdater>, Error>;
}
