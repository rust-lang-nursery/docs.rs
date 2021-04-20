//! Web interface of docs.rs

pub(crate) mod page;

use log::{debug, info};
use serde_json::Value;

/// ctry! (cratesfyitry) is extremely similar to try! and itry!
/// except it returns an error page response instead of plain Err.
#[macro_export]
macro_rules! ctry {
    ($req:expr, $result:expr $(,)?) => {
        match $result {
            Ok(success) => success,
            Err(error) => {
                let request: &::iron::Request = $req;

                ::log::error!(
                    "called ctry!() on an `Err` value: {:?}\nnote: while attempting to fetch the route {:?}\n{:?}",
                    error,
                    request.url,
                    ::backtrace::Backtrace::new(),
                );

                // This is very ugly, but it makes it impossible to get a type inference error
                // from this macro
                let error = $crate::web::ErrorPage {
                    title: "Internal Server Error",
                    message: ::std::option::Option::Some(::std::borrow::Cow::Owned(
                        ::std::format!("{}", error),
                    )),
                    status: ::iron::status::InternalServerError,
                };

                return $crate::web::page::WebPage::into_response(error, request);
            }
        }
    };
}

/// cexpect will check an option and if it's not Some
/// it will return an error page response
macro_rules! cexpect {
    ($req:expr, $option:expr $(,)?) => {
        match $option {
            Some(success) => success,
            None => {
                let request: &::iron::Request = $req;

                ::log::error!(
                    "called cexpect!() on a `None` value while attempting to fetch the route {:?}\n{:?}",
                    request.url,
                    ::backtrace::Backtrace::new(),
                );

                // This is very ugly, but it makes it impossible to get a type inference error
                // from this macro
                let error = $crate::web::ErrorPage {
                    title: "Internal Server Error",
                    message: None,
                    status: ::iron::status::InternalServerError,
                };

                return $crate::web::page::WebPage::into_response(error, request);
            }
        }
    };
}

/// Gets an extension from Request
macro_rules! extension {
    ($req:expr, $ext:ty) => {{
        // Bind $req so we can have good type errors and avoid re-evaluation
        let request: &::iron::Request = $req;

        cexpect!(request, request.extensions.get::<$ext>())
    }};
}

mod build_details;
mod builds;
mod crate_details;
mod csp;
mod error;
mod extensions;
mod features;
mod file;
pub(crate) mod metrics;
mod releases;
mod routes;
mod rustdoc;
mod sitemap;
mod source;
mod statics;

use crate::{impl_webpage, Context};
use chrono::{DateTime, Utc};
use csp::CspMiddleware;
use error::Nope;
use extensions::InjectExtensions;
use failure::Error;
use iron::{
    self,
    headers::{Expires, HttpDate},
    modifiers::Redirect,
    status,
    status::Status,
    Chain, Handler, Iron, IronError, IronResult, Listening, Request, Response, Url,
};
use page::TemplateData;
use postgres::Client;
use router::NoRoute;
use semver::{Version, VersionReq};
use serde::Serialize;
use std::{borrow::Cow, fmt, net::SocketAddr, sync::Arc};

/// Duration of static files for staticfile and DatabaseFileHandler (in seconds)
const STATIC_FILE_CACHE_DURATION: u64 = 60 * 60 * 24 * 30 * 12; // 12 months

const DEFAULT_BIND: &str = "0.0.0.0:3000";

struct MainHandler {
    shared_resource_handler: Box<dyn Handler>,
    router_handler: Box<dyn Handler>,
    inject_extensions: InjectExtensions,
}

impl MainHandler {
    fn chain<H: Handler>(inject_extensions: InjectExtensions, base: H) -> Chain {
        let mut chain = Chain::new(base);
        chain.link_before(inject_extensions);

        chain.link_before(CspMiddleware);
        chain.link_after(CspMiddleware);

        chain
    }

    fn new(template_data: Arc<TemplateData>, context: &dyn Context) -> Result<MainHandler, Error> {
        let inject_extensions = InjectExtensions::new(context, template_data)?;

        let routes = routes::build_routes();
        let shared_resources =
            Self::chain(inject_extensions.clone(), rustdoc::SharedResourceHandler);
        let router_chain = Self::chain(inject_extensions.clone(), routes.iron_router());

        Ok(MainHandler {
            shared_resource_handler: Box::new(shared_resources),
            router_handler: Box::new(router_chain),
            inject_extensions,
        })
    }
}

