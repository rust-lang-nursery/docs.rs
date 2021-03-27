use std::env;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;

use docs_rs::db::{self, add_path_into_database, Pool, PoolClient};
use docs_rs::utils::{remove_crate_priority, set_crate_priority};
use docs_rs::{
    BuildQueue, Config, Context, DocBuilder, Index, Metrics, PackageKind, RustwideBuilder, Server,
    Storage,
};
use failure::{err_msg, Error, ResultExt};
use once_cell::sync::OnceCell;
use sentry::integrations::failure::capture_error;
use sentry::integrations::log::LogIntegration;
use structopt::StructOpt;
use strum::VariantNames;

pub fn main() {
    let _ = dotenv::dotenv();
    //rustwide::logging::init_with(logger_init());
    let sentry_log = LogIntegration::default().with_env_logger_dest(Some(logger_init()));
    let _guard = sentry::init((
        "TODO: fill this in with the secret from https://sentry.io/organizations/rust-lang/",
        sentry::ClientOptions {
            release: Some(docs_rs::BUILD_VERSION.into()),
            ..Default::default()
        }
        .add_integration(sentry_log),
    ));
    log::info!("this is a log from the main thread");
    log::error!("this is an error from the main thread");

    if let Err(err) = CommandLine::from_args().handle_args() {
        capture_error(&err);
        let mut msg = format!("Error: {}", err);
        for cause in err.iter_causes() {
            write!(msg, "\n\nCaused by:\n    {}", cause).unwrap();
        }
        eprintln!("{}", msg);
        if !err.backtrace().is_empty() {
            eprintln!("\nStack backtrace:\n{}", err.backtrace());
        }
        std::process::exit(1);
    }
}

// NOTE: this function should be idempotent
fn logger_init() -> env_logger::Logger {
    use std::io::Write;

    let env = env_logger::Env::default().filter_or("DOCSRS_LOG", "docs_rs=info");
    env_logger::from_env(env)
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}] {}: {}",
                time::now().strftime("%Y/%m/%d %H:%M:%S").unwrap(),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .build()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::EnumVariantNames)]
#[strum(serialize_all = "snake_case")]
enum Toggle {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, StructOpt)]
#[structopt(
    name = "cratesfyi",
    about = env!("CARGO_PKG_DESCRIPTION"),
    version = docs_rs::BUILD_VERSION,
    rename_all = "kebab-case",
)]
enum CommandLine {
    Build(Build),

    /// Starts web server
    StartWebServer {
        #[structopt(name = "SOCKET_ADDR", default_value = "0.0.0.0:3000")]
        socket_addr: String,

        /// Reload templates when they're changed
        #[structopt(long = "reload-templates")]
        reload_templates: bool,
    },

    /// Starts cratesfyi daemon
    Daemon {
        /// Deprecated. Run the server in the foreground instead of detaching a child
        #[structopt(name = "FOREGROUND", short = "f", long = "foreground")]
        foreground: bool,

        /// Enable or disable the registry watcher to automatically enqueue newly published crates
        #[structopt(
            long = "registry-watcher",
            default_value = "enabled",
            possible_values(Toggle::VARIANTS)
        )]
        registry_watcher: Toggle,
    },

    /// Database operations
    Database {
        #[structopt(subcommand)]
        subcommand: DatabaseSubcommand,
    },

    /// Interactions with the build queue
    Queue {
        #[structopt(subcommand)]
        subcommand: QueueSubcommand,
    },
}

impl CommandLine {
    pub fn handle_args(self) -> Result<(), Error> {
        let ctx = BinContext::new();

        match self {
            Self::Build(build) => build.handle_args(ctx)?,
            Self::StartWebServer {
                socket_addr,
                reload_templates,
            } => {
                // Blocks indefinitely
                let _ = Server::start(Some(&socket_addr), reload_templates, &ctx)?;
            }
            Self::Daemon {
                foreground,
                registry_watcher,
            } => {
                if foreground {
                    log::warn!("--foreground was passed, but there is no need for it anymore");
                }

                docs_rs::utils::start_daemon(&ctx, registry_watcher == Toggle::Enabled)?;
            }
            Self::Database { subcommand } => subcommand.handle_args(ctx)?,
            Self::Queue { subcommand } => subcommand.handle_args(ctx)?,
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, StructOpt)]
enum QueueSubcommand {
    /// Add a crate to the build queue
    Add {
        /// Name of crate to build
        #[structopt(name = "CRATE_NAME")]
        crate_name: String,
        /// Version of crate to build
        #[structopt(name = "CRATE_VERSION")]
        crate_version: String,
        /// Priority of build (new crate builds get priority 0)
        #[structopt(
            name = "BUILD_PRIORITY",
            short = "p",
            long = "priority",
            default_value = "5"
        )]
        build_priority: i32,
    },

    /// Interactions with build queue priorities
    DefaultPriority {
        #[structopt(subcommand)]
        subcommand: PrioritySubcommand,
    },
}

