use super::{Blob, FileRange, StorageTransaction};
use crate::db::Pool;
use crate::Metrics;
use failure::Error;
use postgres::Transaction;
use std::{convert::TryFrom, sync::Arc};

pub(crate) struct DatabaseBackend {
    pool: Pool,
    metrics: Arc<Metrics>,
}

impl DatabaseBackend {
    pub(crate) fn new(pool: Pool, metrics: Arc<Metrics>) -> Self {
        Self { pool, metrics }
    }

    pub(super) fn exists(&self, path: &str) -> Result<bool, Error> {
        let query = "SELECT COUNT(*) > 0 FROM files WHERE path = $1";
        let mut conn = self.pool.get()?;
        Ok(conn.query(query, &[&path])?[0].get(0))
    }

    pub(super) fn get(
        &self,
        path: &str,
        max_size: usize,
        range: Option<FileRange>,
    ) -> Result<Blob, Error> {
        use std::convert::TryInto;

        // The maximum size for a BYTEA (the type used for `content`) is 1GB, so this cast is safe:
        // https://www.postgresql.org/message-id/162867790712200946i7ba8eb92v908ac595c0c35aee%40mail.gmail.com
        let max_size = max_size.min(std::i32::MAX as usize) as i32;

        let rows = if let Some(r) = range {
            // when we only want to get a range we can validate already if the range is small enough
            if range.len() > max_size as u64 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    crate::error::SizeLimitReached,
                )
                .into());
            }
            let range_start = i32::try_from(*r.start())?;

            self.pool.get()?.query(
                "SELECT
                     path, mime, date_updated, compression,
                     substring(content from $2 for $3) as content,
                     FALSE as is_too_big
                 FROM files
                 WHERE path = $1;",
                &[
                    &path,
                    &(range_start + 1), // postgres substring is 1-indexed
                    &(range_size as i32),
                ],
            )?
        } else {
            // The size limit is checked at the database level, to avoid receiving data altogether if
            // the limit is exceeded.
            self.pool.get()?.query(
                "SELECT
                     path, mime, date_updated, compression,
                     (CASE WHEN LENGTH(content) <= $2 THEN content ELSE NULL END) AS content,
                     (LENGTH(content) > $2) AS is_too_big
                 FROM files
                 WHERE path = $1;",
                &[&path, &(max_size)],
            )?
        };

        if rows.is_empty() {
            Err(super::PathNotFoundError.into())
        } else {
            let row = &rows[0];

            if row.get("is_too_big") {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    crate::error::SizeLimitReached,
                )
                .into());
            }

            let compression = row.get::<_, Option<i32>>("compression").map(|i| {
                i.try_into()
                    .expect("invalid compression algorithm stored in database")
            });
            Ok(Blob {
                path: row.get("path"),
                mime: row.get("mime"),
                date_updated: row.get("date_updated"),
                content: row.get("content"),
                compression,
            })
        }
    }

    pub(super) fn start_connection(&self) -> Result<DatabaseClient, Error> {
        Ok(DatabaseClient {
            conn: self.pool.get()?,
            metrics: self.metrics.clone(),
        })
    }
}

pub(super) struct DatabaseClient {
    conn: crate::db::PoolClient,
    metrics: Arc<Metrics>,
}

impl DatabaseClient {
    pub(super) fn start_storage_transaction(
        &mut self,
    ) -> Result<DatabaseStorageTransaction<'_>, Error> {
        Ok(DatabaseStorageTransaction {
            transaction: self.conn.transaction()?,
            metrics: &self.metrics,
        })
    }
}

pub(super) struct DatabaseStorageTransaction<'a> {
    transaction: Transaction<'a>,
    metrics: &'a Metrics,
}

impl<'a> StorageTransaction for DatabaseStorageTransaction<'a> {
    fn store_batch(&mut self, batch: Vec<Blob>) -> Result<(), Error> {
        for blob in batch {
            let compression = blob.compression.map(|alg| alg as i32);
            self.transaction.query(
                "INSERT INTO files (path, mime, content, compression)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (path) DO UPDATE
                    SET mime = EXCLUDED.mime, content = EXCLUDED.content, compression = EXCLUDED.compression",
                &[&blob.path, &blob.mime, &blob.content, &compression],
            )?;
            self.metrics.uploaded_files_total.inc();
        }
        Ok(())
    }

    fn delete_prefix(&mut self, prefix: &str) -> Result<(), Error> {
        self.transaction.execute(
            "DELETE FROM files WHERE path LIKE $1;",
            &[&format!("{}%", prefix.replace('%', "\\%"))],
        )?;
        Ok(())
    }

    fn complete(self: Box<Self>) -> Result<(), Error> {
        self.transaction.commit()?;
        Ok(())
    }
}

// The tests for this module are in src/storage/mod.rs, as part of the backend tests. Please add
// any test checking the public interface there.
