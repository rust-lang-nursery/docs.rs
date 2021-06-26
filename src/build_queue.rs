use crate::db::Pool;
use crate::docbuilder::PackageKind;
use crate::error::Result;
use crate::utils::get_crate_priority;
use crate::{Config, Index, Metrics, RustwideBuilder};

use crates_index_diff::ChangeKind;
use log::{debug, error};

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub(crate) struct QueuedCrate {
    #[serde(skip)]
    id: i32,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) priority: i32,
    pub(crate) registry: Option<String>,
}

#[derive(Debug)]
pub struct BuildQueue {
    config: Arc<Config>,
    pub(crate) db: Pool,
    metrics: Arc<Metrics>,
    max_attempts: i32,
}

impl BuildQueue {
    pub fn new(db: Pool, metrics: Arc<Metrics>, config: Arc<Config>) -> Self {
        BuildQueue {
            max_attempts: config.build_attempts.into(),
            config,
            db,
            metrics,
        }
    }

    pub fn add_crate(
        &self,
        name: &str,
        version: &str,
        priority: i32,
        registry: Option<&str>,
    ) -> Result<()> {
        self.db.get()?.execute(
            "INSERT INTO queue (name, version, priority, registry) VALUES ($1, $2, $3, $4);",
            &[&name, &version, &priority, &registry],
        )?;
        Ok(())
    }

    pub(crate) fn pending_count(&self) -> Result<usize> {
        let res = self.db.get()?.query(
            "SELECT COUNT(*) FROM queue WHERE attempt < $1;",
            &[&self.max_attempts],
        )?;
        Ok(res[0].get::<_, i64>(0) as usize)
    }

    pub(crate) fn prioritized_count(&self) -> Result<usize> {
        let res = self.db.get()?.query(
            "SELECT COUNT(*) FROM queue WHERE attempt < $1 AND priority <= 0;",
            &[&self.max_attempts],
        )?;
        Ok(res[0].get::<_, i64>(0) as usize)
    }

    pub(crate) fn failed_count(&self) -> Result<usize> {
        let res = self.db.get()?.query(
            "SELECT COUNT(*) FROM queue WHERE attempt >= $1;",
            &[&self.max_attempts],
        )?;
        Ok(res[0].get::<_, i64>(0) as usize)
    }

    pub(crate) fn queued_crates(&self) -> Result<Vec<QueuedCrate>> {
        let query = self.db.get()?.query(
            "SELECT id, name, version, priority, registry
             FROM queue
             WHERE attempt < $1
             ORDER BY priority ASC, attempt ASC, id ASC",
            &[&self.max_attempts],
        )?;

        Ok(query
            .into_iter()
            .map(|row| QueuedCrate {
                id: row.get("id"),
                name: row.get("name"),
                version: row.get("version"),
                priority: row.get("priority"),
                registry: row.get("registry"),
            })
            .collect())
    }

    pub(crate) fn process_next_crate(
        &self,
        f: impl FnOnce(&QueuedCrate) -> Result<()>,
    ) -> Result<()> {
        let mut conn = self.db.get()?;

        let queued = self.queued_crates()?;
        let to_process = match queued.get(0) {
            Some(krate) => krate,
            None => return Ok(()),
        };

        let res = f(to_process);
        self.metrics.total_builds.inc();
        match res {
            Ok(()) => {
                conn.execute("DELETE FROM queue WHERE id = $1;", &[&to_process.id])?;
            }
            Err(e) => {
                // Increase attempt count
                let rows = conn.query(
                    "UPDATE queue SET attempt = attempt + 1 WHERE id = $1 RETURNING attempt;",
                    &[&to_process.id],
                )?;
                let attempt: i32 = rows[0].get(0);

                if attempt >= self.max_attempts {
                    self.metrics.failed_builds.inc();
                }

                error!(
                    "Failed to build package {}-{} from queue: {}\nBacktrace: {}",
                    to_process.name,
                    to_process.version,
                    e,
                    e.backtrace()
                );
            }
        }

        Ok(())
    }
}

/// Locking functions.
impl BuildQueue {
    pub(crate) fn lock_path(&self) -> PathBuf {
        self.config.prefix.join("docsrs.lock")
    }

