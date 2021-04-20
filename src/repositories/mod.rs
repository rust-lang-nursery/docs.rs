pub use self::github::GitHub;
pub use self::gitlab::GitLab;
pub(crate) use self::updater::RepositoryName;
pub use self::updater::{
    FetchRepositoriesResult, Repository, RepositoryForge, RepositoryStatsUpdater,
};

pub const APP_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    " ",
    include_str!(concat!(env!("OUT_DIR"), "/git_version"))
);

#[derive(Debug, failure::Fail)]
#[fail(display = "rate limit reached")]
struct RateLimitReached;

mod github;
mod gitlab;
mod updater;
