use crate::db::Pool;
use crate::error::Result;
use crate::{Config, Metrics};
use log::error;
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

    pub(crate) fn lock_path(&self) -> PathBuf {
        self.config.prefix.join("docsrs.lock")
    }

    /// Checks for the lock file and returns whether it currently exists.
    pub fn is_locked(&self) -> bool {
        self.lock_path().exists()
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