impl Handler for MainHandler {
    fn handle(&self, req: &mut Request) -> IronResult<Response> {
        fn if_404(
            e: IronError,
            handle: impl FnOnce() -> IronResult<Response>,
        ) -> IronResult<Response> {
            if e.response.status == Some(status::NotFound) {
                // the routes are ordered from least specific to most; give precedence to the
                // new error message.
                handle()
            } else {
                Err(e)
            }
        }

        // This is kind of a mess.
        //
        // Almost all files should be served through the `router_handler`; eventually
        // `shared_resource_handler` should go through the router too.
        //
        // Unfortunately, combining `shared_resource_handler` with the `router_handler` breaks
        // things, because right now `shared_resource_handler` allows requesting files from *any*
        // subdirectory and the router requires us to give a specific path. Changing them to a
        // specific path means that buggy docs from 2018 will have missing CSS (#1181) so until
        // that's fixed, we need to keep the current (buggy) behavior.
        //
        // It's important that `shared_resource_handler` comes first so that global rustdoc files take
        // precedence over local ones (see #1327).
        self.shared_resource_handler
            .handle(req)
            .or_else(|e| if_404(e, || self.router_handler.handle(req)))
            .or_else(|e| {
                let err = if let Some(err) = e.error.downcast_ref::<error::Nope>() {
                    *err
                } else if e.error.downcast_ref::<NoRoute>().is_some()
                    || e.response.status == Some(status::NotFound)
                {
                    error::Nope::ResourceNotFound
                } else if e.response.status == Some(status::InternalServerError) {
                    log::error!("internal server error: {}", e.error);
                    error::Nope::InternalServerError
                } else {
                    log::error!(
                        "No error page for status {:?}; {}",
                        e.response.status,
                        e.error
                    );
                    // TODO: add in support for other errors that are actually used
                    error::Nope::InternalServerError
                };

                if let error::Nope::ResourceNotFound = err {
                    // print the path of the URL that triggered a 404 error
                    struct DebugPath<'a>(&'a iron::Url);
                    impl<'a> fmt::Display for DebugPath<'a> {
                        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                            for path_elem in self.0.path() {
                                write!(f, "/{}", path_elem)?;
                            }

                            if let Some(query) = self.0.query() {
                                write!(f, "?{}", query)?;
                            }

                            if let Some(hash) = self.0.fragment() {
                                write!(f, "#{}", hash)?;
                            }

                            Ok(())
                        }
                    }

                    debug!("Path not found: {}; {}", DebugPath(&req.url), e.error);
                }

                Self::chain(self.inject_extensions.clone(), err).handle(req)
            })
    }
}

struct MatchVersion {
    /// Represents the crate name that was found when attempting to load a crate release.
    ///
    /// `match_version` will attempt to match a provided crate name against similar crate names with
    /// dashes (`-`) replaced with underscores (`_`) and vice versa.
    pub corrected_name: Option<String>,
    pub version: MatchSemver,
}

impl MatchVersion {
    /// If the matched version was an exact match to the requested crate name, returns the
    /// `MatchSemver` for the query. If the lookup required a dash/underscore conversion, returns
    /// `CrateNotFound`.
    fn assume_exact(self) -> Result<MatchSemver, Nope> {
        if self.corrected_name.is_none() {
            Ok(self.version)
        } else {
            Err(Nope::CrateNotFound)
        }
    }
}

/// Represents the possible results of attempting to load a version requirement.
/// The id (i32) of the release is stored to simplify successive queries.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MatchSemver {
    /// `match_version` was given an exact version, which matched a saved crate version.
    Exact((String, i32)),
    /// `match_version` was given a semver version requirement, which matched the given saved crate
    /// version.
    Semver((String, i32)),
}

impl MatchSemver {
    /// Discard information about whether the loaded version was an exact match, and return the
    /// matched version string and id.
    pub fn into_parts(self) -> (String, i32) {
        match self {
            MatchSemver::Exact((v, i)) | MatchSemver::Semver((v, i)) => (v, i),
        }
    }
}

