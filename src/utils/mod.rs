//! Various utilities for docs.rs

pub(crate) use self::cargo_metadata::{CargoMetadata, Package as MetadataPackage};
pub(crate) use self::copy::copy_dir_all;
pub use self::daemon::start_daemon;
pub(crate) use self::html::rewrite_lol;
pub use self::queue::{get_crate_priority, remove_crate_priority, set_crate_priority};
pub use self::queue_builder::queue_builder;
pub(crate) use self::rustc_version::parse_rustc_version;

#[cfg(test)]
pub(crate) use self::cargo_metadata::{Dependency, Target};

mod cargo_metadata;
#[cfg(feature = "consistency_check")]
pub mod consistency;
mod copy;
pub(crate) mod daemon;
mod html;
mod pubsubhubbub;
mod queue;
mod queue_builder;
mod rustc_version;
pub(crate) mod sized_buffer;
