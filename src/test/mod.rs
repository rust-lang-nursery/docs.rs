mod fakes;

use crate::web::Server;
use failure::Error;
use once_cell::unsync::OnceCell;
use postgres::Connection;
use reqwest::{Client, Method, RequestBuilder};
use std::sync::{Arc, Mutex, MutexGuard};
use std::panic;

pub(crate) fn wrapper(f: impl FnOnce(&TestEnvironment) -> Result<(), Error>) {
    let env = TestEnvironment::new();
    // if we didn't catch the panic, the server would hang forever
    let maybe_panic = panic::catch_unwind(panic::AssertUnwindSafe(|| f(&env)));
    env.cleanup();
    let result = match maybe_panic {
        Ok(r) => r,
        Err(payload) => panic::resume_unwind(payload),
    };

    if let Err(err) = result {
        eprintln!("the test failed: {}", err);
        for cause in err.iter_causes() {
            eprintln!("  caused by: {}", cause);
        }

        eprintln!("{}", err.backtrace());

        panic!("the test failed");
    }
}

/// Make sure that a URL returns a status code between 200-299
pub(crate) fn assert_success(path: &str, web: &TestFrontend) -> Result<(), Error> {
    let status = web.get(path).send()?.status();
    assert!(status.is_success(), "failed to GET {}: {}", path, status);
    Ok(())
}

/// Make sure that a URL redirects to a specific page
pub(crate) fn assert_redirect(path: &str, expected_target: &str, web: &TestFrontend) -> Result<(), Error> {
    // Reqwest follows redirects automatically
    let response = web.get(path).send()?;
    let status = response.status();

    let mut tmp;
    let redirect_target = if expected_target.starts_with("https://") {
        response.url().as_str()
    } else {
        tmp = String::from(response.url().path());
        if let Some(query) = response.url().query() {
            tmp.push('?');
            tmp.push_str(query);
        }
        &tmp
    };
    // Either we followed a redirect to the wrong place, or there was no redirect
    if redirect_target != expected_target {
        // wrong place
        if redirect_target != path {
            panic!("{}: expected redirect to {}, got redirect to {}",
                   path, expected_target, redirect_target);
        } else {
            // no redirect
            panic!("{}: expected redirect to {}, got {}", path, expected_target, status);
        }
    }
    assert!(status.is_success(), "failed to GET {}: {}", expected_target, status);
    Ok(())
}

pub(crate) struct TestEnvironment {
    db: OnceCell<TestDatabase>,
    frontend: OnceCell<TestFrontend>,
}

impl TestEnvironment {
    fn new() -> Self {
        Self {
            db: OnceCell::new(),
            frontend: OnceCell::new(),
        }
    }

    fn cleanup(self) {
        if let Some(frontend) = self.frontend.into_inner() {
            frontend.server.leak();
        }
    }

    pub(crate) fn db(&self) -> &TestDatabase {
        self.db
            .get_or_init(|| TestDatabase::new().expect("failed to initialize the db"))
    }

    pub(crate) fn frontend(&self) -> &TestFrontend {
        self.frontend.get_or_init(|| TestFrontend::new(self.db()))
    }
}

pub(crate) struct TestDatabase {
    conn: Arc<Mutex<Connection>>,
}

impl TestDatabase {
    fn new() -> Result<Self, Error> {
        // The temporary migration uses CREATE TEMPORARY TABLE instead of CREATE TABLE, creating
        // fresh temporary copies of the database on top of the real one. The temporary tables are
        // only visible to this connection, and will be deleted when it exits.
        let conn = crate::db::connect_db()?;
        crate::db::migrate_temporary(None, &conn)?;

        Ok(TestDatabase {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub(crate) fn conn(&self) -> MutexGuard<Connection> {
        self.conn.lock().expect("failed to lock the connection")
    }

    pub(crate) fn fake_release(&self) -> fakes::FakeRelease {
        fakes::FakeRelease::new(self)
    }
}

pub(crate) struct TestFrontend {
    server: Server,
    client: Client,
}

impl TestFrontend {
    fn new(db: &TestDatabase) -> Self {
        Self {
            server: Server::start_test(db.conn.clone()),
            client: Client::new(),
        }
    }

    fn build_request(&self, method: Method, url: &str) -> RequestBuilder {
        self.client
            .request(method, &format!("http://{}{}", self.server.addr(), url))
    }

    pub(crate) fn get(&self, url: &str) -> RequestBuilder {
        self.build_request(Method::GET, url)
    }
}
