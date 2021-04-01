use crate::storage::StorageKind;
use failure::{bail, format_err, Error, Fail, ResultExt};
use rusoto_core::Region;
use std::env::VarError;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug)]
pub struct Config {
    pub prefix: PathBuf,
    pub registry_index_path: PathBuf,
    pub registry_url: Option<String>,

    // Database connection params
    pub(crate) database_url: String,
    pub(crate) max_pool_size: u32,
    pub(crate) min_pool_idle: u32,

    // Storage params
    pub(crate) storage_backend: StorageKind,

    // S3 params
    pub(crate) s3_bucket: String,
    pub(crate) s3_region: Region,
    pub(crate) s3_endpoint: Option<String>,
    #[cfg(test)]
    pub(crate) s3_bucket_is_temporary: bool,

    // Github authentication
    pub(crate) github_accesstoken: Option<String>,
    pub(crate) github_updater_min_rate_limit: u32,

    // Max size of the files served by the docs.rs frontend
    pub(crate) max_file_size: usize,
    pub(crate) max_file_size_html: usize,
    // The most memory that can be used to parse an HTML file
    pub(crate) max_parse_memory: usize,
    // Time between 'git gc --auto' calls in seconds
    pub(crate) registry_gc_interval: u64,

    // random crate search generates a number of random IDs to
    // efficiently find a random crate with > 100 GH stars.
    // The amount depends on the ratio of crates with >100 stars
    // to the count of all crates.
    // At the time of creating this setting, it is set to
    // `500` for a ratio of 7249 over 54k crates.
    // For unit-tests the number has to be higher.
    pub(crate) random_crate_search_view_size: u32,

    // where do we want to store the locally cached index files
    // for the remote archives?
    pub(crate) local_archive_cache_path: PathBuf,

    // Build params
    pub(crate) build_attempts: u16,
    pub(crate) rustwide_workspace: PathBuf,
    pub(crate) inside_docker: bool,
    pub(crate) local_docker_image: Option<String>,
    pub(crate) toolchain: String,
    pub(crate) build_cpu_limit: Option<u32>,
    pub(crate) include_default_targets: bool,
    pub(crate) disable_memory_limit: bool,
}

impl Config {
    pub fn from_env() -> Result<Self, Error> {
        let prefix: PathBuf = require_env("CRATESFYI_PREFIX")?;

        Ok(Self {
            build_attempts: env("DOCSRS_BUILD_ATTEMPTS", 5)?,

            prefix: prefix.clone(),
            registry_index_path: env("REGISTRY_INDEX_PATH", prefix.join("crates.io-index"))?,
            registry_url: maybe_env("REGISTRY_URL")?,

            database_url: require_env("CRATESFYI_DATABASE_URL")?,
            max_pool_size: env("DOCSRS_MAX_POOL_SIZE", 90)?,
            min_pool_idle: env("DOCSRS_MIN_POOL_IDLE", 10)?,

            storage_backend: env("DOCSRS_STORAGE_BACKEND", StorageKind::Database)?,

            s3_bucket: env("DOCSRS_S3_BUCKET", "rust-docs-rs".to_string())?,
            s3_region: env("S3_REGION", Region::UsWest1)?,
            s3_endpoint: maybe_env("S3_ENDPOINT")?,
            // DO NOT CONFIGURE THIS THROUGH AN ENVIRONMENT VARIABLE!
            // Accidentally turning this on outside of the test suite might cause data loss in the
            // production environment.
            #[cfg(test)]
            s3_bucket_is_temporary: false,

            github_accesstoken: maybe_env("CRATESFYI_GITHUB_ACCESSTOKEN")?,
            github_updater_min_rate_limit: env("DOCSRS_GITHUB_UPDATER_MIN_RATE_LIMIT", 2500)?,

            max_file_size: env("DOCSRS_MAX_FILE_SIZE", 50 * 1024 * 1024)?,
            max_file_size_html: env("DOCSRS_MAX_FILE_SIZE_HTML", 50 * 1024 * 1024)?,
            // LOL HTML only uses as much memory as the size of the start tag!
            // https://github.com/rust-lang/docs.rs/pull/930#issuecomment-667729380
            max_parse_memory: env("DOCSRS_MAX_PARSE_MEMORY", 5 * 1024 * 1024)?,
            registry_gc_interval: env("DOCSRS_REGISTRY_GC_INTERVAL", 60 * 60)?,

            random_crate_search_view_size: env("DOCSRS_RANDOM_CRATE_SEARCH_VIEW_SIZE", 500)?,

            rustwide_workspace: env("CRATESFYI_RUSTWIDE_WORKSPACE", PathBuf::from(".workspace"))?,
            local_archive_cache_path: env(
                "CRATESFYI_ARCHIVE_INDEX_CACHE_PATH",
                PathBuf::from(".archive_cache"),
            )?,
            inside_docker: env("DOCS_RS_DOCKER", false)?,
            local_docker_image: maybe_env("DOCS_RS_LOCAL_DOCKER_IMAGE")?,
            toolchain: env("CRATESFYI_TOOLCHAIN", "nightly".to_string())?,
            build_cpu_limit: maybe_env("DOCS_RS_BUILD_CPU_LIMIT")?,
            include_default_targets: env("DOCSRS_INCLUDE_DEFAULT_TARGETS", true)?,
            disable_memory_limit: env("DOCSRS_DISABLE_MEMORY_LIMIT", false)?,
        })
    }
}

fn env<T>(var: &str, default: T) -> Result<T, Error>
where
    T: FromStr,
    T::Err: Fail,
{
    Ok(maybe_env(var)?.unwrap_or(default))
}

fn require_env<T>(var: &str) -> Result<T, Error>
where
    T: FromStr,
    T::Err: Fail,
{
    maybe_env(var)?.ok_or_else(|| format_err!("configuration variable {} is missing", var))
}

fn maybe_env<T>(var: &str) -> Result<Option<T>, Error>
where
    T: FromStr,
    T::Err: Fail,
{
    match std::env::var(var) {
        Ok(content) => Ok(content
            .parse::<T>()
            .map(Some)
            .with_context(|_| format!("failed to parse configuration variable {}", var))?),
        Err(VarError::NotPresent) => {
            log::debug!("optional configuration variable {} is not set", var);
            Ok(None)
        }
        Err(VarError::NotUnicode(_)) => bail!("configuration variable {} is not UTF-8", var),
    }
}
