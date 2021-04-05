mod archive_index;
mod compression;
mod database;
mod s3;

pub use self::compression::{compress, decompress, CompressionAlgorithm, CompressionAlgorithms};
use self::database::DatabaseBackend;
use self::s3::S3Backend;
use crate::{db::Pool, Config, Metrics};
use chrono::{DateTime, Utc};
use failure::{err_msg, Error};
use path_slash::PathExt;
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_CONCURRENT_UPLOADS: usize = 1000;

pub type FileRange = std::ops::RangeInclusive<u64>;

#[derive(Debug, failure::Fail)]
#[fail(display = "path not found")]
pub(crate) struct PathNotFoundError;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Blob {
    pub(crate) path: String,
    pub(crate) mime: String,
    pub(crate) date_updated: DateTime<Utc>,
    pub(crate) content: Vec<u8>,
    pub(crate) compression: Option<CompressionAlgorithm>,
}

fn get_file_list_from_dir<P: AsRef<Path>>(path: P, files: &mut Vec<PathBuf>) -> Result<(), Error> {
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

pub fn get_file_list<P: AsRef<Path>>(path: P) -> Result<Vec<PathBuf>, Error> {
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

#[derive(Debug, failure::Fail)]
#[fail(display = "invalid storage backend")]
pub(crate) struct InvalidStorageBackendError;

#[derive(Debug)]
pub(crate) enum StorageKind {
    Database,
    S3,
}

impl std::str::FromStr for StorageKind {
    type Err = InvalidStorageBackendError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "database" => Ok(StorageKind::Database),
            "s3" => Ok(StorageKind::S3),
            _ => Err(InvalidStorageBackendError),
        }
    }
}

enum StorageBackend {
    Database(DatabaseBackend),
    S3(Box<S3Backend>),
}

pub struct Storage {
    backend: StorageBackend,
    local_archive_cache_path: PathBuf,
}

impl Storage {
    pub fn new(pool: Pool, metrics: Arc<Metrics>, config: &Config) -> Result<Self, Error> {
        Ok(Storage {
            local_archive_cache_path: config.local_archive_cache_path.clone(),
            backend: match config.storage_backend {
                StorageKind::Database => {
                    StorageBackend::Database(DatabaseBackend::new(pool, metrics))
                }
                StorageKind::S3 => StorageBackend::S3(Box::new(S3Backend::new(metrics, config)?)),
            },
        })
    }

    pub(crate) fn exists(&self, path: &str) -> Result<bool, Error> {
        match &self.backend {
            StorageBackend::Database(db) => db.exists(path),
            StorageBackend::S3(s3) => s3.exists(path),
        }
    }

    pub(crate) fn exists_in_archive(&self, archive_path: &str, path: &str) -> Result<bool, Error> {
        let index = self.get_index_for(archive_path)?;
        Ok(index.find_file(path).is_ok())
    }

    pub(crate) fn get(&self, path: &str, max_size: usize) -> Result<Blob, Error> {
        let mut blob = match &self.backend {
            StorageBackend::Database(db) => db.get(path, max_size, None),
            StorageBackend::S3(s3) => s3.get(path, max_size, None),
        }?;
        if let Some(alg) = blob.compression {
            blob.content = decompress(blob.content.as_slice(), alg, max_size)?;
            blob.compression = None;
        }
        Ok(blob)
    }

    pub(super) fn get_range(
        &self,
        path: &str,
        max_size: usize,
        range: FileRange,
        compression: Option<CompressionAlgorithm>,
    ) -> Result<Blob, Error> {
        let mut blob = match &self.backend {
            StorageBackend::Database(db) => db.get(path, max_size, Some(range)),
            StorageBackend::S3(s3) => s3.get(path, max_size, Some(range)),
        }?;
        // file content encoding is ignored for ranges
        // since we only have a range anyways, and we need the encoding
        // for the range, not the file
        if let Some(alg) = compression {
            blob.content = decompress(blob.content.as_slice(), alg, max_size)?;
            blob.compression = None;
        }
        Ok(blob)
    }

