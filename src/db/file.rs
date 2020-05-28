//! Simple module to store files in database.
//!
//! cratesfyi is generating more than 5 million files, they are small and mostly html files.
//! They are using so many inodes and it is better to store them in database instead of
//! filesystem. This module is adding files into database and retrieving them.

use crate::error::Result;
use crate::storage::Storage;
use postgres::Connection;

use serde_json::Value;
use std::path::{Path, PathBuf};

pub(crate) use crate::storage::Blob;

pub(crate) fn get_path(conn: &Connection, path: &str) -> Result<Blob> {
    Storage::new(conn).get(path)
}

/// Store all files in a directory and return [[mimetype, filename]] as Json
///
/// If there is an S3 Client configured, store files into an S3 bucket;
/// otherwise, stores files into the 'files' table of the local database.
///
/// The mimetype is detected using `magic`.
///
/// Note that this function is used for uploading both sources
/// and files generated by rustdoc.
pub fn add_path_into_database<P: AsRef<Path>>(
    conn: &Connection,
    prefix: &str,
    path: P,
) -> Result<Value> {
    let mut backend = Storage::new(conn);
    let file_list = backend.store_all(conn, prefix, path.as_ref())?;
    file_list_to_json(file_list.into_iter().collect())
}

fn file_list_to_json(file_list: Vec<(PathBuf, String)>) -> Result<Value> {
    let file_list: Vec<_> = file_list
        .into_iter()
        .map(|(path, name)| {
            Value::Array(vec![
                Value::String(name),
                Value::String(path.into_os_string().into_string().unwrap()),
            ])
        })
        .collect();

    Ok(Value::Array(file_list))
}