impl QueueSubcommand {
    pub fn handle_args(self, ctx: BinContext) -> Result<(), Error> {
        match self {
            Self::Add {
                crate_name,
                crate_version,
                build_priority,
            } => ctx.build_queue()?.add_crate(
                &crate_name,
                &crate_version,
                build_priority,
                ctx.config()?.registry_url.as_deref(),
            )?,

            Self::DefaultPriority { subcommand } => subcommand.handle_args(ctx)?,
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, StructOpt)]
enum PrioritySubcommand {
    /// Set all crates matching a pattern to a priority level
    Set {
        /// See https://www.postgresql.org/docs/current/functions-matching.html for pattern syntax
        #[structopt(name = "PATTERN")]
        pattern: String,
        /// The priority to give crates matching the given `PATTERN`
        priority: i32,
    },

    /// Remove the prioritization of crates for a pattern
    Remove {
        /// See https://www.postgresql.org/docs/current/functions-matching.html for pattern syntax
        #[structopt(name = "PATTERN")]
        pattern: String,
    },
}

impl PrioritySubcommand {
    pub fn handle_args(self, ctx: BinContext) -> Result<(), Error> {
        match self {
            Self::Set { pattern, priority } => {
                set_crate_priority(&mut *ctx.conn()?, &pattern, priority)
                    .context("Could not set pattern's priority")?;
            }

            Self::Remove { pattern } => {
                if let Some(priority) = remove_crate_priority(&mut *ctx.conn()?, &pattern)
                    .context("Could not remove pattern's priority")?
                {
                    println!("Removed pattern with priority {}", priority);
                } else {
                    println!("Pattern did not exist and so was not removed");
                }
            }
        }
        Ok(())
    }
}

/// Builds documentation in a chroot environment
#[derive(Debug, Clone, PartialEq, Eq, StructOpt)]
#[structopt(rename_all = "kebab-case")]
struct Build {
    /// Skips building documentation if documentation exists
    #[structopt(name = "SKIP_IF_EXISTS", short = "s", long = "skip")]
    skip_if_exists: bool,

    #[structopt(subcommand)]
    subcommand: BuildSubcommand,
}

impl Build {
    pub fn handle_args(self, ctx: BinContext) -> Result<(), Error> {
        self.subcommand.handle_args(ctx, self.skip_if_exists)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, StructOpt)]
enum BuildSubcommand {
    /// Builds documentation of every crate
    World,

    /// Builds documentation for a crate
    Crate {
        /// Crate name
        #[structopt(
            name = "CRATE_NAME",
            required_unless("local"),
            requires("CRATE_VERSION")
        )]
        crate_name: Option<String>,

        /// Version of crate
        #[structopt(name = "CRATE_VERSION")]
        crate_version: Option<String>,

        /// Build a crate at a specific path
        #[structopt(short = "l", long = "local", conflicts_with_all(&["CRATE_NAME", "CRATE_VERSION"]))]
        local: Option<PathBuf>,
    },

    /// update the currently installed rustup toolchain
    UpdateToolchain {
        /// Update the toolchain only if no toolchain is currently installed
        #[structopt(name = "ONLY_FIRST_TIME", long = "only-first-time")]
        only_first_time: bool,
    },

    /// Adds essential files for the installed version of rustc
    AddEssentialFiles,

    /// Locks cratesfyi daemon to stop building new crates
    Lock,

    /// Unlocks cratesfyi daemon to continue building new crates
    Unlock,
}