    fn get_index_for(&self, archive_path: &str) -> Result<archive_index::Index, Error> {
        // remote/folder/and/x.zip.index
        let remote_index_path = format!("{}.index", archive_path);
        let local_index_path = self.local_archive_cache_path.join(&remote_index_path);

        if local_index_path.exists() {
            let mut file = fs::File::open(local_index_path)?;
            archive_index::Index::load(&mut file)
        } else {
            let index_content = self.get(&remote_index_path, std::usize::MAX)?.content;

            fs::create_dir_all(
                local_index_path
                    .parent()
                    .ok_or_else(|| err_msg("index path without parent"))?,
            )?;
            let mut file = fs::File::create(&local_index_path)?;
            file.write_all(&index_content)?;

            archive_index::Index::load(&mut &index_content[..])
        }
    }

    pub(crate) fn get_from_archive(
        &self,
        archive_path: &str,
        path: &str,
        max_size: usize,
    ) -> Result<Blob, Error> {
        let index = self.get_index_for(archive_path)?;
        let info = index.find_file(path)?;

        let blob = self.get_range(
            archive_path,
            max_size,
            info.range(),
            Some(info.compression()),
        )?;

        Ok(Blob {
            path: format!("{}/{}", archive_path, path),
            mime: detect_mime(&path).into(),
            date_updated: blob.date_updated,
            content: blob.content,
            compression: None,
        })
    }

    pub(crate) fn store_all_in_archive(
        &self,
        archive_path: &str,
        root_dir: &Path,
    ) -> Result<(HashMap<PathBuf, String>, CompressionAlgorithm), Error> {
        let mut file_paths = HashMap::new();

        // due to how we retrieve the archived files, the compression method has to be supported
        // by our normal content-encoding / compression logic.
        // Deflate is not supported right now, and is compressing worse anyways.
        let options =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Bzip2);

        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        for file_path in get_file_list(root_dir)? {
            let mut file = fs::File::open(root_dir.join(&file_path))?;

            // TODO: should we create directories with `start_directory`?
            // it's not standard, but convention for zip files

            zip.start_file(file_path.to_str().unwrap(), options)?;
            io::copy(&mut file, &mut zip)?;

            let mime = detect_mime(&file_path);
            file_paths.insert(file_path, mime.to_string());
        }

        let mut zip_content = zip.finish()?.into_inner();
        let index = archive_index::Index::new_from_zip(&mut io::Cursor::new(&mut zip_content))?;
        let mut index_content = vec![];
        index.save(&mut index_content)?;
        let alg = CompressionAlgorithm::default();
        let compressed_index_content = compress(&index_content[..], alg)?;

        let remote_index_path = format!("{}.index", &archive_path);

        // additionally store the index in the local cache, so it's directly available
        let local_index_path = self.local_archive_cache_path.join(&remote_index_path);
        if local_index_path.exists() {
            fs::remove_file(&local_index_path)?;
        }
        fs::create_dir_all(local_index_path.parent().unwrap())?;
        let mut local_index_file = fs::File::create(&local_index_path)?;
        local_index_file.write_all(&index_content)?;

        self.store_inner(
            vec![
                Blob {
                    path: archive_path.to_string(),
                    mime: "application/zip".to_owned(),
                    content: zip_content,
                    compression: None,
                    date_updated: Utc::now(),
                },
                Blob {
                    path: remote_index_path,
                    mime: "application/octet-stream".to_owned(),
                    content: compressed_index_content,
                    compression: Some(alg),
                    date_updated: Utc::now(),
                },
            ]
            .into_iter()
            .map(Ok),
        )?;