    /// Checks for the lock file and returns whether it currently exists.
    pub fn is_locked(&self) -> bool {
        self.lock_path().exists()
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
}

/// Index methods.
impl BuildQueue {
    /// Updates registry index repository and adds new crates into build queue.
    ///
    /// Returns the number of crates added
    pub fn get_new_crates(&self, index: &Index) -> Result<usize> {
        let mut conn = self.db.get()?;
        let diff = index.diff()?;
        let (mut changes, oid) = diff.peek_changes()?;
        let mut crates_added = 0;

        // I believe this will fix ordering of queue if we get more than one crate from changes
        changes.reverse();

        for krate in &changes {
            match krate.kind {
                ChangeKind::Yanked => {
                    let res = conn.execute(
                        "
                        UPDATE releases
                            SET yanked = TRUE
                        FROM crates
                        WHERE crates.id = releases.crate_id
                            AND name = $1
                            AND version = $2
                        ",
                        &[&krate.name, &krate.version],
                    );
                    match res {
                        Ok(_) => debug!("{}-{} yanked", krate.name, krate.version),
                        Err(err) => error!(
                            "error while setting {}-{} to yanked: {}",
                            krate.name, krate.version, err
                        ),
                    }
                }

                ChangeKind::Added => {
                    let priority = get_crate_priority(&mut conn, &krate.name)?;

                    match self.add_crate(
                        &krate.name,
                        &krate.version,
                        priority,
                        index.repository_url(),
                    ) {
                        Ok(()) => {
                            debug!("{}-{} added into build queue", krate.name, krate.version);
                            crates_added += 1;
                        }
                        Err(err) => error!(
                            "failed adding {}-{} into build queue: {}",
                            krate.name, krate.version, err
                        ),
                    }
                }
            }
        }

        diff.set_last_seen_reference(oid)?;

        Ok(crates_added)
    }

