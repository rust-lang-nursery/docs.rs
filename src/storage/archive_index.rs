use crate::error::Result;
use crate::storage::{compression::CompressionAlgorithm, FileRange};
use failure::ResultExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize)]
pub struct FileInfo {
    range: FileRange,
    compression: CompressionAlgorithm,
}

impl FileInfo {
    pub fn range(&self) -> FileRange {
        self.range.clone()
    }
    pub fn compression(&self) -> CompressionAlgorithm {
        self.compression
    }
}

#[derive(Deserialize, Serialize)]
pub struct Index {
    files: HashMap<PathBuf, FileInfo>,
}

impl Index {
    pub fn load(reader: impl io::Read) -> Result<Index> {
        serde_cbor::from_reader(reader)
            .context("deserialization error")
            .map_err(Into::into)
    }

    pub fn save(&self, writer: impl io::Write) -> Result<()> {
        serde_cbor::to_writer(writer, self)
            .context("serialization error")
            .map_err(Into::into)
    }

    pub fn new_from_zip<R: io::Read + io::Seek>(zipfile: &mut R) -> Result<Index> {
        let mut archive = zip::ZipArchive::new(zipfile)?;

        // get file locations
        let mut files: HashMap<PathBuf, FileInfo> = HashMap::with_capacity(archive.len());
        for i in 0..archive.len() {
            let zf = archive.by_index(i)?;

            files.insert(
                PathBuf::from(zf.name()),
                FileInfo {
                    range: FileRange::new(
                        zf.data_start(),
                        zf.data_start() + zf.compressed_size() - 1,
                    ),
                    compression: match zf.compression() {
                        zip::CompressionMethod::Bzip2 => CompressionAlgorithm::Bzip2,
                        c => failure::bail!("unsupported compression algorithm {} in zip-file", c),
                    },
                },
            );
        }

        Ok(Index { files })
    }

    pub fn find_file<P: AsRef<Path>>(&self, path: P) -> Result<&FileInfo> {
        self.files
            .get(path.as_ref())
            .ok_or_else(|| super::PathNotFoundError.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::FileOptions;

    fn validate_index(index: &Index) {
        assert_eq!(index.files.len(), 1);

        let fi = index.files.get(&PathBuf::from("testfile1")).unwrap();
        assert_eq!(fi.range, FileRange::new(39, 459));
        assert_eq!(fi.compression, CompressionAlgorithm::Bzip2);
    }

    #[test]
    fn index_create_save_load() {
        let mut tf = tempfile::tempfile().unwrap();

        let objectcontent: Vec<u8> = (0..255).collect();

        let mut archive = zip::ZipWriter::new(tf);
        archive
            .start_file(
                "testfile1",
                FileOptions::default().compression_method(zip::CompressionMethod::Bzip2),
            )
            .unwrap();
        archive.write_all(&objectcontent).unwrap();
        tf = archive.finish().unwrap();

        let index = Index::new_from_zip(&mut tf).unwrap();
        validate_index(&index);

        let mut buf = Vec::new();
        index.save(&mut buf).unwrap();

        let new_index = Index::load(io::Cursor::new(&buf)).unwrap();
        validate_index(&new_index);
    }
}
