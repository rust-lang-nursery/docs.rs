use super::metrics::RequestRecorder;
use iron::middleware::Handler;
use router::Router;
use std::collections::HashSet;

pub(super) const DOC_RUST_LANG_ORG_REDIRECTS: &[&str] =
    &["alloc", "core", "proc_macro", "std", "test"];

// REFACTOR: Break this into smaller initialization functions
pub(super) fn build_routes() -> Routes {
    let mut routes = Routes::new();

    // Well known resources, robots.txt and favicon.ico support redirection, the sitemap.xml
    // must live at the site root:
    //   https://developers.google.com/search/reference/robots_txt#handling-http-result-codes
    //   https://support.google.com/webmasters/answer/183668?hl=en
    routes.static_resource("/robots.txt", PermanentRedirect("/-/static/robots.txt"));
    routes.static_resource("/favicon.ico", PermanentRedirect("/-/static/favicon.ico"));
    routes.static_resource("/sitemap.xml", super::sitemap::sitemapindex_handler);
    routes.static_resource(
        "/-/sitemap/:letter/sitemap.xml",
        super::sitemap::sitemap_handler,
    );

    // This should not need to be served from the root as we reference the inner path in links,
    // but clients might have cached the url and need to update it.
    routes.static_resource(
        "/opensearch.xml",
        PermanentRedirect("/-/static/opensearch.xml"),
    );

    routes.static_resource("/-/static/:single", super::statics::static_handler);
    routes.static_resource("/-/static/*", super::statics::static_handler);
    routes.internal_page("/-/storage-change-detection.html", {
        #[derive(Debug, serde::Serialize)]
        struct StorageChangeDetection {}
        crate::impl_webpage!(StorageChangeDetection = "storage-change-detection.html");
        fn storage_change_detection(req: &mut iron::Request) -> iron::IronResult<iron::Response> {
            crate::web::page::WebPage::into_response(StorageChangeDetection {}, req)
        }
        storage_change_detection
    });

    routes.internal_page("/", super::releases::home_page);

    routes.internal_page("/about", super::sitemap::about_handler);
    routes.internal_page("/about/metrics", super::metrics::metrics_handler);
    routes.internal_page("/about/builds", super::sitemap::about_builds_handler);
    routes.internal_page("/about/:subpage", super::sitemap::about_handler);

    routes.internal_page("/releases", super::releases::recent_releases_handler);
    routes.static_resource("/releases/feed", super::releases::releases_feed_handler);
    routes.internal_page("/releases/:owner", super::releases::owner_handler);
    routes.internal_page("/releases/:owner/:page", super::releases::owner_handler);
    routes.internal_page("/releases/activity", super::releases::activity_handler);
    routes.internal_page("/releases/search", super::releases::search_handler);
    routes.internal_page("/releases/queue", super::releases::build_queue_handler);
    routes.internal_page(
        "/releases/recent/:page",
        super::releases::recent_releases_handler,
    );
    routes.internal_page(
        "/releases/stars",
        super::releases::releases_by_stars_handler,
    );
    routes.internal_page(
        "/releases/stars/:page",
        super::releases::releases_by_stars_handler,
    );
    routes.internal_page(
        "/releases/recent-failures",
        super::releases::releases_recent_failures_handler,
    );
    routes.internal_page(
        "/releases/recent-failures/:page",
        super::releases::releases_recent_failures_handler,
    );
    routes.internal_page(
        "/releases/failures",
        super::releases::releases_failures_by_stars_handler,
    );
    routes.internal_page(
        "/releases/failures/:page",
        super::releases::releases_failures_by_stars_handler,
    );

    routes.internal_page("/crate/:name", super::crate_details::crate_details_handler);
    routes.internal_page(
        "/crate/:name/:version",
        super::crate_details::crate_details_handler,
    );
    routes.internal_page(
        "/crate/:name/:version/builds",
        super::builds::build_list_handler,
    );
    routes.static_resource(
        "/crate/:name/:version/builds.json",
        super::builds::build_list_handler,
    );
    routes.internal_page(
        "/crate/:name/:version/builds/:id",
        super::build_details::build_details_handler,
    );
    routes.internal_page(
        "/crate/:name/:version/features",
        super::features::build_features_handler,
    );
    routes.internal_page(
        "/crate/:name/:version/source",
        SimpleRedirect::new(|url| url.set_path(&format!("{}/", url.path()))),
    );
    routes.internal_page(
        "/crate/:name/:version/source/",
        super::source::source_browser_handler,
    );
    routes.internal_page(
        "/crate/:name/:version/source/*",
        super::source::source_browser_handler,
    );
    routes.internal_page(
        "/crate/:name/:version/target-redirect/*",
        super::rustdoc::target_redirect_handler,
    );

    routes.rustdoc_page("/:crate", super::rustdoc::rustdoc_redirector_handler);
    routes.rustdoc_page("/:crate/", super::rustdoc::rustdoc_redirector_handler);
    routes.rustdoc_page("/:crate/badge.svg", super::rustdoc::badge_handler);
    routes.rustdoc_page(
        "/:crate/:version",
        super::rustdoc::rustdoc_redirector_handler,
    );
    routes.rustdoc_page(
        "/:crate/:version/",
        super::rustdoc::rustdoc_redirector_handler,
    );
    routes.rustdoc_page(
        "/:crate/:version/settings.html",
        super::rustdoc::rustdoc_html_server_handler,
    );
    routes.rustdoc_page(
        "/:crate/:version/all.html",
        super::rustdoc::rustdoc_html_server_handler,
    );
    routes.rustdoc_page(
        "/:crate/:version/:target",
        super::rustdoc::rustdoc_redirector_handler,
    );
    routes.rustdoc_page(
        "/:crate/:version/:target/",
        super::rustdoc::rustdoc_html_server_handler,
    );
    routes.rustdoc_page(
        "/:crate/:version/:target/*.html",
        super::rustdoc::rustdoc_html_server_handler,
    );

    for redirect in DOC_RUST_LANG_ORG_REDIRECTS {
        routes.internal_page(
            &format!("/{}", redirect),
            super::rustdoc::RustLangRedirector::new(redirect),
        );
    }
    // redirect proc-macro to proc_macro
    routes.internal_page(
        "/proc-macro",
        super::rustdoc::RustLangRedirector::new("proc_macro"),
    );

    routes
}

