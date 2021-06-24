use crate::{docbuilder::RustwideBuilder, utils::pubsubhubbub, BuildQueue, DocBuilder};
use failure::Error;
use log::{debug, error, info, warn};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// TODO: change to `fn() -> Result<!, Error>` when never _finally_ stabilizes
pub fn queue_builder(
    mut doc_builder: DocBuilder,
    mut builder: RustwideBuilder,
    build_queue: Arc<BuildQueue>,
) -> Result<(), Error> {
    /// Represents the current state of the builder thread.
    enum BuilderState {
        /// The builder thread has just started, and hasn't built any crates yet.
        Fresh,
        /// The builder has just seen an empty build queue.
        EmptyQueue,
        /// The builder has just seen the lock file.
        Locked,
        /// The builder has just finished building a crate. The enclosed count is the number of
        /// crates built since the caches have been refreshed.
        QueueInProgress(usize),
    }

    let mut status = BuilderState::Fresh;

    loop {
        if !status.is_in_progress() {
            thread::sleep(Duration::from_secs(60));
        }

        // check lock file
        if doc_builder.is_locked() {
            warn!("Lock file exits, skipping building new crates");
            status = BuilderState::Locked;
            continue;
        }

        if status.count() >= 10 {
            // periodically, ping the hubs
            debug!("10 builds in a row; pinging pubsubhubhub");
            status = BuilderState::QueueInProgress(0);

            match pubsubhubbub::ping_hubs() {
                Err(e) => error!("Failed to ping hub: {}", e),
                Ok(n) => debug!("Succesfully pinged {} hubs", n),
            }
        }

        // Only build crates if there are any to build
        debug!("Checking build queue");
        match build_queue.pending_count() {
            Err(e) => {
                error!("Failed to read the number of crates in the queue: {}", e);
                continue;
            }

            Ok(0) => {
                if status.count() > 0 {
                    // ping the hubs before continuing
                    match pubsubhubbub::ping_hubs() {
                        Err(e) => error!("Failed to ping hub: {}", e),
                        Ok(n) => debug!("Succesfully pinged {} hubs", n),
                    }
                }
                debug!("Queue is empty, going back to sleep");
                status = BuilderState::EmptyQueue;
                continue;
            }

            Ok(queue_count) => {
                info!(
                    "Starting build with {} crates in queue (currently on a {} crate streak)",
                    queue_count,
                    status.count()
                );
            }
        }

        // If a panic occurs while building a crate, lock the queue until an admin has a chance to look at it.
        let res = catch_unwind(AssertUnwindSafe(|| {
            match doc_builder.build_next_queue_package(&mut builder) {
                Err(e) => error!("Failed to build crate from queue: {}", e),
                Ok(crate_built) => {
                    if crate_built {
                        status.increment();
                    }
                }
            }
        }));

        if let Err(e) = res {
            error!("GRAVE ERROR Building new crates panicked: {:?}", e);
            // If we panic here something is really truly wrong and trying to handle the error won't help.
            doc_builder.lock().expect("failed to lock queue");
        }
    }

    impl BuilderState {
        fn count(&self) -> usize {
            match *self {
                BuilderState::QueueInProgress(n) => n,
                _ => 0,
            }
        }

        fn is_in_progress(&self) -> bool {
            matches!(*self, BuilderState::QueueInProgress(_))
        }

        fn increment(&mut self) {
            *self = BuilderState::QueueInProgress(self.count() + 1);
        }
    }
}
