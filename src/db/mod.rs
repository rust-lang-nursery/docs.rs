//! Database operations

pub(crate) use self::add_package::add_build_into_database;
pub(crate) use self::add_package::add_package_into_database;
pub use self::delete_crate::delete_crate;
pub use self::file::add_path_into_database;
pub use self::migrate::migrate;

use failure::Fail;
use postgres::{Connection, TlsMode};
use std::env;

mod add_package;
pub mod blacklist;
mod delete_crate;
pub(crate) mod file;
mod migrate;

/// Connects to database
pub fn connect_db() -> Result<Connection, failure::Error> {
    let err = "CRATESFYI_DATABASE_URL environment variable is not set";
    let db_url = env::var("CRATESFYI_DATABASE_URL").map_err(|e| e.context(err))?;
    Connection::connect(&db_url[..], TlsMode::None).map_err(Into::into)
}

pub(crate) fn create_pool() -> r2d2::Pool<r2d2_postgres::PostgresConnectionManager> {
    let db_url = env::var("CRATESFYI_DATABASE_URL")
        .expect("CRATESFYI_DATABASE_URL environment variable is not exists");

    let max_pool_size = env::var("DOCSRS_MAX_POOL_SIZE")
        .map(|s| {
            s.parse::<u32>()
                .expect("DOCSRS_MAX_POOL_SIZE must be an integer")
        })
        .unwrap_or(90);
    crate::web::metrics::MAX_DB_CONNECTIONS.set(max_pool_size as i64);

    let min_pool_idle = env::var("DOCSRS_MIN_POOL_IDLE")
        .map(|s| {
            s.parse::<u32>()
                .expect("DOCSRS_MIN_POOL_IDLE must be an integer")
        })
        .unwrap_or(10);

    let manager =
        r2d2_postgres::PostgresConnectionManager::new(&db_url[..], r2d2_postgres::TlsMode::None)
            .expect("Failed to create PostgresConnectionManager");

    r2d2::Pool::builder()
        .max_size(max_pool_size)
        .min_idle(Some(min_pool_idle))
        .build(manager)
        .expect("Failed to create r2d2 pool")
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    #[ignore]
    fn test_connect_db() {
        let conn = connect_db();
        assert!(conn.is_ok());
    }
}