impl BuildSubcommand {
    pub fn handle_args(self, ctx: BinContext, skip_if_exists: bool) -> Result<(), Error> {
        let docbuilder = DocBuilder::new(ctx.config()?, ctx.pool()?, ctx.build_queue()?);

        let rustwide_builder = || -> Result<RustwideBuilder, Error> {
            let mut builder = RustwideBuilder::init(&ctx)?;
            builder.set_skip_build_if_exists(skip_if_exists);
            Ok(builder)
        };

        match self {
            Self::World => {
                rustwide_builder()?
                    .build_world()
                    .context("Failed to build world")?;
            }

            Self::Crate {
                crate_name,
                crate_version,
                local,
            } => {
                let mut builder = rustwide_builder()?;

                if let Some(path) = local {
                    builder
                        .build_local_package(&path)
                        .context("Building documentation failed")?;
                } else {
                    let registry_url = ctx.config()?.registry_url.clone();
                    builder
                        .build_package(
                            &crate_name.ok_or_else(|| err_msg("must specify name if not local"))?,
                            &crate_version
                                .ok_or_else(|| err_msg("must specify version if not local"))?,
                            registry_url
                                .as_ref()
                                .map(|s| PackageKind::Registry(s.as_str()))
                                .unwrap_or(PackageKind::CratesIo),
                        )
                        .context("Building documentation failed")?;
                }
            }

            Self::UpdateToolchain { only_first_time } => {
                if only_first_time {
                    let mut conn = ctx
                        .pool()?
                        .get()
                        .context("failed to get a database connection")?;
                    let res =
                        conn.query("SELECT * FROM config WHERE name = 'rustc_version';", &[])?;

                    if !res.is_empty() {
                        println!("update-toolchain was already called in the past, exiting");
                        return Ok(());
                    }
                }

                rustwide_builder()?
                    .update_toolchain()
                    .context("failed to update toolchain")?;
            }

            Self::AddEssentialFiles => {
                rustwide_builder()?
                    .add_essential_files()
                    .context("failed to add essential files")?;
            }

            Self::Lock => docbuilder.lock().context("Failed to lock")?,
            Self::Unlock => docbuilder.unlock().context("Failed to unlock")?,
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, StructOpt)]
enum DatabaseSubcommand {
    /// Run database migration
    Migrate {
        /// The database version to migrate to
        #[structopt(name = "VERSION")]
        version: Option<i64>,
    },

    /// Updates github stats for crates.
    UpdateGithubFields,

    /// Backfill GitHub stats for crates.
    BackfillGithubStats,

    /// Updates info for a crate from the registry's API
    UpdateCrateRegistryFields {
        #[structopt(name = "CRATE")]
        name: String,
    },

    AddDirectory {
        /// Path of file or directory
        #[structopt(name = "DIRECTORY")]
        directory: PathBuf,
        /// Prefix of files in database
        #[structopt(name = "PREFIX", env = "CRATESFYI_PREFIX")]
        prefix: String,
    },

    /// Remove documentation from the database
    Delete {
        #[structopt(subcommand)]
        command: DeleteSubcommand,
    },

    /// Blacklist operations
    Blacklist {
        #[structopt(subcommand)]
        command: BlacklistSubcommand,
    },