    /// Builds the top package from the queue. Returns whether there was a package in the queue.
    ///
    /// Note that this will return `Ok(true)` even if the package failed to build.
    pub(crate) fn build_next_queue_package(&self, builder: &mut RustwideBuilder) -> Result<bool> {
        let mut processed = false;
        self.process_next_crate(|krate| {
            processed = true;

            let kind = krate
                .registry
                .as_ref()
                .map(|r| PackageKind::Registry(r.as_str()))
                .unwrap_or(PackageKind::CratesIo);

            if let Err(err) = builder.update_toolchain() {
                log::error!("Updating toolchain failed, locking queue: {}", err);
                self.lock()?;
                return Err(err);
            }

            builder.build_package(&krate.name, &krate.version, kind)?;
            Ok(())
        })?;

        Ok(processed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_process_crates() {
        const MAX_ATTEMPTS: u16 = 3;

        crate::test::wrapper(|env| {
            env.override_config(|config| {
                config.build_attempts = MAX_ATTEMPTS;
            });

            let queue = env.build_queue();

            let test_crates = [
                ("low-priority", "1.0.0", 1000),
                ("high-priority-foo", "1.0.0", -1000),
                ("medium-priority", "1.0.0", -10),
                ("high-priority-bar", "1.0.0", -1000),
                ("standard-priority", "1.0.0", 0),
                ("high-priority-baz", "1.0.0", -1000),
            ];
            for krate in &test_crates {
                queue.add_crate(krate.0, krate.1, krate.2, None)?;
            }

            let assert_next = |name| -> Result<()> {
                queue.process_next_crate(|krate| {
                    assert_eq!(name, krate.name);
                    Ok(())
                })?;
                Ok(())
            };
            let assert_next_and_fail = |name| -> Result<()> {
                queue.process_next_crate(|krate| {
                    assert_eq!(name, krate.name);
                    failure::bail!("simulate a failure");
                })?;
                Ok(())
            };

            // The first processed item is the one with the highest priority added first.
            assert_next("high-priority-foo")?;

            // Simulate a failure in high-priority-bar.
            assert_next_and_fail("high-priority-bar")?;

            // Continue with the next high priority crate.
            assert_next("high-priority-baz")?;

            // After all the crates with the max priority are processed, before starting to process
            // crates with a lower priority the failed crates with the max priority will be tried
            // again.
            assert_next("high-priority-bar")?;

            // Continue processing according to the priority.
            assert_next("medium-priority")?;
            assert_next("standard-priority")?;

            // Simulate the crate failing many times.
            for _ in 0..MAX_ATTEMPTS {
                assert_next_and_fail("low-priority")?;
            }

            // Since low-priority failed many times it will be removed from the queue. Because of
            // that the queue should now be empty.
            let mut called = false;
            queue.process_next_crate(|_| {
                called = true;
                Ok(())
            })?;
            assert!(!called, "there were still items in the queue");

            // Ensure metrics were recorded correctly
            let metrics = env.metrics();
            assert_eq!(metrics.total_builds.get(), 9);
            assert_eq!(metrics.failed_builds.get(), 1);

            Ok(())
        })
    }

    #[test]
    fn test_pending_count() {
        crate::test::wrapper(|env| {
            let queue = env.build_queue();

            assert_eq!(queue.pending_count()?, 0);
            queue.add_crate("foo", "1.0.0", 0, None)?;
            assert_eq!(queue.pending_count()?, 1);
            queue.add_crate("bar", "1.0.0", 0, None)?;
            assert_eq!(queue.pending_count()?, 2);

            queue.process_next_crate(|krate| {
                assert_eq!("foo", krate.name);
                Ok(())
            })?;
            assert_eq!(queue.pending_count()?, 1);

            Ok(())
        });
    }

    #[test]
    fn test_prioritized_count() {
        crate::test::wrapper(|env| {
            let queue = env.build_queue();

            assert_eq!(queue.prioritized_count()?, 0);
            queue.add_crate("foo", "1.0.0", 0, None)?;
            assert_eq!(queue.prioritized_count()?, 1);
            queue.add_crate("bar", "1.0.0", -100, None)?;
            assert_eq!(queue.prioritized_count()?, 2);
            queue.add_crate("baz", "1.0.0", 100, None)?;
            assert_eq!(queue.prioritized_count()?, 2);

            queue.process_next_crate(|krate| {
                assert_eq!("bar", krate.name);
                Ok(())
            })?;
            assert_eq!(queue.prioritized_count()?, 1);

            Ok(())
        });
    }

    #[test]
    fn test_failed_count() {
        const MAX_ATTEMPTS: u16 = 3;
        crate::test::wrapper(|env| {
            env.override_config(|config| {
                config.build_attempts = MAX_ATTEMPTS;
            });
            let queue = env.build_queue();

            assert_eq!(queue.failed_count()?, 0);
            queue.add_crate("foo", "1.0.0", -100, None)?;
            assert_eq!(queue.failed_count()?, 0);
            queue.add_crate("bar", "1.0.0", 0, None)?;

            for _ in 0..MAX_ATTEMPTS {
                assert_eq!(queue.failed_count()?, 0);
                queue.process_next_crate(|krate| {
                    assert_eq!("foo", krate.name);
                    failure::bail!("this failed");
                })?;
            }
            assert_eq!(queue.failed_count()?, 1);

            queue.process_next_crate(|krate| {
                assert_eq!("bar", krate.name);
                Ok(())
            })?;
            assert_eq!(queue.failed_count()?, 1);

            Ok(())
        });
    }

    #[test]
    fn test_queued_crates() {
        crate::test::wrapper(|env| {
            let queue = env.build_queue();

            let test_crates = [
                ("bar", "1.0.0", 0),
                ("foo", "1.0.0", -10),
                ("baz", "1.0.0", 10),
            ];
            for krate in &test_crates {
                queue.add_crate(krate.0, krate.1, krate.2, None)?;
            }

            assert_eq!(
                vec![
                    ("foo", "1.0.0", -10),
                    ("bar", "1.0.0", 0),
                    ("baz", "1.0.0", 10),
                ],
                queue
                    .queued_crates()?
                    .iter()
                    .map(|c| (c.name.as_str(), c.version.as_str(), c.priority))
                    .collect::<Vec<_>>()
            );

            Ok(())
        });
    }
}
