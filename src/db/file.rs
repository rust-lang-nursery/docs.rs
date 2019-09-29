//! Simple module to store files in database.
//!
//! cratesfyi is generating more than 5 million files, they are small and mostly html files.
//! They are using so many inodes and it is better to store them in database instead of
//! filesystem. This module is adding files into database and retrieving them.


use std::path::{PathBuf, Path};
use postgres::Connection;
use rustc_serialize::json::{Json, ToJson};
use std::fs;
use std::io::Read;
use crate::error::Result;
use failure::err_msg;
use rusoto_s3::{S3, PutObjectRequest, GetObjectRequest, S3Client};
use rusoto_core::region::Region;
use rusoto_credential::EnvironmentProvider;


fn get_file_list_from_dir<P: AsRef<Path>>(path: P,
                                          files: &mut Vec<PathBuf>)
                                          -> Result<()> {
    let path = path.as_ref();

    for file in path.read_dir()? {
        let file = file?;

        if file.file_type()?.is_file() {
            files.push(file.path());
        } else if file.file_type()?.is_dir() {
            get_file_list_from_dir(file.path(), files)?;
        }
    }

    Ok(())
}


pub fn get_file_list<P: AsRef<Path>>(path: P) -> Result<Vec<PathBuf>> {
    let path = path.as_ref();
    let mut files = Vec::new();

    if !path.exists() {
        return Err(err_msg("File not found"));
    } else if path.is_file() {
        files.push(PathBuf::from(path.file_name().unwrap()));
    } else if path.is_dir() {
        get_file_list_from_dir(path, &mut files)?;
        for file_path in &mut files {
            // We want the paths in this list to not be {path}/bar.txt but just bar.txt
            *file_path = PathBuf::from(file_path.strip_prefix(path).unwrap());
        }
    }

    Ok(files)
}

pub struct Blob {
    pub path: String,
    pub mime: String,
    pub date_updated: time::Timespec,
    pub content: Vec<u8>,
}