    /// Compares the database with the index and resolves inconsistencies
    #[cfg(feature = "consistency_check")]
    Synchronize {
        /// Don't actually resolve the inconsistencies, just log them
        #[structopt(long)]
        dry_run: bool,
    },
}

impl DatabaseSubcommand {
    pub fn handle_args(self, ctx: BinContext) -> Result<(), Error> {
        match self {
            Self::Migrate { version } => {
                db::migrate(version, &mut *ctx.conn()?)
                    .context("Failed to run database migrations")?;
            }

            Self::UpdateGithubFields => {
                docs_rs::utils::GithubUpdater::new(ctx.config()?, ctx.pool()?)?
                    .ok_or_else(|| failure::format_err!("missing GitHub token"))?
                    .update_all_crates()?;
            }

            Self::BackfillGithubStats => {
                docs_rs::utils::GithubUpdater::new(ctx.config()?, ctx.pool()?)?
                    .ok_or_else(|| failure::format_err!("missing GitHub token"))?
                    .backfill_repositories()?;
            }

            Self::UpdateCrateRegistryFields { name } => {
                let index = ctx.index()?;

                db::update_crate_data_in_database(
                    &mut *ctx.conn()?,
                    &name,
                    &index.api().get_crate_data(&name)?,
                )?;
            }

            Self::AddDirectory { directory, prefix } => {
                add_path_into_database(&*ctx.storage()?, &prefix, directory)
                    .context("Failed to add directory into database")?;
            }

            Self::Delete {
                command: DeleteSubcommand::Version { name, version },
            } => db::delete_version(&mut *ctx.conn()?, &*ctx.storage()?, &name, &version)
                .context("failed to delete the crate")?,
            Self::Delete {
                command: DeleteSubcommand::Crate { name },
            } => db::delete_crate(&mut *ctx.conn()?, &*ctx.storage()?, &name)
                .context("failed to delete the crate")?,
            Self::Blacklist { command } => command.handle_args(ctx)?,

            #[cfg(feature = "consistency_check")]
            Self::Synchronize { dry_run } => {
                docs_rs::utils::consistency::run_check(&mut *ctx.conn()?, &*ctx.index()?, dry_run)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, StructOpt)]
enum BlacklistSubcommand {
    /// List all crates on the blacklist
    List,

    /// Add a crate to the blacklist
    Add {
        /// Crate name
        #[structopt(name = "CRATE_NAME")]
        crate_name: String,
    },

    /// Remove a crate from the blacklist
    Remove {
        /// Crate name
        #[structopt(name = "CRATE_NAME")]
        crate_name: String,
    },
}

impl BlacklistSubcommand {
    fn handle_args(self, ctx: BinContext) -> Result<(), Error> {
        let mut conn = &mut *ctx.conn()?;
        match self {
            Self::List => {
                let crates = db::blacklist::list_crates(&mut conn)
                    .context("failed to list crates on blacklist")?;

                println!("{}", crates.join("\n"));
            }

            Self::Add { crate_name } => db::blacklist::add_crate(&mut conn, &crate_name)
                .context("failed to add crate to blacklist")?,

            Self::Remove { crate_name } => db::blacklist::remove_crate(&mut conn, &crate_name)
                .context("failed to remove crate from blacklist")?,
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, StructOpt)]
enum DeleteSubcommand {
    /// Delete a whole crate
    Crate {
        /// Name of the crate to delete
        #[structopt(name = "CRATE_NAME")]
        name: String,
    },
    /// Delete a single version of a crate (which may include multiple builds)
    Version {
        /// Name of the crate to delete
        #[structopt(name = "CRATE_NAME")]
        name: String,

        /// The version of the crate to delete
        #[structopt(name = "VERSION")]
        version: String,
    },
}

struct BinContext {
    build_queue: OnceCell<Arc<BuildQueue>>,
    storage: OnceCell<Arc<Storage>>,
    config: OnceCell<Arc<Config>>,
    pool: OnceCell<Pool>,
    metrics: OnceCell<Arc<Metrics>>,
    index: OnceCell<Arc<Index>>,
}

impl BinContext {
    fn new() -> Self {
        Self {
            build_queue: OnceCell::new(),
            storage: OnceCell::new(),
            config: OnceCell::new(),
            pool: OnceCell::new(),
            metrics: OnceCell::new(),
            index: OnceCell::new(),
        }
    }

    fn conn(&self) -> Result<PoolClient, Error> {
        Ok(self.pool()?.get()?)
    }
}

impl Context for BinContext {
    fn build_queue(&self) -> Result<Arc<BuildQueue>, Error> {
        Ok(self
            .build_queue
            .get_or_try_init::<_, Error>(|| {
                Ok(Arc::new(BuildQueue::new(
                    self.pool()?,
                    self.metrics()?,
                    &*self.config()?,
                )))
            })?
            .clone())
    }

    fn storage(&self) -> Result<Arc<Storage>, Error> {
        Ok(self
            .storage
            .get_or_try_init::<_, Error>(|| {
                Ok(Arc::new(Storage::new(
                    self.pool()?,
                    self.metrics()?,
                    &*self.config()?,
                )?))
            })?
            .clone())
    }

    fn config(&self) -> Result<Arc<Config>, Error> {
        Ok(self
            .config
            .get_or_try_init::<_, Error>(|| Ok(Arc::new(Config::from_env()?)))?
            .clone())
    }

    fn pool(&self) -> Result<Pool, Error> {
        Ok(self
            .pool
            .get_or_try_init::<_, Error>(|| Ok(Pool::new(&*self.config()?, self.metrics()?)?))?
            .clone())
    }

    fn metrics(&self) -> Result<Arc<Metrics>, Error> {
        Ok(self
            .metrics
            .get_or_try_init::<_, Error>(|| Ok(Arc::new(Metrics::new()?)))?
            .clone())
    }

    fn index(&self) -> Result<Arc<Index>, Error> {
        Ok(self
            .index
            .get_or_try_init::<_, Error>(|| {
                let config = self.config()?;
                Ok(Arc::new(
                    if let Some(registry_url) = config.registry_url.clone() {
                        Index::from_url(config.registry_index_path.clone(), registry_url)
                    } else {
                        Index::new(config.registry_index_path.clone())
                    }?,
                ))
            })?
            .clone())
    }
}
