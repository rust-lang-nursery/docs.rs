//! Releases web handlers

use crate::{
    build_queue::QueuedCrate,
    db::{Pool, PoolClient},
    impl_webpage,
    web::{error::Nope, match_version, page::WebPage, redirect_base},
    BuildQueue, Config,
};
use chrono::{DateTime, NaiveDate, Utc};
use iron::{
    headers::{ContentType, Expires, HttpDate},
    mime::{Mime, SubLevel, TopLevel},
    modifiers::Redirect,
    status, IronResult, Request, Response, Url,
};
use log::{debug, trace};
use postgres::Client;
use router::Router;
use serde::{Deserialize, Serialize};

/// Number of release in home page
const RELEASES_IN_HOME: i64 = 15;
/// Releases in /releases page
const RELEASES_IN_RELEASES: i64 = 30;
/// Releases in recent releases feed
const RELEASES_IN_FEED: i64 = 150;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Release {
    pub(crate) name: String,
    pub(crate) version: String,
    description: Option<String>,
    target_name: Option<String>,
    rustdoc_status: bool,
    pub(crate) release_time: DateTime<Utc>,
    stars: i32,
}

impl Default for Release {
    fn default() -> Release {
        Release {
            name: String::new(),
            version: String::new(),
            description: None,
            target_name: None,
            rustdoc_status: false,
            release_time: Utc::now(),
            stars: 0,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum Order {
    ReleaseTime, // this is default order
    GithubStars,
    RecentFailures,
    FailuresByGithubStars,
}

impl Default for Order {
    fn default() -> Self {
        Self::ReleaseTime
    }
}

pub(crate) fn get_releases(conn: &mut Client, page: i64, limit: i64, order: Order) -> Vec<Release> {
    let offset = (page - 1) * limit;

    // WARNING: it is _crucial_ that this always be hard-coded and NEVER be user input
    let (ordering, filter_failed): (&'static str, _) = match order {
        Order::ReleaseTime => ("releases.release_time", false),
        Order::GithubStars => ("repositories.stars", false),
        Order::RecentFailures => ("releases.release_time", true),
        Order::FailuresByGithubStars => ("repositories.stars", true),
    };
    let query = format!(
        "SELECT crates.name,
            releases.version,
            releases.description,
            releases.target_name,
            releases.release_time,
            releases.rustdoc_status,
            repositories.stars
        FROM crates
        INNER JOIN releases ON crates.latest_version_id = releases.id
        LEFT JOIN repositories ON releases.repository_id = repositories.id
        WHERE
            ((NOT $3) OR (releases.build_status = FALSE AND releases.is_library = TRUE)) 
            AND {0} IS NOT NULL

        ORDER BY {0} DESC
        LIMIT $1 OFFSET $2",
        ordering,
    );

    conn.query(query.as_str(), &[&limit, &offset, &filter_failed])
        .unwrap()
        .into_iter()
        .map(|row| Release {
            name: row.get(0),
            version: row.get(1),
            description: row.get(2),
            target_name: row.get(3),
            release_time: row.get(4),
            rustdoc_status: row.get(5),
            stars: row.get::<_, Option<i32>>(6).unwrap_or(0),
        })
        .collect()
}

fn get_releases_by_owner(
    conn: &mut Client,
    page: i64,
    limit: i64,
    owner: &str,
) -> (String, Vec<Release>) {
    let offset = (page - 1) * limit;

    let query = "SELECT crates.name,
                        releases.version,
                        releases.description,
                        releases.target_name,
                        releases.release_time,
                        releases.rustdoc_status,
                        repositories.stars,
                        owners.name,
                        owners.login
                 FROM crates
                 INNER JOIN releases ON releases.id = crates.latest_version_id
                 INNER JOIN owner_rels ON owner_rels.cid = crates.id
                 INNER JOIN owners ON owners.id = owner_rels.oid
                 LEFT JOIN repositories ON releases.repository_id = repositories.id
                 WHERE owners.login = $1
                 ORDER BY repositories.stars DESC NULLS LAST
                 LIMIT $2 OFFSET $3";
    let query = conn.query(query, &[&owner, &limit, &offset]).unwrap();

    let mut owner_name = None;
    let packages = query
        .into_iter()
        .map(|row| {
            if owner_name.is_none() {
                owner_name = Some(if !row.get::<usize, String>(7).is_empty() {
                    row.get(7)
                } else {
                    row.get(8)
                });
            }

            Release {
                name: row.get(0),
                version: row.get(1),
                description: row.get(2),
                target_name: row.get(3),
                release_time: row.get(4),
                rustdoc_status: row.get(5),
                stars: row.get::<_, Option<i32>>(6).unwrap_or(0),
            }
        })
        .collect();

    (owner_name.unwrap_or_default(), packages)
}

/// Get the search results for a crate search query
///
/// This delegates to the crates.io search API.
fn get_search_results(
    conn: &mut Client,
    query: &str,
    page: i64,
    limit: i64,
) -> Result<(u64, Vec<Release>), failure::Error> {
    #[derive(Deserialize)]
    struct CratesIoReleases {
        crates: Vec<CratesIoRelease>,
        meta: CratesIoMeta,
    }
    #[derive(Deserialize, Debug)]
    struct CratesIoRelease {
        name: String,
        max_version: String,
    }
    #[derive(Deserialize)]
    struct CratesIoMeta {
        total: u64,
    }

    use crate::utils::APP_USER_AGENT;
    use once_cell::sync::Lazy;
    use reqwest::blocking::Client as HttpClient;
    use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, USER_AGENT};

    static HTTP_CLIENT: Lazy<HttpClient> = Lazy::new(|| {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(APP_USER_AGENT));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        HttpClient::builder()
            .default_headers(headers)
            .build()
            .unwrap()
    });

    let url = url::Url::parse_with_params(
        "https://crates.io/api/v1/crates",
        &[
            ("q", query),
            ("page", &page.to_string()),
            ("per_page", &limit.to_string()),
        ],
    )?;
    debug!("fetching search results from {}", url);
    let releases: CratesIoReleases = HTTP_CLIENT.get(url).send()?.json()?;
    let (names_and_versions, names): (Vec<_>, Vec<_>) = releases
        .crates
        .into_iter()
        // The `postgres` crate doesn't support anonymous records.
        // Use strings instead.
        // Additionally, looking at both the name and version doesn't allow using the index;
        // first filter by crate name so the query is more efficient.
        .map(|krate| (format!("{}:{}", krate.name, krate.max_version), krate.name))
        .unzip();
    trace!("crates.io search results {:#?}", names_and_versions);
    let crates = conn
        .query(
            "
        SELECT
            crates.name,
            releases.version,
            releases.description,
            releases.release_time,
            releases.target_name,
            releases.rustdoc_status,
            github_repos.stars
        FROM crates INNER JOIN releases ON crates.id = releases.crate_id
                    LEFT JOIN github_repos ON releases.github_repo = github_repos.id
        WHERE crates.name = ANY($1) AND crates.name || ':' || releases.version = ANY($2)
        ",
            &[&names, &names_and_versions],
        )?
        .into_iter()
        .map(|row| {
            let stars: Option<_> = row.get("stars");
            Release {
                name: row.get("name"),
                version: row.get("version"),
                description: row.get("description"),
                release_time: row.get("release_time"),
                target_name: row.get("target_name"),
                rustdoc_status: row.get("rustdoc_status"),
                stars: stars.unwrap_or(0),
            }
        })
        .collect();
    Ok((releases.meta.total, crates))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct HomePage {
    recent_releases: Vec<Release>,
}

impl_webpage! {
    HomePage = "core/home.html",
}

pub fn home_page(req: &mut Request) -> IronResult<Response> {
    let mut conn = extension!(req, Pool).get()?;
    let recent_releases = get_releases(&mut conn, 1, RELEASES_IN_HOME, Order::ReleaseTime);

    HomePage { recent_releases }.into_response(req)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReleaseFeed {
    recent_releases: Vec<Release>,
}

impl_webpage! {
    ReleaseFeed  = "releases/feed.xml",
    content_type = ContentType(Mime(TopLevel::Application, SubLevel::Xml, vec![])),
}

pub fn releases_feed_handler(req: &mut Request) -> IronResult<Response> {
    let mut conn = extension!(req, Pool).get()?;
    let recent_releases = get_releases(&mut conn, 1, RELEASES_IN_FEED, Order::ReleaseTime);

    ReleaseFeed { recent_releases }.into_response(req)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ViewReleases {
    releases: Vec<Release>,
    description: String,
    release_type: ReleaseType,
    show_next_page: bool,
    show_previous_page: bool,
    page_number: i64,
    owner: Option<String>,
}

impl_webpage! {
    ViewReleases = "releases/releases.html",
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ReleaseType {
    Recent,
    Stars,
    RecentFailures,
    Failures,
    Owner,
    Search,
}

fn releases_handler(req: &mut Request, release_type: ReleaseType) -> IronResult<Response> {
    let page_number: i64 = extension!(req, Router)
        .find("page")
        .and_then(|page_num| page_num.parse().ok())
        .unwrap_or(1);

    let (description, release_order) = match release_type {
        ReleaseType::Recent => ("Recently uploaded crates", Order::ReleaseTime),
        ReleaseType::Stars => ("Crates with most stars", Order::GithubStars),
        ReleaseType::RecentFailures => ("Recent crates failed to build", Order::RecentFailures),
        ReleaseType::Failures => (
            "Crates with most stars failed to build",
            Order::FailuresByGithubStars,
        ),

        ReleaseType::Owner | ReleaseType::Search => panic!(
            "The owners and search page have special requirements and cannot use this handler",
        ),
    };

    let releases = {
        let mut conn = extension!(req, Pool).get()?;
        get_releases(&mut conn, page_number, RELEASES_IN_RELEASES, release_order)
    };

    // Show next and previous page buttons
    let (show_next_page, show_previous_page) = (
        releases.len() == RELEASES_IN_RELEASES as usize,
        page_number != 1,
    );

    ViewReleases {
        releases,
        description: description.into(),
        release_type,
        show_next_page,
        show_previous_page,
        page_number,
        owner: None,
    }
    .into_response(req)
}

pub fn recent_releases_handler(req: &mut Request) -> IronResult<Response> {
    releases_handler(req, ReleaseType::Recent)
}

pub fn releases_by_stars_handler(req: &mut Request) -> IronResult<Response> {
    releases_handler(req, ReleaseType::Stars)
}

pub fn releases_recent_failures_handler(req: &mut Request) -> IronResult<Response> {
    releases_handler(req, ReleaseType::RecentFailures)
}

pub fn releases_failures_by_stars_handler(req: &mut Request) -> IronResult<Response> {
    releases_handler(req, ReleaseType::Failures)
}

pub fn owner_handler(req: &mut Request) -> IronResult<Response> {
    let router = extension!(req, Router);
    // page number of releases
    let page_number: i64 = router
        .find("page")
        .and_then(|page_num| page_num.parse().ok())
        .unwrap_or(1);
    let owner_route_value = router.find("owner").unwrap();

    let (owner_name, releases) = {
        let mut conn = extension!(req, Pool).get()?;

        // We need to keep the owner_route_value unchanged, as we may render paginated links in the page.
        // Changing the owner_route_value directly will cause the link to change, for example: @foobar -> foobar.
        let mut owner = owner_route_value;
        if owner.starts_with('@') {
            owner = &owner[1..];
        }
        get_releases_by_owner(&mut conn, page_number, RELEASES_IN_RELEASES, owner)
    };

    if releases.is_empty() {
        return Err(Nope::OwnerNotFound.into());
    }

    // Show next and previous page buttons
    let (show_next_page, show_previous_page) = (
        releases.len() == RELEASES_IN_RELEASES as usize,
        page_number != 1,
    );

    ViewReleases {
        releases,
        description: format!("Crates from {}", owner_name),
        release_type: ReleaseType::Owner,
        show_next_page,
        show_previous_page,
        page_number,
        owner: Some(owner_route_value.into()),
    }
    .into_response(req)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct Search {
    pub(super) title: String,
    #[serde(rename = "releases")]
    pub(super) results: Vec<Release>,
    pub(super) search_query: Option<String>,
    pub(super) previous_page_button: bool,
    pub(super) next_page_button: bool,
    pub(super) current_page: i64,
    /// This should always be `ReleaseType::Search`
    pub(super) release_type: ReleaseType,
    #[serde(skip)]
    pub(super) status: iron::status::Status,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            title: String::default(),
            results: Vec::default(),
            search_query: None,
            previous_page_button: false,
            next_page_button: false,
            current_page: 0,
            release_type: ReleaseType::Search,
            status: iron::status::Ok,
        }
    }
}

fn redirect_to_random_crate(req: &Request, conn: &mut PoolClient) -> IronResult<Response> {
    // We try to find a random crate and redirect to it.
    //
    // The query is efficient, but relies on a static factor which depends
    // on the amount of crates with > 100 GH stars over the amount of all crates.
    //
    // If random-crate-searches end up being empty, increase that value.

    let config = extension!(req, Config);
    let rows = ctry!(
        req,
        conn.query(
            "WITH params AS (
                    -- get maximum possible id-value in crates-table
                    SELECT last_value AS max_id FROM crates_id_seq
                )
                SELECT
                    crates.name,
                    releases.version,
                    releases.target_name
                FROM (
                    -- generate random numbers in the ID-range. 
                    SELECT DISTINCT 1 + trunc(random() * params.max_id)::INTEGER AS id
                    FROM params, generate_series(1, $1)
                ) AS r
                INNER JOIN crates ON r.id = crates.id
                INNER JOIN releases ON crates.latest_version_id = releases.id
                INNER JOIN repositories ON releases.repository_id = repositories.id
                WHERE
                    releases.rustdoc_status = TRUE AND
                    repositories.stars >= 100
                LIMIT 1",
            &[&(config.random_crate_search_view_size as i32)]
        )
    );

    if let Some(row) = rows.into_iter().next() {
        let name: String = row.get("name");
        let version: String = row.get("version");
        let target_name: String = row.get("target_name");
        let url = ctry!(
            req,
            Url::parse(&format!(
                "{}/{}/{}/{}/",
                redirect_base(req),
                name,
                version,
                target_name
            )),
        );

        let metrics = extension!(req, crate::Metrics).clone();
        metrics.im_feeling_lucky_searches.inc();

        Ok(super::redirect(url))
    } else {
        log::error!("found no result in random crate search");
        Err(Nope::NoResults.into())
    }
}

impl_webpage! {
    Search = "releases/releases.html",
    status = |search| search.status,
}

pub fn search_handler(req: &mut Request) -> IronResult<Response> {
    let url = req.url.as_ref();
    let mut params = url.query_pairs();
    let query = params.find(|(key, _)| key == "query");
    let mut conn = extension!(req, Pool).get()?;

    if let Some((_, query)) = query {
        // check if I am feeling lucky button pressed and redirect user to crate page
        // if there is a match
        // NOTE: calls `query_pairs()` again because iterators are lazy and only yield items once
        if url
            .query_pairs()
            .any(|(key, _)| key == "i-am-feeling-lucky")
        {
            // redirect to a random crate if query is empty
            if query.is_empty() {
                return redirect_to_random_crate(req, &mut conn);
            }

            // since we never pass a version into `match_version` here, we'll never get
            // `MatchVersion::Exact`, so the distinction between `Exact` and `Semver` doesn't
            // matter
            if let Ok(matchver) = match_version(&mut conn, &query, None) {
                let (version, id) = matchver.version.into_parts();
                let query = matchver.corrected_name.unwrap_or_else(|| query.to_string());

                // FIXME: This is a super dirty way to check if crate have rustdocs generated.
                //        match_version should handle this instead of this code block.
                //        This block is introduced to fix #163
                let rustdoc_status = {
                    let rows = ctry!(
                        req,
                        conn.query(
                            "SELECT rustdoc_status
                         FROM releases
                         WHERE releases.id = $1",
                            &[&id]
                        ),
                    );

                    rows.into_iter()
                        .next()
                        .map(|r| r.get("rustdoc_status"))
                        .unwrap_or_default()
                };

                let url = if rustdoc_status {
                    ctry!(
                        req,
                        Url::parse(&format!("{}/{}/{}/", redirect_base(req), query, version)),
                    )
                } else {
                    ctry!(
                        req,
                        Url::parse(&format!(
                            "{}/crate/{}/{}",
                            redirect_base(req),
                            query,
                            version,
                        )),
                    )
                };

                let mut resp = Response::with((status::Found, Redirect(url)));
                resp.headers.set(Expires(HttpDate(time::now())));

                return Ok(resp);
            }
        }

        let (_, results) = ctry!(
            req,
            get_search_results(&mut conn, &query, 1, RELEASES_IN_RELEASES)
        );
        let title = if results.is_empty() {
            format!("No results found for '{}'", query)
        } else {
            format!("Search results for '{}'", query)
        };

        // FIXME: There is no pagination
        Search {
            title,
            results,
            search_query: Some(query.into_owned()),
            ..Default::default()
        }
        .into_response(req)
    } else {
        Err(Nope::NoResults.into())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ReleaseActivity {
    description: &'static str,
    dates: Vec<String>,
    counts: Vec<i64>,
    failures: Vec<i64>,
}

impl_webpage! {
    ReleaseActivity = "releases/activity.html",
}

pub fn activity_handler(req: &mut Request) -> IronResult<Response> {
    let mut conn = extension!(req, Pool).get()?;

    let data: Vec<(NaiveDate, i64, i64)> = ctry!(
        req,
        conn.query(
            "
            WITH dates AS (
                -- we need this series so that days in the statistic that don't have any releases are included
                SELECT generate_series( 
                        CURRENT_DATE - INTERVAL '30 days',
                        CURRENT_DATE - INTERVAL '1 day',
                        '1 day'::interval
                    )::date AS date_
            ), 
            release_stats AS (
                SELECT
                    release_time::date AS date_,
                    COUNT(*) AS counts,
                    SUM(CAST((is_library = TRUE AND build_status = FALSE) AS INT)) AS failures
                FROM
                    releases
                WHERE
                    release_time >= CURRENT_DATE - INTERVAL '30 days' AND
                    release_time < CURRENT_DATE
                GROUP BY
                    release_time::date
            ) 
            SELECT 
                dates.date_ AS date,
                COALESCE(rs.counts, 0) AS counts,
                COALESCE(rs.failures, 0) AS failures 
            FROM
                dates 
                LEFT OUTER JOIN Release_stats AS rs ON dates.date_ = rs.date_

            ORDER BY 
                dates.date_
            ",
            &[],
        )
    )
    .into_iter()
    .map(|row| (row.get(0), row.get(1), row.get(2)))
    .collect();

    ReleaseActivity {
        description: "Monthly release activity",
        dates: data
            .iter()
            .map(|&d| d.0.format("%d %b").to_string())
            .collect(),
        counts: data.iter().map(|&d| d.1).collect(),
        failures: data.iter().map(|&d| d.2).collect(),
    }
    .into_response(req)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct BuildQueuePage {
    description: &'static str,
    queue: Vec<QueuedCrate>,
}

impl_webpage! {
    BuildQueuePage = "releases/build_queue.html",
}

pub fn build_queue_handler(req: &mut Request) -> IronResult<Response> {
    let mut queue = ctry!(req, extension!(req, BuildQueue).queued_crates());
    for krate in queue.iter_mut() {
        // The priority here is inverted: in the database if a crate has a higher priority it
        // will be built after everything else, which is counter-intuitive for people not
        // familiar with docs.rs's inner workings.
        krate.priority = -krate.priority;
    }

    BuildQueuePage {
        description: "List of crates scheduled to build",
        queue,
    }
    .into_response(req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::api::CrateOwner;
    use crate::test::{assert_redirect, assert_success, wrapper, TestFrontend};
    use chrono::{Duration, TimeZone};
    use failure::Error;
    use kuchiki::traits::TendrilSink;
    use std::collections::HashSet;

    #[test]
    fn get_releases_by_stars() {
        wrapper(|env| {
            let db = env.db();

            env.fake_release()
                .name("foo")
                .version("1.0.0")
                .github_stats("ghost/foo", 10, 10, 10)
                .create()?;
            env.fake_release()
                .name("bar")
                .version("1.0.0")
                .github_stats("ghost/bar", 20, 20, 20)
                .create()?;
            // release without stars will not be shown
            env.fake_release().name("baz").version("1.0.0").create()?;

            let releases = get_releases(&mut db.conn(), 1, 10, Order::GithubStars);
            assert_eq!(
                vec![
                    "bar", // 20 stars
                    "foo", // 10 stars
                ],
                releases
                    .iter()
                    .map(|release| release.name.as_str())
                    .collect::<Vec<_>>(),
            );

            Ok(())
        })
    }

    #[test]
    fn database_search() {
        wrapper(|env| {
            let db = env.db();

            env.fake_release().name("foo").version("0.0.0").create()?;
            env.fake_release()
                .name("bar-foo")
                .version("0.0.0")
                .create()?;
            env.fake_release()
                .name("foo-bar")
                .version("0.0.1")
                .create()?;
            env.fake_release().name("fo0").version("0.0.0").create()?;
            env.fake_release()
                .name("fool")
                .version("0.0.0")
                .build_result_failed()
                .create()?;
            env.fake_release()
                .name("freakin")
                .version("0.0.0")
                .create()?;
            env.fake_release()
                .name("something unreleated")
                .version("0.0.0")
                .create()?;

            let (num_results, results) = get_search_results(&mut db.conn(), "foo", 1, 100)?;
            assert_eq!(num_results, 4);

            let mut results = results.into_iter();

            let expected = ["foo", "fo0", "bar-foo", "foo-bar"];
            for expected in expected.iter() {
                assert_eq!(expected, &results.next().unwrap().name);
            }
            assert_eq!(results.count(), 0);

            Ok(())
        })
    }

    #[test]
    fn exacts_dont_care() {
        wrapper(|env| {
            let db = env.db();

            let releases = ["regex", "regex-", "regex-syntax"];
            for release in releases.iter() {
                env.fake_release().name(release).version("0.0.0").create()?;
            }

            let near_matches = ["Regex", "rEgex", "reGex", "regEx", "regeX"];

            for name in near_matches.iter() {
                let (num_results, mut results) =
                    dbg!(get_search_results(&mut db.conn(), *name, 1, 100))?;
                assert_eq!(num_results, 3);

                for name in releases.iter() {
                    assert_eq!(results.remove(0).name, *name);
                }
                assert!(results.is_empty());
            }

            Ok(())
        })
    }

    #[test]
    fn unsuccessful_not_shown() {
        wrapper(|env| {
            let db = env.db();
            env.fake_release()
                .name("regex")
                .version("0.0.0")
                .build_result_failed()
                .create()?;

            let (num_results, results) = get_search_results(&mut db.conn(), "regex", 1, 100)?;
            assert_eq!(num_results, 0);

            let results = results.into_iter();
            assert_eq!(results.count(), 0);

            Ok(())
        })
    }

    #[test]
    fn yanked_not_shown() {
        wrapper(|env| {
            let db = env.db();
            env.fake_release()
                .name("regex")
                .version("0.0.0")
                .yanked(true)
                .create()?;

            let (num_results, results) = get_search_results(&mut db.conn(), "regex", 1, 100)?;
            assert_eq!(num_results, 0);

            let results = results.into_iter();
            assert_eq!(results.count(), 0);

            Ok(())
        })
    }

    #[test]
    fn fuzzily_match() {
        wrapper(|env| {
            let db = env.db();
            env.fake_release().name("regex").version("0.0.0").create()?;

            let (num_results, results) = get_search_results(&mut db.conn(), "redex", 1, 100)?;
            assert_eq!(num_results, 1);

            let mut results = results.into_iter();
            assert_eq!(results.next().unwrap().name, "regex");
            assert_eq!(results.count(), 0);

            Ok(())
        })
    }

    // Description searching more than doubles search time
    // #[test]
    // fn search_descriptions() {
    //     wrapper(|env| {
    //         let db = env.db();
    //         env.fake_release()
    //             .name("something_completely_unrelated")
    //             .description("Supercalifragilisticexpialidocious")
    //             .create()?;
    //
    //         let (num_results, results) =
    //             get_search_results(&mut db.conn(), "supercalifragilisticexpialidocious", 1, 100)?;
    //         assert_eq!(num_results, 1);
    //
    //         let mut results = results.into_iter();
    //         assert_eq!(
    //             results.next().unwrap().name,
    //             "something_completely_unrelated"
    //         );
    //         assert_eq!(results.count(), 0);
    //
    //         Ok(())
    //     })
    // }

    #[test]
    fn search_limits() {
        wrapper(|env| {
            let db = env.db();

            env.fake_release().name("something_magical").create()?;
            env.fake_release().name("something_sinister").create()?;
            env.fake_release().name("something_fantastical").create()?;
            env.fake_release()
                .name("something_completely_unrelated")
                .create()?;

            let (num_results, results) = get_search_results(&mut db.conn(), "something", 1, 2)?;
            assert_eq!(num_results, 4);

            let mut results = results.into_iter();
            assert_eq!(results.next().unwrap().name, "something_magical");
            assert_eq!(results.next().unwrap().name, "something_sinister");
            assert_eq!(results.count(), 0);

            Ok(())
        })
    }

    #[test]
    fn search_offsets() {
        wrapper(|env| {
            let db = env.db();
            env.fake_release().name("something_magical").create()?;
            env.fake_release().name("something_sinister").create()?;
            env.fake_release().name("something_fantastical").create()?;
            env.fake_release()
                .name("something_completely_unrelated")
                .create()?;

            let (num_results, results) = get_search_results(&mut db.conn(), "something", 2, 2)?;
            assert_eq!(num_results, 4);

            let mut results = results.into_iter();
            assert_eq!(results.next().unwrap().name, "something_fantastical");
            assert_eq!(
                results.next().unwrap().name,
                "something_completely_unrelated",
            );
            assert_eq!(results.count(), 0);

            Ok(())
        })
    }

    #[test]
    fn release_dates() {
        wrapper(|env| {
            let db = env.db();
            env.fake_release()
                .name("somethang")
                .release_time(Utc.ymd(2021, 4, 16).and_hms(4, 33, 50))
                .version("0.3.0")
                .description("this is the correct choice")
                .create()?;
            env.fake_release()
                .name("somethang")
                .release_time(Utc.ymd(2020, 4, 16).and_hms(4, 33, 50))
                .description("second")
                .version("0.2.0")
                .create()?;
            env.fake_release()
                .name("somethang")
                .release_time(Utc.ymd(2019, 4, 16).and_hms(4, 33, 50))
                .description("third")
                .version("0.1.0")
                .create()?;
            env.fake_release()
                .name("somethang")
                .release_time(Utc.ymd(2018, 4, 16).and_hms(4, 33, 50))
                .description("fourth")
                .version("0.0.0")
                .create()?;

            let (num_results, results) = get_search_results(&mut db.conn(), "somethang", 1, 100)?;
            assert_eq!(num_results, 1);

            let mut results = results.into_iter();
            assert_eq!(
                results.next().unwrap().description,
                Some("this is the correct choice".into()),
            );
            assert_eq!(results.count(), 0);

            Ok(())
        })
    }

    // Description searching more than doubles search time
    // #[test]
    // fn fuzzy_over_description() {
    //     wrapper(|env| {
    //         let db = env.db();
    //         env.fake_release()
    //             .name("name_better_than_description")
    //             .description("this is the correct choice")
    //             .create()?;
    //         env.fake_release()
    //             .name("im_completely_unrelated")
    //             .description("name_better_than_description")
    //             .create()?;
    //         env.fake_release()
    //             .name("i_have_zero_relation_whatsoever")
    //             .create()?;
    //
    //         let (num_results, results) =
    //             get_search_results(&mut db.conn(), "name_better_than_description", 1, 100)?;
    //         assert_eq!(num_results, 2);
    //
    //         let mut results = results.into_iter();
    //
    //         let next = results.next().unwrap();
    //         assert_eq!(next.name, "name_better_than_description");
    //         assert_eq!(next.description, Some("this is the correct choice".into()));
    //
    //         let next = results.next().unwrap();
    //         assert_eq!(next.name, "im_completely_unrelated");
    //         assert_eq!(
    //             next.description,
    //             Some("name_better_than_description".into())
    //         );
    //
    //         assert_eq!(results.count(), 0);
    //
    //         Ok(())
    //     })
    // }

    #[test]
    fn dont_return_unrelated() {
        wrapper(|env| {
            let db = env.db();
            env.fake_release().name("match").create()?;
            env.fake_release().name("matcher").create()?;
            env.fake_release().name("matchest").create()?;
            env.fake_release()
                .name("i_am_useless_and_mean_nothing")
                .create()?;

            let (num_results, results) = get_search_results(&mut db.conn(), "match", 1, 100)?;
            assert_eq!(num_results, 3);

            let mut results = results.into_iter();
            assert_eq!(results.next().unwrap().name, "match");
            assert_eq!(results.next().unwrap().name, "matcher");
            assert_eq!(results.next().unwrap().name, "matchest");
            assert_eq!(results.count(), 0);

            Ok(())
        })
    }

    #[test]
    fn order_by_downloads() {
        wrapper(|env| {
            let db = env.db();
            env.fake_release().name("matca").downloads(100).create()?;
            env.fake_release().name("matcb").downloads(10).create()?;
            env.fake_release().name("matcc").downloads(1).create()?;

            let (num_results, results) = get_search_results(&mut db.conn(), "match", 1, 100)?;
            assert_eq!(num_results, 3);

            let mut results = results.into_iter();
            assert_eq!(results.next().unwrap().name, "matca");
            assert_eq!(results.next().unwrap().name, "matcb");
            assert_eq!(results.next().unwrap().name, "matcc");
            assert_eq!(results.count(), 0);

            Ok(())
        })
    }

    #[test]
    fn im_feeling_lucky_with_stars() {
        wrapper(|env| {
            {
                // The normal test-setup will offset all primary sequences by 10k
                // to prevent errors with foreign key relations.
                // Random-crate-search relies on the sequence for the crates-table
                // to find a maximum possible ID. This combined with only one actual
                // crate in the db breaks this test.
                // That's why we reset the id-sequence to zero for this test.

                let mut conn = env.db().conn();
                conn.execute(r#"ALTER SEQUENCE crates_id_seq RESTART WITH 1"#, &[])?;
            }

            let web = env.frontend();
            env.fake_release()
                .github_stats("some/repo", 333, 22, 11)
                .name("some_random_crate")
                .create()?;
            assert_redirect(
                "/releases/search?query=&i-am-feeling-lucky=1",
                "/some_random_crate/1.0.0/some_random_crate/",
                web,
            )?;
            Ok(())
        })
    }

    #[test]
    fn search() {
        wrapper(|env| {
            let web = env.frontend();
            env.fake_release().name("some_random_crate").create()?;

            let links = get_release_links("/releases/search?query=some_random_crate", web)?;

            assert_eq!(links.len(), 1);
            assert_eq!(links[0], "/some_random_crate/1.0.0/some_random_crate/",);
            Ok(())
        })
    }

    fn get_release_links(path: &str, web: &TestFrontend) -> Result<Vec<String>, Error> {
        let response = web.get(path).send()?;
        assert!(response.status().is_success());

        let page = kuchiki::parse_html().one(response.text()?);

        Ok(page
            .select("a.release")
            .expect("missing heading")
            .map(|el| {
                let attributes = el.attributes.borrow();
                attributes.get("href").unwrap().to_string()
            })
            .collect())
    }

    #[test]
    fn releases_by_stars() {
        wrapper(|env| {
            env.fake_release()
                .name("crate_that_succeeded_with_github")
                .version("0.1.0")
                .github_stats("some/repo", 66, 22, 11)
                .release_time(Utc.ymd(2020, 4, 16).and_hms(4, 33, 50))
                .create()?;

            env.fake_release()
                .name("crate_that_succeeded_with_github")
                .version("0.2.0")
                .github_stats("some/repo", 66, 22, 11)
                .release_time(Utc.ymd(2020, 4, 20).and_hms(4, 33, 50))
                .create()?;

            env.fake_release()
                .name("crate_that_succeeded_without_github")
                .release_time(Utc.ymd(2020, 5, 16).and_hms(4, 33, 50))
                .version("0.2.0")
                .create()?;

            env.fake_release()
                .name("crate_that_failed_with_github")
                .version("0.1.0")
                .github_stats("some/repo", 33, 22, 11)
                .release_time(Utc.ymd(2020, 6, 16).and_hms(4, 33, 50))
                .build_result_failed()
                .create()?;

            let links = get_release_links("/releases/stars", env.frontend())?;

            // output is sorted by stars, not release-time
            assert_eq!(links.len(), 2);
            assert_eq!(
                links[0],
                "/crate_that_succeeded_with_github/0.2.0/crate_that_succeeded_with_github/"
            );
            assert_eq!(links[1], "/crate/crate_that_failed_with_github/0.1.0");

            Ok(())
        })
    }

    #[test]
    fn failures_by_stars() {
        wrapper(|env| {
            env.fake_release()
                .name("crate_that_succeeded_with_github")
                .version("0.1.0")
                .github_stats("some/repo", 66, 22, 11)
                .release_time(Utc.ymd(2020, 4, 16).and_hms(4, 33, 50))
                .create()?;

            env.fake_release()
                .name("crate_that_succeeded_with_github")
                .version("0.2.0")
                .github_stats("some/repo", 66, 22, 11)
                .release_time(Utc.ymd(2020, 4, 20).and_hms(4, 33, 50))
                .create()?;

            env.fake_release()
                .name("crate_that_succeeded_without_github")
                .release_time(Utc.ymd(2020, 5, 16).and_hms(4, 33, 50))
                .version("0.2.0")
                .create()?;

            env.fake_release()
                .name("crate_that_failed_with_github")
                .version("0.1.0")
                .github_stats("some/repo", 33, 22, 11)
                .release_time(Utc.ymd(2020, 6, 16).and_hms(4, 33, 50))
                .build_result_failed()
                .create()?;

            let links = get_release_links("/releases/failures", env.frontend())?;

            // output is sorted by stars, not release-time
            assert_eq!(links.len(), 1);
            assert_eq!(links[0], "/crate/crate_that_failed_with_github/0.1.0");

            Ok(())
        })
    }

    #[test]
    fn releases_failed_by_time() {
        wrapper(|env| {
            env.fake_release()
                .name("crate_that_succeeded_with_github")
                .version("0.1.0")
                .github_stats("some/repo", 33, 22, 11)
                .release_time(Utc.ymd(2020, 4, 16).and_hms(4, 33, 50))
                .create()?;
            // make sure that crates get at most one release shown, so they don't crowd the page
            env.fake_release()
                .name("crate_that_succeeded_with_github")
                .github_stats("some/repo", 33, 22, 11)
                .release_time(Utc.ymd(2020, 5, 16).and_hms(4, 33, 50))
                .version("0.2.0")
                .create()?;
            env.fake_release()
                .name("crate_that_failed")
                .version("0.1.0")
                .release_time(Utc.ymd(2020, 6, 16).and_hms(4, 33, 50))
                .build_result_failed()
                .create()?;

            let links = get_release_links("/releases/recent-failures", env.frontend())?;

            assert_eq!(links.len(), 1);
            assert_eq!(links[0], "/crate/crate_that_failed/0.1.0");

            Ok(())
        })
    }

    #[test]
    fn releases_homepage_and_recent() {
        wrapper(|env| {
            env.fake_release()
                .name("crate_that_succeeded_with_github")
                .version("0.1.0")
                .github_stats("some/repo", 33, 22, 11)
                .release_time(Utc.ymd(2020, 4, 16).and_hms(4, 33, 50))
                .create()?;
            // make sure that crates get at most one release shown, so they don't crowd the page
            env.fake_release()
                .name("crate_that_succeeded_with_github")
                .github_stats("some/repo", 33, 22, 11)
                .release_time(Utc.ymd(2020, 5, 16).and_hms(4, 33, 50))
                .version("0.2.0")
                .create()?;
            env.fake_release()
                .name("crate_that_failed")
                .version("0.1.0")
                .release_time(Utc.ymd(2020, 6, 16).and_hms(4, 33, 50))
                .build_result_failed()
                .create()?;

            for page in &["/", "/releases"] {
                let links = get_release_links(page, env.frontend())?;

                assert_eq!(links.len(), 2);
                assert_eq!(links[0], "/crate/crate_that_failed/0.1.0");
                assert_eq!(
                    links[1],
                    "/crate_that_succeeded_with_github/0.2.0/crate_that_succeeded_with_github/"
                );
            }

            Ok(())
        })
    }

    #[test]
    fn release_activity() {
        wrapper(|env| {
            let web = env.frontend();

            let empty_data = format!("data: [{}]", vec!["0"; 30].join(","));

            // no data / only zeros without releases
            let response = web.get("/releases/activity/").send()?;
            assert!(response.status().is_success());
            assert_eq!(response.text().unwrap().matches(&empty_data).count(), 2);

            env.fake_release().name("some_random_crate").create()?;
            env.fake_release()
                .name("some_random_crate_that_failed")
                .build_result_failed()
                .create()?;

            // same when the release is on the current day, since we ignore today.
            let response = web.get("/releases/activity/").send()?;
            assert!(response.status().is_success());
            assert_eq!(response.text().unwrap().matches(&empty_data).count(), 2);

            env.fake_release()
                .name("some_random_crate_yesterday")
                .release_time(Utc::now() - Duration::days(1))
                .create()?;
            env.fake_release()
                .name("some_random_crate_that_failed_yesterday")
                .build_result_failed()
                .release_time(Utc::now() - Duration::days(1))
                .create()?;

            // with releases yesterday we get the data we want
            let response = web.get("/releases/activity/").send()?;
            assert!(response.status().is_success());
            let text = response.text().unwrap();
            // counts contain both releases
            assert!(text.contains(&format!("data: [{},2]", vec!["0"; 29].join(","))));
            // failures only one
            assert!(text.contains(&format!("data: [{},1]", vec!["0"; 29].join(","))));

            Ok(())
        })
    }

    #[test]
    fn release_feed() {
        wrapper(|env| {
            let web = env.frontend();
            assert_success("/releases/feed", web)?;

            env.fake_release().name("some_random_crate").create()?;
            env.fake_release()
                .name("some_random_crate_that_failed")
                .build_result_failed()
                .create()?;
            assert_success("/releases/feed", web)
        })
    }

    #[test]
    fn test_releases_queue() {
        wrapper(|env| {
            let queue = env.build_queue();
            let web = env.frontend();

            let empty = kuchiki::parse_html().one(web.get("/releases/queue").send()?.text()?);
            assert!(empty
                .select(".release > strong")
                .expect("missing heading")
                .any(|el| el.text_contents().contains("nothing")));

            queue.add_crate("foo", "1.0.0", 0, None)?;
            queue.add_crate("bar", "0.1.0", -10, None)?;
            queue.add_crate("baz", "0.0.1", 10, None)?;

            let full = kuchiki::parse_html().one(web.get("/releases/queue").send()?.text()?);
            let items = full
                .select(".queue-list > li")
                .expect("missing list items")
                .collect::<Vec<_>>();

            assert_eq!(items.len(), 3);
            let expected = [
                ("bar", "0.1.0", Some(10)),
                ("foo", "1.0.0", None),
                ("baz", "0.0.1", Some(-10)),
            ];
            for (li, expected) in items.iter().zip(&expected) {
                let a = li.as_node().select_first("a").expect("missing link");
                assert!(a.text_contents().contains(expected.0));
                assert!(a.text_contents().contains(expected.1));

                if let Some(priority) = expected.2 {
                    assert!(li
                        .text_contents()
                        .contains(&format!("priority: {}", priority)));
                }
            }

            Ok(())
        });
    }

    #[test]
    fn nonexistent_owner_page() {
        wrapper(|env| {
            env.fake_release()
                .name("some_random_crate")
                .add_owner(CrateOwner {
                    login: "foobar".into(),
                    avatar: "https://example.org/foobar".into(),
                    name: "Foo Bar".into(),
                    email: "foobar@example.org".into(),
                })
                .create()?;
            let page = kuchiki::parse_html().one(
                env.frontend()
                    .get("/releases/random-author")
                    .send()?
                    .text()?,
            );

            assert_eq!(page.select("#crate-title").unwrap().count(), 1);
            assert_eq!(
                page.select("#crate-title")
                    .unwrap()
                    .next()
                    .unwrap()
                    .text_contents(),
                "The requested owner does not exist",
            );

            Ok(())
        });
    }

    #[test]
    fn owners_page() {
        wrapper(|env| {
            let web = env.frontend();
            env.fake_release()
                .name("some_random_crate")
                .add_owner(CrateOwner {
                    login: "foobar".into(),
                    avatar: "https://example.org/foobar".into(),
                    name: "Foo Bar".into(),
                    email: "foobar@example.org".into(),
                })
                .create()?;
            // Request an owner without @ sign.
            assert_success("/releases/foobar", web)?;
            // Request an owner with @ sign.
            assert_success("/releases/@foobar", web)
        })
    }

    #[test]
    fn owners_pagination() {
        wrapper(|env| {
            let web = env.frontend();
            for i in 0..RELEASES_IN_RELEASES {
                env.fake_release()
                    .name(&format!("some_random_crate_{}", i))
                    .add_owner(CrateOwner {
                        login: "foobar".into(),
                        avatar: "https://example.org/foobar".into(),
                        name: "Foo Bar".into(),
                        email: "foobar@example.org".into(),
                    })
                    .create()?;
            }
            let page = kuchiki::parse_html().one(web.get("/releases/@foobar").send()?.text()?);
            let button = page.select_first("a[href='/releases/@foobar/2']");

            assert!(button.is_ok());

            Ok(())
        })
    }

    #[test]
    fn home_page_links() {
        wrapper(|env| {
            let web = env.frontend();
            env.fake_release()
                .name("some_random_crate")
                .add_owner(CrateOwner {
                    login: "foobar".into(),
                    avatar: "https://example.org/foobar".into(),
                    name: "Foo Bar".into(),
                    email: "foobar@example.org".into(),
                })
                .create()?;

            let mut urls = vec![];
            let mut seen = HashSet::new();
            seen.insert("".to_owned());

            let resp = web.get("").send()?;
            assert!(resp.status().is_success());

            let html = kuchiki::parse_html().one(resp.text()?);
            for link in html.select("a").unwrap() {
                let link = link.as_node().as_element().unwrap();

                urls.push(link.attributes.borrow().get("href").unwrap().to_owned());
            }

            while let Some(url) = urls.pop() {
                // Skip urls we've already checked
                if !seen.insert(url.clone()) {
                    continue;
                }

                let resp = if url.starts_with("http://") || url.starts_with("https://") {
                    // Skip external links
                    continue;
                } else {
                    web.get(&url).send()?
                };
                let status = resp.status();
                assert!(status.is_success(), "failed to GET {}: {}", url, status);
            }

            Ok(())
        });
    }

    #[test]
    fn check_releases_page_content() {
        // NOTE: this is a little fragile and may have to be updated if the HTML layout changes
        let sel = ".pure-menu-horizontal>.pure-menu-list>.pure-menu-item>.pure-menu-link>.title";
        wrapper(|env| {
            let tester = |url| {
                let page = kuchiki::parse_html()
                    .one(env.frontend().get(url).send().unwrap().text().unwrap());
                assert_eq!(page.select("#crate-title").unwrap().count(), 1);
                let not_matching = page
                    .select(sel)
                    .unwrap()
                    .map(|node| node.text_contents())
                    .zip(
                        [
                            "Recent",
                            "Stars",
                            "Recent Failures",
                            "Failures By Stars",
                            "Activity",
                            "Queue",
                        ]
                        .iter(),
                    )
                    .filter(|(a, b)| a.as_str() != **b)
                    .collect::<Vec<_>>();
                if !not_matching.is_empty() {
                    let not_found = not_matching.iter().map(|(_, b)| b).collect::<Vec<_>>();
                    let found = not_matching.iter().map(|(a, _)| a).collect::<Vec<_>>();
                    assert!(
                        not_matching.is_empty(),
                        "Titles did not match for URL `{}`: not found: {:?}, found: {:?}",
                        url,
                        not_found,
                        found,
                    );
                }
            };

            for url in &[
                "/releases",
                "/releases/stars",
                "/releases/recent-failures",
                "/releases/failures",
                "/releases/activity",
                "/releases/queue",
            ] {
                tester(url);
            }

            Ok(())
        });
    }

    #[test]
    fn test_empty_query() {
        wrapper(|env| {
            let mut conn = env.db().conn();
            let (num_results, results) = get_search_results(&mut conn, "", 0, 0).unwrap();
            assert_eq!(num_results, 0);
            assert!(results.is_empty());
            Ok(())
        })
    }
}
