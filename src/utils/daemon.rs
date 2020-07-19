//! Simple daemon
//!
//! This daemon will start web server, track new packages and build them

use crate::{
    db::Pool,
    storage::Storage,
    utils::{queue_builder, update_release_activity, GithubUpdater},
    BuildQueue, Config, DocBuilder, DocBuilderOptions,
};
use chrono::{Timelike, Utc};
use failure::Error;
use log::{debug, error, info};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::{env, thread};

fn start_registry_watcher(pool: Pool, build_queue: Arc<BuildQueue>) -> Result<(), Error> {
    thread::Builder::new()
        .name("registry index reader".to_string())
        .spawn(move || {
            // space this out to prevent it from clashing against the queue-builder thread on launch
            thread::sleep(Duration::from_secs(30));
            loop {
                let opts = opts();
                let mut doc_builder = DocBuilder::new(opts, pool.clone(), build_queue.clone());

                if doc_builder.is_locked() {
                    debug!("Lock file exists, skipping checking new crates");
                } else {
                    debug!("Checking new crates");
                    match doc_builder.get_new_crates() {
                        Ok(n) => debug!("{} crates added to queue", n),
                        Err(e) => error!("Failed to get new crates: {}", e),
                    }
                }

                thread::sleep(Duration::from_secs(60));
            }
        })?;

    Ok(())
}

pub fn start_daemon(
    config: Arc<Config>,
    db: Pool,
    build_queue: Arc<BuildQueue>,
    storage: Arc<Storage>,
    enable_registry_watcher: bool,
) -> Result<(), Error> {
    const CRATE_VARIABLES: &[&str] = &["CRATESFYI_PREFIX"];

    // first check required environment variables
    for v in CRATE_VARIABLES.iter() {
        if env::var(v).is_err() {
            panic!("Environment variable {} not found", v)
        }
    }

    let dbopts = opts();

    // check paths once
    dbopts.check_paths().unwrap();

    if enable_registry_watcher {
        // check new crates every minute
        start_registry_watcher(db.clone(), build_queue.clone())?;
    }

    // build new crates every minute
    let cloned_db = db.clone();
    let cloned_build_queue = build_queue.clone();
    let cloned_storage = storage.clone();
    thread::Builder::new()
        .name("build queue reader".to_string())
        .spawn(move || {
            let doc_builder =
                DocBuilder::new(opts(), cloned_db.clone(), cloned_build_queue.clone());
            queue_builder(doc_builder, cloned_db, cloned_build_queue, cloned_storage).unwrap();
        })
        .unwrap();

    // update release activity everyday at 23:55
    let cloned_db = db.clone();
    cron(
        "release activity updater",
        Duration::from_secs(60),
        move || {
            let now = Utc::now();
            if now.hour() == 23 && now.minute() == 55 {
                info!("Updating release activity");
                update_release_activity(&*cloned_db.get()?)?;
            }
            Ok(())
        },
    )?;

    // update github stats every hour
    let github_updater = GithubUpdater::new(&config, db.clone())?;
    cron(
        "github stats updater",
        Duration::from_secs(60 * 60),
        move || {
            github_updater.update_all_crates()?;
            Ok(())
        },
    )?;

    // TODO: update ssl certificate every 3 months

    // at least start web server
    info!("Starting web server");

    crate::Server::start(None, false, db, config, build_queue, storage)?;
    Ok(())
}

fn cron<F>(name: &'static str, interval: Duration, exec: F) -> Result<(), Error>
where
    F: Fn() -> Result<(), Error> + Send + 'static,
{
    thread::Builder::new()
        .name(name.into())
        .spawn(move || loop {
            thread::sleep(interval);
            if let Err(err) = exec() {
                error!("failed to run scheduled task '{}': {:?}", name, err);
            }
        })?;
    Ok(())
}

fn opts() -> DocBuilderOptions {
    let prefix = PathBuf::from(
        env::var("CRATESFYI_PREFIX").expect("CRATESFYI_PREFIX environment variable not found"),
    );
    DocBuilderOptions::from_prefix(prefix)
}
