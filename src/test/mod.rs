mod fakes;

use crate::db::{Pool, PoolConnection};
use crate::storage::Storage;
use crate::web::Server;
use crate::BuildQueue;
use crate::Config;
use failure::Error;
use log::error;
use once_cell::unsync::OnceCell;
use postgres::Connection;
use reqwest::{
    blocking::{Client, RequestBuilder},
    Method,
};
use std::{panic, sync::Arc};

pub(crate) fn wrapper(f: impl FnOnce(&TestEnvironment) -> Result<(), Error>) {
    let _ = dotenv::dotenv();

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
pub(crate) fn assert_redirect(
    path: &str,
    expected_target: &str,
    web: &TestFrontend,
) -> Result<(), Error> {
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
            panic!(
                "{}: expected redirect to {}, got redirect to {}",
                path, expected_target, redirect_target
            );
        } else {
            // no redirect
            panic!(
                "{}: expected redirect to {}, got {}",
                path, expected_target, status
            );
        }
    }
    assert!(
        status.is_success(),
        "failed to GET {}: {}",
        expected_target,
        status
    );
    Ok(())
}

pub(crate) struct TestEnvironment {
    build_queue: OnceCell<Arc<BuildQueue>>,
    config: OnceCell<Arc<Config>>,
    db: OnceCell<TestDatabase>,
    storage: OnceCell<Arc<Storage>>,
    frontend: OnceCell<TestFrontend>,
    s3: OnceCell<Arc<Storage>>,
    storage_db: OnceCell<Arc<Storage>>,
}

pub(crate) fn init_logger() {
    // If this fails it's probably already initialized
    let _ = env_logger::from_env(env_logger::Env::default().filter("DOCSRS_LOG"))
        .is_test(true)
        .try_init();
}

impl TestEnvironment {
    fn new() -> Self {
        init_logger();
        Self {
            build_queue: OnceCell::new(),
            config: OnceCell::new(),
            db: OnceCell::new(),
            storage: OnceCell::new(),
            frontend: OnceCell::new(),
            s3: OnceCell::new(),
            storage_db: OnceCell::new(),
        }
    }

    fn cleanup(self) {
        if let Some(frontend) = self.frontend.into_inner() {
            frontend.server.leak();
        }
        if let Some(storage) = self.storage.get() {
            storage
                .cleanup_after_test()
                .expect("failed to cleanup after tests");
        }
    }

    fn base_config(&self) -> Config {
        let mut config = Config::from_env().expect("failed to get base config");

        // Use less connections for each test compared to production.
        config.max_pool_size = 2;
        config.min_pool_idle = 0;

        // Use a temporary S3 bucket.
        config.s3_bucket = format!("docsrs-test-bucket-{}", rand::random::<u64>());
        config.s3_bucket_is_temporary = true;

        config
    }

    pub(crate) fn override_config(&self, f: impl FnOnce(&mut Config)) {
        let mut config = self.base_config();
        f(&mut config);

        if self.config.set(Arc::new(config)).is_err() {
            panic!("can't call override_config after the configuration is accessed!");
        }
    }

    pub(crate) fn build_queue(&self) -> Arc<BuildQueue> {
        self.build_queue
            .get_or_init(|| Arc::new(BuildQueue::new(self.db().pool(), &self.config())))
            .clone()
    }

    pub(crate) fn config(&self) -> Arc<Config> {
        self.config
            .get_or_init(|| Arc::new(self.base_config()))
            .clone()
    }

    pub(crate) fn storage(&self) -> Arc<Storage> {
        self.storage
            .get_or_init(|| {
                Arc::new(
                    Storage::new(self.db().pool(), &*self.config())
                        .expect("failed to initialize the storage"),
                )
            })
            .clone()
    }

    pub(crate) fn db(&self) -> &TestDatabase {
        self.db
            .get_or_init(|| TestDatabase::new(&self.config()).expect("failed to initialize the db"))
    }

    pub(crate) fn frontend(&self) -> &TestFrontend {
        self.frontend.get_or_init(|| {
            TestFrontend::new(self.db(), self.config(), self.build_queue(), self.storage())
        })
    }

    pub(crate) fn s3(&self) -> Arc<Storage> {
        self.s3
            .get_or_init(|| {
                Arc::new(
                    Storage::temp_new_s3(&*self.config())
                        .expect("failed to initialize the storage"),
                )
            })
            .clone()
    }

    pub(crate) fn temp_storage_db(&self) -> Arc<Storage> {
        self.storage_db
            .get_or_init(|| {
                Arc::new(
                    Storage::temp_new_db(self.db().pool())
                        .expect("failed to initialize the storage"),
                )
            })
            .clone()
    }

    pub(crate) fn fake_release(&self) -> fakes::FakeRelease {
        fakes::FakeRelease::new(self.db(), self.storage())
    }
}

pub(crate) struct TestDatabase {
    pool: Pool,
    schema: String,
}

impl TestDatabase {
    fn new(config: &Config) -> Result<Self, Error> {
        // A random schema name is generated and used for the current connection. This allows each
        // test to create a fresh instance of the database to run within.
        let schema = format!("docs_rs_test_schema_{}", rand::random::<u64>());

        let conn = Connection::connect(config.database_url.as_str(), postgres::TlsMode::None)?;
        conn.batch_execute(&format!(
            "
                CREATE SCHEMA {0};
                SET search_path TO {0}, public;
            ",
            schema
        ))?;
        crate::db::migrate(None, &conn)?;

        // Move all sequence start positions 10000 apart to avoid overlapping primary keys
        let query: String = conn
            .query(
                "
                    SELECT relname
                    FROM pg_class
                    INNER JOIN pg_namespace
                        ON pg_class.relnamespace = pg_namespace.oid
                    WHERE pg_class.relkind = 'S'
                        AND pg_namespace.nspname = $1
                ",
                &[&schema],
            )?
            .into_iter()
            .map(|row| row.get(0))
            .enumerate()
            .map(|(i, sequence): (_, String)| {
                let offset = (i + 1) * 10000;
                format!(r#"ALTER SEQUENCE "{}" RESTART WITH {};"#, sequence, offset)
            })
            .collect();
        conn.batch_execute(&query)?;

        Ok(TestDatabase {
            pool: Pool::new_with_schema(config, &schema)?,
            schema,
        })
    }

    pub(crate) fn pool(&self) -> Pool {
        self.pool.clone()
    }

    pub(crate) fn conn(&self) -> PoolConnection {
        self.pool
            .get()
            .expect("failed to get a connection out of the pool")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        crate::db::migrate(Some(0), &self.conn()).expect("downgrading database works");
        if let Err(e) = self
            .conn()
            .execute(&format!("DROP SCHEMA {} CASCADE;", self.schema), &[])
        {
            error!("failed to drop test schema {}: {}", self.schema, e);
        }
    }
}

pub(crate) struct TestFrontend {
    server: Server,
    client: Client,
}

impl TestFrontend {
    fn new(
        db: &TestDatabase,
        config: Arc<Config>,
        build_queue: Arc<BuildQueue>,
        storage: Arc<Storage>,
    ) -> Self {
        Self {
            server: Server::start(
                Some("127.0.0.1:0"),
                false,
                db.pool.clone(),
                config,
                build_queue,
                storage,
            )
            .expect("failed to start the web server"),
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
