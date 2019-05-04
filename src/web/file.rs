//! Database based file handler

use super::pool::Pool;
use iron::status;
use iron::{Handler, IronError, IronResult, Request, Response};
use postgres::Connection;

pub struct File {
    pub path: String,
    pub mime: String,
    pub date_added: time::Timespec,
    pub date_updated: time::Timespec,
    pub content: Vec<u8>,
}

impl File {
    /// Gets file from database
    pub fn from_path(conn: &Connection, path: &str) -> Option<File> {
        let rows = conn
            .query(
                "SELECT path, mime, date_added, date_updated, content
                               FROM files
                               WHERE path = $1",
                &[&path],
            )
            .unwrap();

        if rows.len() == 0 {
            None
        } else {
            let row = rows.get(0);
            Some(File {
                path: row.get(0),
                mime: row.get(1),
                date_added: row.get(2),
                date_updated: row.get(3),
                content: row.get(4),
            })
        }
    }

    /// Consumes File and creates a iron response
    pub fn serve(self) -> Response {
        use iron::headers::{CacheControl, CacheDirective, ContentType, HttpDate, LastModified};

        let mut response = Response::with((status::Ok, self.content));
        let cache = vec![
            CacheDirective::Public,
            CacheDirective::MaxAge(super::STATIC_FILE_CACHE_DURATION as u32),
        ];
        response
            .headers
            .set(ContentType(self.mime.parse().unwrap()));
        response.headers.set(CacheControl(cache));
        response
            .headers
            .set(LastModified(HttpDate(time::at(self.date_updated))));
        response
    }

    /// Checks if mime type of file is "application/x-empty"
    pub fn is_empty(&self) -> bool {
        self.mime == "application/x-empty"
    }
}

/// Database based file handler for iron
///
/// This is similar to staticfile crate, but its using getting files from database.
pub struct DatabaseFileHandler;

impl Handler for DatabaseFileHandler {
    fn handle(&self, req: &mut Request<'_, '_>) -> IronResult<Response> {
        let path = req.url.path().join("/");
        let conn = extension!(req, Pool);
        if let Some(file) = File::from_path(&conn, &path) {
            Ok(file.serve())
        } else {
            Err(IronError::new(
                super::error::Nope::CrateNotFound,
                status::NotFound,
            ))
        }
    }
}
