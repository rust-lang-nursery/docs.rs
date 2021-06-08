//! Database operations

pub use self::add_package::update_crate_data_in_database;
pub(crate) use self::add_package::{
    add_build_into_database, add_doc_coverage, add_package_into_database,
};
pub use self::delete::{delete_crate, delete_version};
pub use self::file::{add_path_into_database, add_path_into_remote_archive};
pub use self::migrate::migrate;
pub use self::pool::{Pool, PoolClient, PoolError};

mod add_package;
pub mod blacklist;
mod delete;
pub(crate) mod file;
mod migrate;
mod pool;
pub(crate) mod types;