/// Checks the database for crate releases that match the given name and version.
///
/// `version` may be an exact version number or loose semver version requirement. The return value
/// will indicate whether the given version exactly matched a version number from the database.
///
/// This function will also check for crates where dashes in the name (`-`) have been replaced with
/// underscores (`_`) and vice-versa. The return value will indicate whether the crate name has
/// been matched exactly, or if there has been a "correction" in the name that matched instead.
fn match_version(
    conn: &mut Client,
    name: &str,
    version: Option<&str>,
) -> Result<MatchVersion, Nope> {
    // version is an Option<&str> from router::Router::get, need to decode first
    use iron::url::percent_encoding::percent_decode;

    let req_version = version
        .and_then(|v| percent_decode(v.as_bytes()).decode_utf8().ok())
        .map(|v| {
            if v == "newest" || v == "latest" {
                "*".into()
            } else {
                v
            }
        })
        .unwrap_or_else(|| "*".into());

    let mut corrected_name = None;
    let versions: Vec<(String, i32, bool)> = {
        let query = "SELECT name, version, releases.id, releases.yanked
            FROM releases INNER JOIN crates ON releases.crate_id = crates.id
            WHERE normalize_crate_name(name) = normalize_crate_name($1)";

        let rows = conn.query(query, &[&name]).unwrap();
        let mut rows = rows.iter().peekable();

        if let Some(row) = rows.peek() {
            let db_name = row.get(0);

            if db_name != name {
                corrected_name = Some(db_name);
            }
        };

        rows.map(|row| (row.get(1), row.get(2), row.get(3)))
            .collect()
    };

    if versions.is_empty() {
        return Err(Nope::CrateNotFound);
    }

    // first check for exact match, we can't expect users to use semver in query
    if let Some((version, id, _)) = versions.iter().find(|(vers, _, _)| vers == &req_version) {
        return Ok(MatchVersion {
            corrected_name,
            version: MatchSemver::Exact((version.to_owned(), *id)),
        });
    }

    // Now try to match with semver
    let req_sem_ver = VersionReq::parse(&req_version).map_err(|_| Nope::VersionNotFound)?;

    // we need to sort versions first
    let versions_sem = {
        let mut versions_sem: Vec<(Version, i32)> = Vec::with_capacity(versions.len());

        for version in versions.iter().filter(|(_, _, yanked)| !yanked) {
            // in theory a crate must always have a semver compatible version,
            // but check result just in case
            let version_sem = Version::parse(&version.0).map_err(|err| {
                log::error!(
                    "invalid semver in database for crate {}: {}. Err: {}",
                    name,
                    version.0,
                    err
                );
                Nope::InternalServerError
            })?;
            versions_sem.push((version_sem, version.1));
        }

        versions_sem.sort();
        versions_sem.reverse();
        versions_sem
    };

    if let Some((version, id)) = versions_sem
        .iter()
        .find(|(vers, _)| req_sem_ver.matches(vers))
    {
        return Ok(MatchVersion {
            corrected_name,
            version: MatchSemver::Semver((version.to_string(), *id)),
        });
    }

    // semver is acting weird for '*' (any) range if a crate only has pre-release versions
    // return first non-yanked version if requested version is '*'
    if req_version == "*" {
        return versions_sem
            .first()
            .map(|v| MatchVersion {
                corrected_name,
                version: MatchSemver::Semver((v.0.to_string(), v.1)),
            })
            .ok_or(Nope::VersionNotFound);
    }

    // Since we return with a CrateNotFound earlier if the db reply is empty,
    // we know that versions were returned but none satisfied the version requirement
    Err(Nope::VersionNotFound)
}

/// Wrapper around the Markdown parser and renderer to render markdown
fn render_markdown(text: &str) -> String {
    use comrak::{markdown_to_html, ComrakExtensionOptions, ComrakOptions};

    let options = ComrakOptions {
        extension: ComrakExtensionOptions {
            superscript: true,
            table: true,
            autolink: true,
            tasklist: true,
            strikethrough: true,
            ..ComrakExtensionOptions::default()
        },
        ..ComrakOptions::default()
    };

    markdown_to_html(text, &options)
}