        let file_alg = CompressionAlgorithm::Bzip2;
        Ok((file_paths, file_alg))
    }

    fn transaction<T, F>(&self, f: F) -> Result<T, Error>
    where
        F: FnOnce(&mut dyn StorageTransaction) -> Result<T, Error>,
    {
        let mut conn;
        let mut trans: Box<dyn StorageTransaction> = match &self.backend {
            StorageBackend::Database(db) => {
                conn = db.start_connection()?;
                Box::new(conn.start_storage_transaction()?)
            }
            StorageBackend::S3(s3) => Box::new(s3.start_storage_transaction()),
        };

        let res = f(trans.as_mut())?;
        trans.complete()?;
        Ok(res)
    }

    // Store all files in `root_dir` into the backend under `prefix`.
    //
    // This returns (map<filename, mime type>, set<compression algorithms>).
    pub(crate) fn store_all(
        &self,
        prefix: &str,
        root_dir: &Path,
    ) -> Result<(HashMap<PathBuf, String>, HashSet<CompressionAlgorithm>), Error> {
        let mut file_paths_and_mimes = HashMap::new();
        let mut algs = HashSet::with_capacity(1);

        let blobs = get_file_list(root_dir)?
            .into_iter()
            .filter_map(|file_path| {
                // Some files have insufficient permissions
                // (like .lock file created by cargo in documentation directory).
                // Skip these files.
                fs::File::open(root_dir.join(&file_path))
                    .ok()
                    .map(|file| (file_path, file))
            })
            .map(|(file_path, file)| -> Result<_, Error> {
                let alg = CompressionAlgorithm::default();
                let content = compress(file, alg)?;
                let bucket_path = Path::new(prefix).join(&file_path).to_slash().unwrap();

                let mime = detect_mime(&file_path);
                file_paths_and_mimes.insert(file_path, mime.to_string());
                algs.insert(alg);

                Ok(Blob {
                    path: bucket_path,
                    mime: mime.to_string(),
                    content,
                    compression: Some(alg),
                    // this field is ignored by the backend
                    date_updated: Utc::now(),
                })
            });

        self.store_inner(blobs)?;
        Ok((file_paths_and_mimes, algs))
    }

    #[cfg(test)]
    pub(crate) fn store_blobs(&self, blobs: Vec<Blob>) -> Result<(), Error> {
        self.store_inner(blobs.into_iter().map(Ok))
    }

    // Store file into the backend at the given path (also used to detect mime type), returns the
    // chosen compression algorithm
    pub(crate) fn store_one(
        &self,
        path: impl Into<String>,
        content: impl Into<Vec<u8>>,
    ) -> Result<CompressionAlgorithm, Error> {
        let path = path.into();
        let content = content.into();
        let alg = CompressionAlgorithm::default();
        let content = compress(&*content, alg)?;
        let mime = detect_mime(&path).to_owned();

        self.store_inner(std::iter::once(Ok(Blob {
            path,
            mime,
            content,
            compression: Some(alg),
            // this field is ignored by the backend
            date_updated: Utc::now(),
        })))?;

        Ok(alg)
    }

    fn store_inner(
        &self,
        blobs: impl IntoIterator<Item = Result<Blob, Error>>,
    ) -> Result<(), Error> {
        let mut blobs = blobs.into_iter();
        self.transaction(|trans| {
            loop {
                let batch: Vec<_> = blobs
                    .by_ref()
                    .take(MAX_CONCURRENT_UPLOADS)
                    .collect::<Result<_, Error>>()?;
                if batch.is_empty() {
                    break;
                }
                trans.store_batch(batch)?;
            }
            Ok(())
        })
    }

    pub(crate) fn delete_prefix(&self, prefix: &str) -> Result<(), Error> {
        self.transaction(|trans| trans.delete_prefix(prefix))
    }

    // We're using `&self` instead of consuming `self` or creating a Drop impl because during tests
    // we leak the web server, and Drop isn't executed in that case (since the leaked web server
    // still holds a reference to the storage).
    #[cfg(test)]
    pub(crate) fn cleanup_after_test(&self) -> Result<(), Error> {
        if let StorageBackend::S3(s3) = &self.backend {
            s3.cleanup_after_test()?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.backend {
            StorageBackend::Database(_) => write!(f, "database-backed storage"),
            StorageBackend::S3(_) => write!(f, "S3-backed storage"),
        }
    }
}

trait StorageTransaction {
    fn store_batch(&mut self, batch: Vec<Blob>) -> Result<(), Error>;
    fn delete_prefix(&mut self, prefix: &str) -> Result<(), Error>;
    fn complete(self: Box<Self>) -> Result<(), Error>;
}

fn detect_mime(file_path: impl AsRef<Path>) -> &'static str {
    let mime = mime_guess::from_path(file_path.as_ref())
        .first_raw()
        .unwrap_or("text/plain");
    match mime {
        "text/plain" | "text/troff" | "text/x-markdown" | "text/x-rust" | "text/x-toml" => {
            match file_path.as_ref().extension().and_then(OsStr::to_str) {
                Some("md") => "text/markdown",
                Some("rs") => "text/rust",
                Some("markdown") => "text/markdown",
                Some("css") => "text/css",
                Some("toml") => "text/toml",
                Some("js") => "application/javascript",
                Some("json") => "application/json",
                _ => mime,
            }
        }
        "image/svg" => "image/svg+xml",
        _ => mime,
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::env;

    #[test]
    fn test_get_file_list() {
        crate::test::init_logger();
        let files = get_file_list(env::current_dir().unwrap());
        assert!(files.is_ok());
        assert!(!files.unwrap().is_empty());

        let files = get_file_list(env::current_dir().unwrap().join("Cargo.toml")).unwrap();
        assert_eq!(files[0], std::path::Path::new("Cargo.toml"));
    }

    #[test]
    fn test_mime_types() {
        check_mime(".gitignore", "text/plain");
        check_mime("hello.toml", "text/toml");
        check_mime("hello.css", "text/css");
        check_mime("hello.js", "application/javascript");
        check_mime("hello.html", "text/html");
        check_mime("hello.hello.md", "text/markdown");
        check_mime("hello.markdown", "text/markdown");
        check_mime("hello.json", "application/json");
        check_mime("hello.txt", "text/plain");
        check_mime("file.rs", "text/rust");
        check_mime("important.svg", "image/svg+xml");
    }

    fn check_mime(path: &str, expected_mime: &str) {
        let detected_mime = detect_mime(Path::new(&path));
        assert_eq!(detected_mime, expected_mime);
    }
}