/// This wrapper class aids the construction of iron's Router, with docs.rs-specific additions to
/// it. Routes are supposed to be added by the build_routes function, which calls methods in this
/// struct depending on the type of route being added.
pub(super) struct Routes {
    /// Normal GET routes.
    get: Vec<(String, Box<dyn Handler>)>,
    /// GET routes serving rustdoc content. The BlockBlacklistedPrefixes middleware is added
    /// automatically to all of them.
    rustdoc_get: Vec<(String, Box<dyn Handler>)>,
    /// Prefixes of all the internal routes. This data is used to power the
    /// BlockBlacklistedPrefixes middleware.
    page_prefixes: HashSet<String>,
}

impl Routes {
    fn new() -> Self {
        Self {
            get: Vec::new(),
            rustdoc_get: Vec::new(),
            page_prefixes: HashSet::new(),
        }
    }

    pub(super) fn page_prefixes(&self) -> HashSet<String> {
        self.page_prefixes.clone()
    }

    pub(super) fn iron_router(mut self) -> Router {
        let mut router = Router::new();
        for (pattern, handler) in self.get.drain(..) {
            router.get(&pattern, handler, calculate_id(&pattern));
        }

        // All rustdoc pages have the prefixes of other docs.rs pages blacklisted. This prevents,
        // for example, a crate named "about" from hijacking /about/0.1.0/index.html.
        let blacklist = self.page_prefixes();
        for (pattern, handler) in self.rustdoc_get.drain(..) {
            router.get(
                &pattern,
                BlockBlacklistedPrefixes::new(blacklist.clone(), handler),
                calculate_id(&pattern),
            );
        }

        router
    }

    /// A static resource is a normal page without any special behavior on the router side.
    fn static_resource(&mut self, pattern: &str, handler: impl Handler) {
        self.get.push((
            pattern.to_string(),
            Box::new(RequestRecorder::new(handler, "static resource")),
        ));
    }