pub fn get_path(conn: &Connection, path: &str) -> Option<Blob> {
    if let Some(client) = s3_client() {
        let res = client.get_object(GetObjectRequest {
            bucket: "rust-docs-rs".into(),
            key: path.into(),
            ..Default::default()
        }).sync();

        let res = match res {
            Ok(r) => r,
            Err(_) => {
                return None;
            }
        };

        let mut b = res.body.unwrap().into_blocking_read();
        let mut content = Vec::new();
        b.read_to_end(&mut content).unwrap();

        let last_modified = res.last_modified.unwrap();
        let last_modified = time::strptime(&last_modified, "%a, %d %b %Y %H:%M:%S %Z")
            .unwrap_or_else(|e| panic!("failed to parse {:?} as timespec: {:?}", last_modified, e))
            .to_timespec();

        Some(Blob {
            path: path.into(),
            mime: res.content_type.unwrap(),
            date_updated: last_modified,
            content,
        })
    } else {
        let rows = conn.query("SELECT path, mime, date_updated, content
                            FROM files
                            WHERE path = $1", &[&path]).unwrap();

        if rows.len() == 0 {
            None
        } else {
            let row = rows.get(0);

            Some(Blob {
                path: row.get(0),
                mime: row.get(1),
                date_updated: row.get(2),
                content: row.get(3),
            })
        }
    }
}

fn s3_client() -> Option<S3Client> {
    // If AWS keys aren't configured, then presume we should use the DB exclusively
    // for file storage.
    if std::env::var_os("AWS_ACCESS_KEY_ID").is_none() {
        return None;
    }
    Some(S3Client::new_with(
        rusoto_core::request::HttpClient::new().unwrap(),
        EnvironmentProvider::default(),
        std::env::var("S3_ENDPOINT").ok().map(|e| Region::Custom {
            name: "us-west-1".to_owned(),
            endpoint: e,
        }).unwrap_or(Region::UsWest1),
    ))
}

/// Adds files into database and returns list of files with their mime type in Json
pub fn add_path_into_database<P: AsRef<Path>>(conn: &Connection,
                                              prefix: &str,
                                              path: P)
                                              -> Result<Json> {
    use magic::{Cookie, flags};
    let cookie = Cookie::open(flags::MIME_TYPE)?;
    cookie.load::<&str>(&[])?;

    let trans = conn.transaction()?;
    let mut client = s3_client();
    let mut file_list_with_mimes: Vec<(String, PathBuf)> = Vec::new();

    for file_path in get_file_list(&path)? {
        let (path, content, mime) = {
            let path = Path::new(path.as_ref()).join(&file_path);
            // Some files have insufficient permissions (like .lock file created by cargo in
            // documentation directory). We are skipping this files.
            let mut file = match fs::File::open(path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let mut content: Vec<u8> = Vec::new();
            file.read_to_end(&mut content)?;
            let bucket_path = Path::new(prefix).join(&file_path)
                .into_os_string().into_string().unwrap();

            let mime = {
                let mime = cookie.buffer(&content)?;
                // css's are causing some problem in browsers
                // magic will return text/plain for css file types
                // convert them to text/css
                // do the same for javascript files
                if mime == "text/plain" {
                    let e = file_path.extension().unwrap_or_default();
                    if e == "css" {
                        "text/css".to_owned()
                    } else if e == "js" {
                        "application/javascript".to_owned()
                    } else {
                        mime.to_owned()
                    }
                } else {
                    mime.to_owned()
                }
            };

            let content: Option<Vec<u8>> = if let Some(client) = &mut client {
                let mut attempts = 0;
                loop {
                    let s3_res = client.put_object(PutObjectRequest {
                        bucket: "rust-docs-rs".into(),
                        key: bucket_path.clone(),
                        body: Some(content.clone().into()),
                        content_type: Some(mime.clone()),
                        ..Default::default()
                    }).sync();
                    attempts += 1;
                    match s3_res {
                        // we've successfully uploaded the content, so steal it;
                        // we don't want to put it in the DB
                        Ok(_) => break None,
                        // Since s3 was configured, we want to panic on failure to upload.
                        Err(e) => {
                            log::error!("failed to upload to {}: {:?}", bucket_path, e);
                            // Get a new client, in case the old one's connection is stale.
                            // AWS will kill our connection if it's alive for too long; this avoids
                            // that preventing us from building the crate entirely.
                            *client = s3_client().unwrap();
                            if attempts > 3 {
                                panic!("failed to upload 3 times, exiting");
                            } else {
                                continue;
                            }
                        },
                    }
                }
            } else {
                Some(content.clone().into())
            };

            file_list_with_mimes.push((mime.clone(), file_path.clone()));

            (
                bucket_path,
                content,
                mime,
            )
        };

        // If AWS credentials are configured, don't insert/update the database
        if client.is_none() {
            // check if file already exists in database
            let rows = conn.query("SELECT COUNT(*) FROM files WHERE path = $1", &[&path])?;

            let content = content.expect("content never None if client is None");

            if rows.get(0).get::<usize, i64>(0) == 0 {
                trans.query("INSERT INTO files (path, mime, content) VALUES ($1, $2, $3)",
                                &[&path, &mime, &content])?;
            } else {
                trans.query("UPDATE files SET mime = $2, content = $3, date_updated = NOW() \
                                WHERE path = $1",
                                &[&path, &mime, &content])?;
            }
        }
    }

    trans.commit()?;

    file_list_to_json(file_list_with_mimes)
}



fn file_list_to_json(file_list: Vec<(String, PathBuf)>) -> Result<Json> {

    let mut file_list_json: Vec<Json> = Vec::new();

    for file in file_list {
        let mut v: Vec<String> = Vec::new();
        v.push(file.0.clone());
        v.push(file.1.into_os_string().into_string().unwrap());
        file_list_json.push(v.to_json());
    }

    Ok(file_list_json.to_json())
}

pub fn move_to_s3(conn: &Connection, n: usize) -> Result<usize> {
    let trans = conn.transaction()?;
    let client = s3_client().expect("configured s3");

    let rows = trans.query(
            &format!("SELECT path, mime, content FROM files WHERE content != E'in-s3' LIMIT {}", n),
            &[])?;
    let count = rows.len();

    let mut rt = ::tokio::runtime::Runtime::new().unwrap();
    let mut futures = Vec::new();
    for row in &rows {
        let path: String = row.get(0);
        let mime: String = row.get(1);
        let content: Vec<u8> = row.get(2);
        let path_1 = path.clone();
        futures.push(client.put_object(PutObjectRequest {
            bucket: "rust-docs-rs".into(),
            key: path.clone(),
            body: Some(content.into()),
            content_type: Some(mime),
            ..Default::default()
        }).map(move |_| {
            path_1
        }).map_err(move |e| {
            panic!("failed to upload to {}: {:?}", path, e)
        }));
    }

    use ::futures::future::Future;
    match rt.block_on(::futures::future::join_all(futures)) {
        Ok(paths) => {
            let statement = trans.prepare("DELETE FROM files WHERE path = $1").unwrap();
            for path in paths {
                statement.execute(&[&path]).unwrap();
            }
        }
        Err(e) => {
            panic!("results err: {:?}", e);
        }
    }

    trans.commit()?;

    Ok(count)
}

#[cfg(test)]
mod test {
    use std::env;
    use super::get_file_list;

    #[test]
    fn test_get_file_list() {
        let _ = env_logger::try_init();

        let files = get_file_list(env::current_dir().unwrap());
        assert!(files.is_ok());
        assert!(files.unwrap().len() > 0);

        let files = get_file_list(env::current_dir().unwrap().join("Cargo.toml")).unwrap();
        assert_eq!(files[0], std::path::Path::new("Cargo.toml"));
    }
}
