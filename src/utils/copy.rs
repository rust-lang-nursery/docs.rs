use std::fs;
use std::io;
use std::path::Path;

/// cp -r src dst
pub(crate) fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    let dst = dst.as_ref();
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let filename = entry.file_name();
        if entry.file_type()?.is_dir() {
            copy_dir_all(entry.path(), dst.join(filename))?;
        } else {
            fs::copy(entry.path(), dst.join(filename))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use std::fs;

    #[test]
    fn test_copy_doc_dir() {
        let source = tempfile::Builder::new()
            .prefix("docsrs-src")
            .tempdir()
            .unwrap();
        let destination = tempfile::Builder::new()
            .prefix("docsrs-dst")
            .tempdir()
            .unwrap();
        let doc = source.path().join("doc");
        fs::create_dir(&doc).unwrap();
        fs::create_dir(doc.join("inner")).unwrap();

        fs::write(doc.join("index.html"), "<html>spooky</html>").unwrap();
        fs::write(doc.join("inner").join("index.html"), "<html>spooky</html>").unwrap();

        // lets try to copy a src directory to tempdir
        copy_dir_all(source.path().join("doc"), destination.path()).unwrap();
        assert!(destination.path().join("index.html").exists());
        assert!(destination.path().join("inner").join("index.html").exists());
    }
}