    /// Internal pages are docs.rs's own pages, instead of the documentation of a crate uploaded by
    /// an user. The router adds these extra things when adding a new internal page:
    ///
    /// - The first component of the page's URL will be registered as a "page prefix". Page
    /// prefixes are blacklisted by rustdoc pages, to prevent a crate named the same as a prefix
    /// from hijacking docs.rs's own URLs.
    ///
    /// - If the page URL doesn't end with a slash, a redirect from the URL with the trailing slash
    /// to the one without is automatically added.
    fn internal_page(&mut self, pattern: &str, handler: impl Handler) {
        self.get.push((
            pattern.to_string(),
            Box::new(RequestRecorder::new(handler, pattern)),
        ));

        // Automatically add another route ending with / that redirects to the slash-less route.
        if !pattern.ends_with('/') {
            let pattern = format!("{}/", pattern);
            self.get.push((
                pattern.to_string(),
                Box::new(RequestRecorder::new(
                    SimpleRedirect::new(|url| {
                        url.set_path(&url.path().trim_end_matches('/').to_string())
                    }),
                    pattern,
                )),
            ));
        }

        // Register the prefix if it's not the home page and the first path component is not a
        // pattern or a wildcard.
        if pattern != "/" {
            if let Some(first_component) = pattern.trim_matches('/').split('/').next() {
                if !first_component.contains('*') && !first_component.starts_with(':') {
                    self.page_prefixes.insert(first_component.to_string());
                }
            }
        }
    }

    /// A rustdoc page is a page serving generated documentation. It's similar to a static
    /// resource, but path prefixes are automatically blacklisted (see internal pages to learn more
    /// about page prefixes).
    fn rustdoc_page(&mut self, pattern: &str, handler: impl Handler) {
        self.get.push((
            pattern.to_string(),
            Box::new(RequestRecorder::new(handler, "rustdoc page")),
        ));
    }
}

#[derive(Copy, Clone)]
struct SimpleRedirect {
    url_mangler: fn(&mut iron::url::Url),
}

impl SimpleRedirect {
    fn new(url_mangler: fn(&mut iron::url::Url)) -> Self {
        Self { url_mangler }
    }
}

impl Handler for SimpleRedirect {
    fn handle(&self, req: &mut iron::Request) -> iron::IronResult<iron::Response> {
        let mut url: iron::url::Url = req.url.clone().into();
        (self.url_mangler)(&mut url);
        Ok(iron::Response::with((
            iron::status::Found,
            iron::modifiers::Redirect(iron::Url::from_generic_url(url).unwrap()),
        )))
    }
}

#[derive(Copy, Clone)]
struct PermanentRedirect(&'static str);

impl Handler for PermanentRedirect {
    fn handle(&self, _req: &mut iron::Request) -> iron::IronResult<iron::Response> {
        Ok(iron::Response::with((
            iron::status::MovedPermanently,
            iron::modifiers::RedirectRaw(self.0.to_owned()),
        )))
    }
}

/// Iron Middleware that prevents requests to blacklisted prefixes.
///
/// In our application, a prefix is blacklisted if a docs.rs page exists below it. For example,
/// since /releases/queue is a docs.rs page, /releases is a blacklisted prefix.
///
/// The middleware must be used for all the pages serving crates at the top level, to prevent a
/// crate from putting their own content in an URL that's supposed to be used by docs.rs.
pub(super) struct BlockBlacklistedPrefixes {
    blacklist: HashSet<String>,
    handler: Box<dyn Handler>,
}

impl BlockBlacklistedPrefixes {
    pub(super) fn new(blacklist: HashSet<String>, handler: Box<dyn Handler>) -> Self {
        Self { blacklist, handler }
    }
}

impl Handler for BlockBlacklistedPrefixes {
    fn handle(&self, req: &mut iron::Request) -> iron::IronResult<iron::Response> {
        if let Some(prefix) = req.url.path().get(0) {
            if self.blacklist.contains(*prefix) {
                return Err(super::error::Nope::CrateNotFound.into());
            }
        }
        self.handler.handle(req)
    }
}

/// Automatically generate a Route ID from a pattern. Every non-alphanumeric character is replaced
/// with `_`.
fn calculate_id(pattern: &str) -> String {
    let calculate_char = |c: char| {
        if c.is_alphanumeric() || c == '-' {
            c
        } else {
            '_'
        }
    };

    pattern.chars().map(calculate_char).collect()
}

#[cfg(test)]
mod tests {
    use crate::test::*;

    #[test]
    fn test_root_redirects() {
        wrapper(|env| {
            // These are "well-known" resources that will be requested from the root, but support
            // redirection
            assert_redirect("/favicon.ico", "/-/static/favicon.ico", env.frontend())?;
            assert_redirect("/robots.txt", "/-/static/robots.txt", env.frontend())?;

            // This has previously been served with a url pointing to the root, it may be
            // plausible to remove the redirects in the future, but for now we need to keep serving
            // it.
            assert_redirect(
                "/opensearch.xml",
                "/-/static/opensearch.xml",
                env.frontend(),
            )?;

            Ok(())
        });
    }
}