/// Backend tests are a set of tests executed on all the supported storage backends. They ensure
/// docs.rs behaves the same no matter the storage backend currently used.
///
/// To add a new test create the function without adding the `#[test]` attribute, and add the
/// function name to the `backend_tests!` macro at the bottom of the module.
///
/// This is the preferred way to test whether backends work.
#[cfg(test)]
mod backend_tests {
    use super::*;
    use std::fs;

    fn test_exists(storage: &Storage) -> Result<(), Error> {
        assert!(!storage.exists("path/to/file.txt").unwrap());
        let blob = Blob {
            path: "path/to/file.txt".into(),
            mime: "text/plain".into(),
            date_updated: Utc::now(),
            content: "Hello world!".into(),
            compression: None,
        };
        storage.store_blobs(vec![blob])?;
        assert!(storage.exists("path/to/file.txt")?);

        Ok(())
    }

    fn test_get_object(storage: &Storage) -> Result<(), Error> {
        let blob = Blob {
            path: "foo/bar.txt".into(),
            mime: "text/plain".into(),
            date_updated: Utc::now(),
            compression: None,
            content: b"test content\n".to_vec(),
        };

        storage.store_blobs(vec![blob.clone()])?;

        let found = storage.get("foo/bar.txt", std::usize::MAX)?;
        assert_eq!(blob.mime, found.mime);
        assert_eq!(blob.content, found.content);

        for path in &["bar.txt", "baz.txt", "foo/baz.txt"] {
            assert!(storage
                .get(path, std::usize::MAX)
                .unwrap_err()
                .downcast_ref::<PathNotFoundError>()
                .is_some());
        }

        Ok(())
    }

    fn test_get_range(storage: &Storage) -> Result<(), Error> {
        let blob = Blob {
            path: "foo/bar.txt".into(),
            mime: "text/plain".into(),
            date_updated: Utc::now(),
            compression: None,
            content: b"test content\n".to_vec(),
        };

        storage.store_blobs(vec![blob.clone()])?;

        assert_eq!(
            blob.content[0..=4],
            storage
                .get_range("foo/bar.txt", std::usize::MAX, 0..=4, None)?
                .content
        );
        assert_eq!(
            blob.content[5..=12],
            storage
                .get_range("foo/bar.txt", std::usize::MAX, 5..=12, None)?
                .content
        );

        for path in &["bar.txt", "baz.txt", "foo/baz.txt"] {
            assert!(storage
                .get_range(path, std::usize::MAX, 0..=4, None)
                .unwrap_err()
                .downcast_ref::<PathNotFoundError>()
                .is_some());
        }

        Ok(())
    }

    fn test_get_too_big(storage: &Storage) -> Result<(), Error> {
        const MAX_SIZE: usize = 1024;

        let small_blob = Blob {
            path: "small-blob.bin".into(),
            mime: "text/plain".into(),
            date_updated: Utc::now(),
            content: vec![0; MAX_SIZE],
            compression: None,
        };
        let big_blob = Blob {
            path: "big-blob.bin".into(),
            mime: "text/plain".into(),
            date_updated: Utc::now(),
            content: vec![0; MAX_SIZE * 2],
            compression: None,
        };

        storage.store_blobs(vec![small_blob.clone(), big_blob])?;

        let blob = storage.get("small-blob.bin", MAX_SIZE)?;
        assert_eq!(blob.content.len(), small_blob.content.len());

        assert!(storage
            .get("big-blob.bin", MAX_SIZE)
            .unwrap_err()
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())
            .and_then(|err| err.downcast_ref::<crate::error::SizeLimitReached>())
            .is_some());