#[must_use = "`Server` blocks indefinitely when dropped"]
pub struct Server {
    inner: Listening,
}

impl Server {
    pub fn start(
        addr: Option<&str>,
        reload_templates: bool,
        context: &dyn Context,
    ) -> Result<Self, Error> {
        // Initialize templates
        let template_data = Arc::new(TemplateData::new(&mut *context.pool()?.get()?)?);
        if reload_templates {
            TemplateData::start_template_reloading(template_data.clone(), context.pool()?);
        }

        let server = Self::start_inner(addr.unwrap_or(DEFAULT_BIND), template_data, context)?;
        info!("Running docs.rs web server on http://{}", server.addr());
        Ok(server)
    }

    fn start_inner(
        addr: &str,
        template_data: Arc<TemplateData>,
        context: &dyn Context,
    ) -> Result<Self, Error> {
        let mut iron = Iron::new(MainHandler::new(template_data, context)?);
        if cfg!(test) {
            iron.threads = 1;
        }
        let inner = iron
            .http(addr)
            .unwrap_or_else(|_| panic!("Failed to bind to socket on {}", addr));

        Ok(Server { inner })
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.inner.socket
    }

    /// Iron is bugged, and it never closes the server even when the listener is dropped. To
    /// avoid never-ending tests this method forgets about the server, leaking it and allowing the
    /// program to end.
    ///
    /// The OS will then close all the dangling servers once the process exits.
    ///
    /// https://docs.rs/iron/0.5/iron/struct.Listening.html#method.close
    #[cfg(test)]
    pub(crate) fn leak(self) {
        std::mem::forget(self.inner);
    }
}

/// Converts Timespec to nice readable relative time string
fn duration_to_str(init: DateTime<Utc>) -> String {
    let now = Utc::now();
    let delta = now.signed_duration_since(init);

    let delta = (
        delta.num_days(),
        delta.num_hours(),
        delta.num_minutes(),
        delta.num_seconds(),
    );

    match delta {
        (days, ..) if days > 5 => format!("{}", init.format("%b %d, %Y")),
        (days @ 2..=5, ..) => format!("{} days ago", days),
        (1, ..) => "one day ago".to_string(),

        (_, hours, ..) if hours > 1 => format!("{} hours ago", hours),
        (_, 1, ..) => "an hour ago".to_string(),

        (_, _, minutes, _) if minutes > 1 => format!("{} minutes ago", minutes),
        (_, _, 1, _) => "one minute ago".to_string(),

        (_, _, _, seconds) if seconds > 0 => format!("{} seconds ago", seconds),
        _ => "just now".to_string(),
    }
}

/// Creates a `Response` which redirects to the given path on the scheme/host/port from the given
/// `Request`.
fn redirect(url: Url) -> Response {
    let mut resp = Response::with((status::Found, Redirect(url)));
    resp.headers.set(Expires(HttpDate(time::now())));

    resp
}

fn redirect_base(req: &Request) -> String {
    // Try to get the scheme from CloudFront first, and then from iron
    let scheme = req
        .headers
        .get_raw("cloudfront-forwarded-proto")
        .and_then(|values| values.get(0))
        .and_then(|value| std::str::from_utf8(value).ok())
        .filter(|proto| *proto == "http" || *proto == "https")
        .unwrap_or_else(|| req.url.scheme());

    // Only include the port if it's needed
    let port = req.url.port();
    if port == 80 {
        format!("{}://{}", scheme, req.url.host())
    } else {
        format!("{}://{}:{}", scheme, req.url.host(), port)
    }
}

/// MetaData used in header
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MetaData {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) description: Option<String>,
    pub(crate) target_name: Option<String>,
    pub(crate) rustdoc_status: bool,
    pub(crate) default_target: String,
    pub(crate) doc_targets: Vec<String>,
    pub(crate) yanked: bool,
}

