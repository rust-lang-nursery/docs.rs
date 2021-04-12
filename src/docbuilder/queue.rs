//! Updates registry index and builds new packages

use super::{DocBuilder, PackageKind, RustwideBuilder};
use crate::error::Result;
use crate::utils::get_crate_priority;
use crate::Index;
use crates_index_diff::ChangeKind;
use log::{debug, error};

impl DocBuilder {
    /// Updates registry index repository and adds new crates into build queue.
    /// Returns the number of crates added
    pub fn get_new_crates(&mut self, index: &Index) -> Result<usize> {
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

                    match self.build_queue.add_crate(
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
    pub(crate) fn build_next_queue_package(
        &mut self,
        builder: &mut RustwideBuilder,
    ) -> Result<bool> {
        let mut processed = false;
        let queue = self.build_queue.clone();
        queue.process_next_crate(|krate| {
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