        Ok(())
    }

    fn test_store_blobs(storage: &Storage, metrics: &Metrics) -> Result<(), Error> {
        const NAMES: &[&str] = &[
            "a",
            "b",
            "a_very_long_file_name_that_has_an.extension",
            "parent/child",
            "h/i/g/h/l/y/_/n/e/s/t/e/d/_/d/i/r/e/c/t/o/r/i/e/s",
        ];

        let blobs = NAMES
            .iter()
            .map(|&path| Blob {
                path: path.into(),
                mime: "text/plain".into(),
                date_updated: Utc::now(),
                compression: None,
                content: b"Hello world!\n".to_vec(),
            })
            .collect::<Vec<_>>();

        storage.store_blobs(blobs.clone()).unwrap();

        for blob in &blobs {
            let actual = storage.get(&blob.path, std::usize::MAX)?;
            assert_eq!(blob.path, actual.path);
            assert_eq!(blob.mime, actual.mime);
        }

        assert_eq!(NAMES.len(), metrics.uploaded_files_total.get() as usize);

        Ok(())
    }

    fn test_store_all_in_archive(storage: &Storage, metrics: &Metrics) -> Result<(), Error> {
        let dir = tempfile::Builder::new()
            .prefix("docs.rs-upload-archive-test")
            .tempdir()?;
        let files = ["Cargo.toml", "src/main.rs"];
        for &file in &files {
            let path = dir.path().join(file);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, "data")?;
        }

        let (stored_files, compression_alg) =
            storage.store_all_in_archive("folder/test.zip", dir.path())?;
        // TODO: test if the index was stored locally and remotely

        assert_eq!(compression_alg, CompressionAlgorithm::Bzip2);
        assert_eq!(stored_files.len(), files.len());
        for name in &files {
            let name = Path::new(name);
            assert!(stored_files.contains_key(name));
        }
        assert_eq!(
            stored_files.get(Path::new("Cargo.toml")).unwrap(),
            "text/toml"
        );
        assert_eq!(
            stored_files.get(Path::new("src/main.rs")).unwrap(),
            "text/rust"
        );

        // delete the existing index to test the download of it
        // the first exists-query will download and store the index
        // TODO: test if the local index doesn't exist
        assert_eq!(
            storage.exists_in_archive("folder/test.zip", "Cargo.toml")?,
            true
        );
        // the second one will use the local index
        // TODO: test if the local index does exist now
        assert_eq!(
            storage.exists_in_archive("folder/test.zip", "src/main.rs")?,
            true
        );

        let file = storage.get_from_archive("folder/test.zip", "Cargo.toml", std::usize::MAX)?;
        assert_eq!(file.content, b"data");
        assert_eq!(file.mime, "text/toml");
        assert_eq!(file.path, "folder/test.zip/Cargo.toml");

        let file = storage.get_from_archive("folder/test.zip", "src/main.rs", std::usize::MAX)?;
        assert_eq!(file.content, b"data");
        assert_eq!(file.mime, "text/rust");
        assert_eq!(file.path, "folder/test.zip/src/main.rs");

        assert_eq!(2, metrics.uploaded_files_total.get());

        Ok(())
    }

    fn test_store_all(storage: &Storage, metrics: &Metrics) -> Result<(), Error> {
        let dir = tempfile::Builder::new()
            .prefix("docs.rs-upload-test")
            .tempdir()?;
        let files = ["Cargo.toml", "src/main.rs"];
        for &file in &files {
            let path = dir.path().join(file);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, "data")?;
        }

        let (stored_files, algs) = storage.store_all("prefix", dir.path())?;
        assert_eq!(stored_files.len(), files.len());
        for name in &files {
            let name = Path::new(name);
            assert!(stored_files.contains_key(name));
        }
        assert_eq!(
            stored_files.get(Path::new("Cargo.toml")).unwrap(),
            "text/toml"
        );
        assert_eq!(
            stored_files.get(Path::new("src/main.rs")).unwrap(),
            "text/rust"
        );

        let file = storage.get("prefix/Cargo.toml", std::usize::MAX)?;
        assert_eq!(file.content, b"data");
        assert_eq!(file.mime, "text/toml");
        assert_eq!(file.path, "prefix/Cargo.toml");

        let file = storage.get("prefix/src/main.rs", std::usize::MAX)?;
        assert_eq!(file.content, b"data");
        assert_eq!(file.mime, "text/rust");
        assert_eq!(file.path, "prefix/src/main.rs");

        let mut expected_algs = HashSet::new();
        expected_algs.insert(CompressionAlgorithm::default());
        assert_eq!(algs, expected_algs);

        assert_eq!(2, metrics.uploaded_files_total.get());

        Ok(())
    }

    fn test_batched_uploads(storage: &Storage) -> Result<(), Error> {
        let now = Utc::now();
        let uploads: Vec<_> = (0..=MAX_CONCURRENT_UPLOADS + 1)
            .map(|i| {
                let content = format!("const IDX: usize = {};", i).as_bytes().to_vec();
                Blob {
                    mime: "text/rust".into(),
                    content,
                    path: format!("{}.rs", i),
                    date_updated: now,
                    compression: None,
                }
            })
            .collect();

        storage.store_blobs(uploads.clone())?;

        for blob in &uploads {
            let stored = storage.get(&blob.path, std::usize::MAX)?;
            assert_eq!(&stored.content, &blob.content);
        }

        Ok(())
    }

    fn test_delete_prefix(storage: &Storage) -> Result<(), Error> {
        test_deletion(
            storage,
            "foo/bar/",
            &[
                "foo.txt",
                "foo/bar.txt",
                "foo/bar/baz.txt",
                "foo/bar/foobar.txt",
                "bar.txt",
            ],
            &["foo.txt", "foo/bar.txt", "bar.txt"],
            &["foo/bar/baz.txt", "foo/bar/foobar.txt"],
        )
    }

    fn test_delete_percent(storage: &Storage) -> Result<(), Error> {
        // PostgreSQL treats "%" as a special char when deleting a prefix. Make sure any "%" in the
        // provided prefix is properly escaped.
        test_deletion(
            storage,
            "foo/%/",
            &["foo/bar.txt", "foo/%/bar.txt"],
            &["foo/bar.txt"],
            &["foo/%/bar.txt"],
        )
    }

    fn test_deletion(
        storage: &Storage,
        prefix: &str,
        start: &[&str],
        present: &[&str],
        missing: &[&str],
    ) -> Result<(), Error> {
        storage.store_blobs(
            start
                .iter()
                .map(|path| Blob {
                    path: (*path).to_string(),
                    content: b"foo\n".to_vec(),
                    compression: None,
                    mime: "text/plain".into(),
                    date_updated: Utc::now(),
                })
                .collect(),
        )?;

        storage.delete_prefix(prefix)?;

        for existing in present {
            assert!(storage.get(existing, std::usize::MAX).is_ok());
        }
        for missing in missing {
            assert!(storage
                .get(missing, std::usize::MAX)
                .unwrap_err()
                .downcast_ref::<PathNotFoundError>()
                .is_some());
        }

        Ok(())
    }

    // Remember to add the test name to the macro below when adding a new one.

    macro_rules! backend_tests {
        (
            backends { $($backend:ident => $config:expr,)* }
            tests $tests:tt
            tests_with_metrics $tests_with_metrics:tt
        ) => {
            $(
                mod $backend {
                    use crate::test::TestEnvironment;
                    use crate::storage::{Storage, StorageKind};
                    use std::sync::Arc;

                    fn get_storage(env: &TestEnvironment) -> Arc<Storage> {
                        env.override_config(|config| {
                            config.storage_backend = $config;
                        });
                        env.storage()
                    }

                    backend_tests!(@tests $tests);
                    backend_tests!(@tests_with_metrics $tests_with_metrics);
                }
            )*
        };
        (@tests { $($test:ident,)* }) => {
            $(
                #[test]
                fn $test() {
                    crate::test::wrapper(|env| {
                        super::$test(&*get_storage(env))
                    });
                }
            )*
        };
        (@tests_with_metrics { $($test:ident,)* }) => {
            $(
                #[test]
                fn $test() {
                    crate::test::wrapper(|env| {
                        super::$test(&*get_storage(env), &*env.metrics())
                    });
                }
            )*
        };
    }

    backend_tests! {
        backends {
            s3 => StorageKind::S3,
            database => StorageKind::Database,
        }

        tests {
            test_batched_uploads,
            test_exists,
            test_get_object,
            test_get_range,
            test_get_too_big,
            test_delete_prefix,
            test_delete_percent,
        }

        tests_with_metrics {
            test_store_blobs,
            test_store_all,
            test_store_all_in_archive,
        }
    }
}