impl MetaData {
    fn from_crate(conn: &mut Client, name: &str, version: &str) -> Option<MetaData> {
        let rows = conn
            .query(
                "SELECT crates.name,
                       releases.version,
                       releases.description,
                       releases.target_name,
                       releases.rustdoc_status,
                       releases.default_target,
                       releases.doc_targets,
                       releases.yanked
                FROM releases
                INNER JOIN crates ON crates.id = releases.crate_id
                WHERE crates.name = $1 AND releases.version = $2",
                &[&name, &version],
            )
            .unwrap();

        let row = rows.get(0)?;

        Some(MetaData {
            name: row.get(0),
            version: row.get(1),
            description: row.get(2),
            target_name: row.get(3),
            rustdoc_status: row.get(4),
            default_target: row.get(5),
            doc_targets: MetaData::parse_doc_targets(row.get(6)),
            yanked: row.get(7),
        })
    }

    fn parse_doc_targets(targets: Value) -> Vec<String> {
        targets
            .as_array()
            .map(|array| {
                array
                    .iter()
                    .filter_map(|item| item.as_str().map(|s| s.to_owned()))
                    .collect()
            })
            .unwrap_or_else(Vec::new)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ErrorPage {
    /// The title of the page
    pub title: &'static str,
    /// The error message, displayed as a description
    pub message: Option<Cow<'static, str>>,
    #[serde(skip)]
    pub status: Status,
}

impl_webpage! {
    ErrorPage = "error.html",
    status = |err| err.status,
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{docbuilder::DocCoverage, test::*, web::match_version};
    use kuchiki::traits::TendrilSink;
    use serde_json::json;

    fn release(version: &str, env: &TestEnvironment) -> i32 {
        env.fake_release()
            .name("foo")
            .version(version)
            .create()
            .unwrap()
    }

    fn version(v: Option<&str>, db: &TestDatabase) -> Option<String> {
        let version = match_version(&mut db.conn(), "foo", v)
            .ok()?
            .assume_exact()
            .ok()?
            .into_parts()
            .0;
        Some(version)
    }

    #[allow(clippy::unnecessary_wraps)]
    fn semver(version: &'static str) -> Option<String> {
        Some(version.into())
    }

    #[allow(clippy::unnecessary_wraps)]
    fn exact(version: &'static str) -> Option<String> {
        Some(version.into())
    }

    fn clipboard_is_present_for_path(path: &str, web: &TestFrontend) -> bool {
        let data = web.get(path).send().unwrap().text().unwrap();
        let node = kuchiki::parse_html().one(data);
        node.select("#clipboard").unwrap().count() == 1
    }

    #[test]
    fn test_index_returns_success() {
        wrapper(|env| {
            let web = env.frontend();
            assert!(web.get("/").send()?.status().is_success());
            Ok(())
        });
    }

    #[test]
    fn test_doc_coverage_for_crate_pages() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.0.1")
                .source_file("test.rs", &[])
                .doc_coverage(DocCoverage {
                    total_items: 10,
                    documented_items: 6,
                    total_items_needing_examples: 2,
                    items_with_examples: 1,
                })
                .create()?;
            let web = env.frontend();

            let foo_crate = kuchiki::parse_html().one(web.get("/crate/foo/0.0.1").send()?.text()?);
            for value in &["60%", "6", "10", "2", "1"] {
                assert!(foo_crate
                    .select(".pure-menu-item b")
                    .unwrap()
                    .any(|e| e.text_contents().contains(value)));
            }

            let foo_doc = kuchiki::parse_html().one(web.get("/foo/0.0.1/foo").send()?.text()?);
            assert!(foo_doc
                .select(".pure-menu-link b")
                .unwrap()
                .any(|e| e.text_contents().contains("60%")));

            Ok(())
        });
    }

    #[test]
    fn test_show_clipboard_for_crate_pages() {
        wrapper(|env| {
            env.fake_release()
                .name("fake_crate")
                .version("0.0.1")
                .source_file("test.rs", &[])
                .create()
                .unwrap();
            let web = env.frontend();
            assert!(clipboard_is_present_for_path(
                "/crate/fake_crate/0.0.1",
                web
            ));
            assert!(clipboard_is_present_for_path(
                "/crate/fake_crate/0.0.1/source/",
                web
            ));
            assert!(clipboard_is_present_for_path(
                "/fake_crate/0.0.1/fake_crate",
                web
            ));
            Ok(())
        });
    }

    #[test]
    fn test_hide_clipboard_for_non_crate_pages() {
        wrapper(|env| {
            env.fake_release()
                .name("fake_crate")
                .version("0.0.1")
                .create()
                .unwrap();
            let web = env.frontend();
            assert!(!clipboard_is_present_for_path("/about", web));
            assert!(!clipboard_is_present_for_path("/releases", web));
            assert!(!clipboard_is_present_for_path("/", web));
            assert!(!clipboard_is_present_for_path("/not/a/real/path", web));
            Ok(())
        });
    }

    #[test]
    fn standard_library_redirects() {
        wrapper(|env| {
            let web = env.frontend();
            for krate in &["std", "alloc", "core", "proc_macro", "test"] {
                let target = format!("https://doc.rust-lang.org/stable/{}/", krate);

                // with or without slash
                assert_redirect(&format!("/{}", krate), &target, web)?;
                assert_redirect(&format!("/{}/", krate), &target, web)?;
            }

            let target = "https://doc.rust-lang.org/stable/proc_macro/";
            // with or without slash
            assert_redirect("/proc-macro", target, web)?;
            assert_redirect("/proc-macro/", target, web)?;
            Ok(())
        })
    }

    #[test]
    fn binary_docs_redirect_to_crate() {
        wrapper(|env| {
            env.fake_release()
                .name("bat")
                .version("0.2.0")
                .binary(true)
                .create()
                .unwrap();
            let web = env.frontend();
            assert_redirect("/bat/0.2.0", "/crate/bat/0.2.0", web)?;
            assert_redirect("/bat/0.2.0/i686-unknown-linux-gnu", "/crate/bat/0.2.0", web)?;
            /* TODO: this should work (https://github.com/rust-lang/docs.rs/issues/603)
            assert_redirect("/bat/0.2.0/i686-unknown-linux-gnu/bat", "/crate/bat/0.2.0", web)?;
            assert_redirect("/bat/0.2.0/i686-unknown-linux-gnu/bat/", "/crate/bat/0.2.0/", web)?;
            */
            Ok(())
        })
    }

    #[test]
    fn can_view_source() {
        wrapper(|env| {
            env.fake_release()
                .name("regex")
                .version("0.3.0")
                .source_file("src/main.rs", br#"println!("definitely valid rust")"#)
                .create()
                .unwrap();

            let web = env.frontend();
            assert_success("/crate/regex/0.3.0/source/src/main.rs", web)?;
            assert_success("/crate/regex/0.3.0/source", web)?;
            assert_success("/crate/regex/0.3.0/source/src", web)?;
            assert_success("/regex/0.3.0/src/regex/main.rs", web)?;
            Ok(())
        })
    }

    #[test]
    // https://github.com/rust-lang/docs.rs/issues/223
    fn prereleases_are_not_considered_for_semver() {
        wrapper(|env| {
            let db = env.db();
            let version = |v| version(v, db);
            let release = |v| release(v, env);

            release("0.3.1-pre");
            assert_eq!(version(Some("*")), semver("0.3.1-pre"));

            release("0.3.1-alpha");
            assert_eq!(version(Some("0.3.1-alpha")), exact("0.3.1-alpha"));

            release("0.3.0");
            let three = semver("0.3.0");
            assert_eq!(version(None), three);
            // same thing but with "*"
            assert_eq!(version(Some("*")), three);
            // make sure exact matches still work
            assert_eq!(version(Some("0.3.0")), exact("0.3.0"));

            Ok(())
        });
    }

    #[test]
    fn platform_dropdown_not_shown_with_no_targets() {
        wrapper(|env| {
            release("0.1.0", env);
            let web = env.frontend();
            let text = web.get("/foo/0.1.0/foo").send()?.text()?;
            let platform = kuchiki::parse_html()
                .one(text)
                .select(r#"ul > li > a[aria-label="Platform"]"#)
                .unwrap()
                .count();
            assert_eq!(platform, 0);

            // sanity check the test is doing something
            env.fake_release()
                .name("foo")
                .version("0.2.0")
                .add_platform("x86_64-unknown-linux-musl")
                .create()?;
            let text = web.get("/foo/0.2.0/foo").send()?.text()?;
            let platform = kuchiki::parse_html()
                .one(text)
                .select(r#"ul > li > a[aria-label="Platform"]"#)
                .unwrap()
                .count();
            assert_eq!(platform, 1);
            Ok(())
        });
    }

    #[test]
    // https://github.com/rust-lang/docs.rs/issues/221
    fn yanked_crates_are_not_considered() {
        wrapper(|env| {
            let db = env.db();

            let release_id = release("0.3.0", env);
            let query = "UPDATE releases SET yanked = true WHERE id = $1 AND version = '0.3.0'";

            db.conn().query(query, &[&release_id]).unwrap();
            assert_eq!(version(None, db), None);
            assert_eq!(version(Some("0.3"), db), None);

            release("0.1.0+4.1", env);
            assert_eq!(version(Some("0.1.0+4.1"), db), exact("0.1.0+4.1"));
            assert_eq!(version(None, db), semver("0.1.0+4.1"));

            Ok(())
        });
    }

    #[test]
    // vaguely related to https://github.com/rust-lang/docs.rs/issues/395
    fn metadata_has_no_effect() {
        wrapper(|env| {
            let db = env.db();

            release("0.1.0+4.1", env);
            release("0.1.1", env);
            assert_eq!(version(None, db), semver("0.1.1"));
            release("0.5.1+zstd.1.4.4", env);
            assert_eq!(version(None, db), semver("0.5.1+zstd.1.4.4"));
            assert_eq!(version(Some("0.5"), db), semver("0.5.1+zstd.1.4.4"));
            assert_eq!(
                version(Some("0.5.1+zstd.1.4.4"), db),
                exact("0.5.1+zstd.1.4.4")
            );

            Ok(())
        });
    }

    #[test]
    fn serialize_metadata() {
        let mut metadata = MetaData {
            name: "serde".to_string(),
            version: "1.0.0".to_string(),
            description: Some("serde does stuff".to_string()),
            target_name: None,
            rustdoc_status: true,
            default_target: "x86_64-unknown-linux-gnu".to_string(),
            doc_targets: vec![
                "x86_64-unknown-linux-gnu".to_string(),
                "arm64-unknown-linux-gnu".to_string(),
            ],
            yanked: false,
        };

        let correct_json = json!({
            "name": "serde",
            "version": "1.0.0",
            "description": "serde does stuff",
            "target_name": null,
            "rustdoc_status": true,
            "default_target": "x86_64-unknown-linux-gnu",
            "doc_targets": [
                "x86_64-unknown-linux-gnu",
                "arm64-unknown-linux-gnu",
            ],
            "yanked": false,
        });

        assert_eq!(correct_json, serde_json::to_value(&metadata).unwrap());

        metadata.target_name = Some("serde_lib_name".to_string());
        let correct_json = json!({
            "name": "serde",
            "version": "1.0.0",
            "description": "serde does stuff",
            "target_name": "serde_lib_name",
            "rustdoc_status": true,
            "default_target": "x86_64-unknown-linux-gnu",
            "doc_targets": [
                "x86_64-unknown-linux-gnu",
                "arm64-unknown-linux-gnu",
            ],
            "yanked": false,
        });

        assert_eq!(correct_json, serde_json::to_value(&metadata).unwrap());

        metadata.description = None;
        let correct_json = json!({
            "name": "serde",
            "version": "1.0.0",
            "description": null,
            "target_name": "serde_lib_name",
            "rustdoc_status": true,
            "default_target": "x86_64-unknown-linux-gnu",
            "doc_targets": [
                "x86_64-unknown-linux-gnu",
                "arm64-unknown-linux-gnu",
            ],
            "yanked": false,
        });

        assert_eq!(correct_json, serde_json::to_value(&metadata).unwrap());
    }

    #[test]
    fn test_tabindex_is_present_on_topbar_crate_search_input() {
        wrapper(|env| {
            release("0.1.0", env);
            let web = env.frontend();
            let text = web.get("/foo/0.1.0/foo").send()?.text()?;
            let tabindex = kuchiki::parse_html()
                .one(text)
                .select(r#"#nav-search[tabindex="-1"]"#)
                .unwrap()
                .count();
            assert_eq!(tabindex, 1);
            Ok(())
        });
    }
}
