use crate::web::page::TemplateData;
use crate::{
    db::Pool, repositories::RepositoryStatsUpdater, BuildQueue, Config, Context, Metrics, Storage,
};
use failure::Error;
use iron::{BeforeMiddleware, IronResult, Request};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(super) struct InjectExtensions {
    build_queue: Arc<BuildQueue>,
    pool: Pool,
    config: Arc<Config>,
    storage: Arc<Storage>,
    metrics: Arc<Metrics>,
    template_data: Arc<TemplateData>,
    repository_stats_updater: Arc<RepositoryStatsUpdater>,
}

impl InjectExtensions {
    pub(super) fn new(
        context: &dyn Context,
        template_data: Arc<TemplateData>,
    ) -> Result<Self, Error> {
        Ok(Self {
            build_queue: context.build_queue()?,
            pool: context.pool()?,
            config: context.config()?,
            storage: context.storage()?,
            metrics: context.metrics()?,
            repository_stats_updater: context.repository_stats_updater()?,
            template_data,
        })
    }
}

impl BeforeMiddleware for InjectExtensions {
    fn before(&self, req: &mut Request) -> IronResult<()> {
        req.extensions
            .insert::<BuildQueue>(self.build_queue.clone());
        req.extensions.insert::<Pool>(self.pool.clone());
        req.extensions.insert::<Config>(self.config.clone());
        req.extensions.insert::<Storage>(self.storage.clone());
        req.extensions.insert::<Metrics>(self.metrics.clone());
        req.extensions
            .insert::<TemplateData>(self.template_data.clone());
        req.extensions
            .insert::<RepositoryStatsUpdater>(self.repository_stats_updater.clone());

        Ok(())
    }
}

macro_rules! key {
    ($key:ty => $value:ty) => {
        impl iron::typemap::Key for $key {
            type Value = $value;
        }
    };
}

key!(BuildQueue => Arc<BuildQueue>);
key!(Pool => Pool);
key!(Config => Arc<Config>);
key!(Storage => Arc<Storage>);
key!(Metrics => Arc<Metrics>);
key!(TemplateData => Arc<TemplateData>);
key!(RepositoryStatsUpdater => Arc<RepositoryStatsUpdater>);
