use super::{match_version, redirect_base, render_markdown, MatchSemver, MetaData};
use crate::{db::Pool, impl_webpage, web::page::WebPage};
use chrono::{DateTime, Utc};
use iron::prelude::*;
use iron::Url;
use postgres::Client;
use router::Router;
use serde::{ser::Serializer, Serialize};
use serde_json::Value;

// TODO: Add target name and versions

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CrateDetails {
    name: String,
    version: String,
    description: Option<String>,
    owners: Vec<(String, String)>,
    dependencies: Option<Value>,
    #[serde(serialize_with = "optional_markdown")]
    readme: Option<String>,
    #[serde(serialize_with = "optional_markdown")]
    rustdoc: Option<String>, // this is description_long in database
    release_time: DateTime<Utc>,
    build_status: bool,
    last_successful_build: Option<String>,
    rustdoc_status: bool,
    pub archive_storage: bool,
    repository_url: Option<String>,
    homepage_url: Option<String>,
    keywords: Option<Value>,
    have_examples: bool, // need to check this manually
    pub target_name: String,
    releases: Vec<Release>,
    github_metadata: Option<GitHubMetadata>,
    pub(crate) metadata: MetaData,
    is_library: bool,
    license: Option<String>,
    documentation_url: Option<String>,
    total_items: Option<f32>,
    documented_items: Option<f32>,
    total_items_needing_examples: Option<f32>,
    items_with_examples: Option<f32>,
    /// Database id for this crate
    pub(crate) crate_id: i32,
    /// Database id for this release
    pub(crate) release_id: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct GitHubMetadata {
    stars: i32,
    forks: i32,
    issues: i32,
    name: Option<String>,
}

fn optional_markdown<S>(markdown: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    markdown
        .as_ref()
        .map(|markdown| render_markdown(&markdown))
        .serialize(serializer)
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct Release {
    pub version: semver::Version,
    pub build_status: bool,
    pub yanked: bool,
    pub is_library: bool,
}

impl CrateDetails {
    pub fn new(conn: &mut Client, name: &str, version: &str) -> Option<CrateDetails> {
        // get all stuff, I love you rustfmt
        let query = "
            SELECT
                crates.id AS crate_id,
                releases.id AS release_id,
                crates.name,
                releases.version,
                releases.description,
                releases.dependencies,
                releases.readme,
                releases.description_long,
                releases.release_time,
                releases.build_status,
                releases.rustdoc_status,
                releases.archive_storage,
                releases.repository_url,
                releases.homepage_url,
                releases.keywords,
                releases.have_examples,
                releases.target_name,
                releases.github_repo,
                github_repos.stars AS github_stars,
                github_repos.forks AS github_forks,
                github_repos.issues AS github_issues,
                github_repos.name AS github_name,
                releases.is_library,
                releases.yanked,
                releases.doc_targets,
                releases.license,
                releases.documentation_url,
                releases.default_target,
                doc_coverage.total_items,
                doc_coverage.documented_items,
                doc_coverage.total_items_needing_examples,
                doc_coverage.items_with_examples
            FROM releases
            INNER JOIN crates ON releases.crate_id = crates.id
            LEFT JOIN doc_coverage ON doc_coverage.release_id = releases.id
            LEFT JOIN github_repos ON releases.github_repo = github_repos.id
            WHERE crates.name = $1 AND releases.version = $2;";

        let rows = conn.query(query, &[&name, &version]).unwrap();

        let krate = if rows.is_empty() {
            return None;
        } else {
            &rows[0]
        };

        let crate_id: i32 = krate.get("crate_id");
        let release_id: i32 = krate.get("release_id");

        // get releases, sorted by semver
        let releases = releases_for_crate(conn, crate_id);

        let github_metadata = if krate.get::<_, Option<String>>("github_repo").is_some() {
            Some(GitHubMetadata {
                issues: krate.get("github_issues"),
                stars: krate.get("github_stars"),
                forks: krate.get("github_forks"),
                name: krate.get("github_name"),
            })
        } else {
            None
        };

        let metadata = MetaData {
            name: krate.get("name"),
            version: krate.get("version"),
            description: krate.get("description"),
            rustdoc_status: krate.get("rustdoc_status"),
            target_name: krate.get("target_name"),
            default_target: krate.get("default_target"),
            doc_targets: MetaData::parse_doc_targets(krate.get("doc_targets")),
            yanked: krate.get("yanked"),
        };

        let documented_items: Option<i32> = krate.get("documented_items");
        let total_items: Option<i32> = krate.get("total_items");
        let total_items_needing_examples: Option<i32> = krate.get("total_items_needing_examples");
        let items_with_examples: Option<i32> = krate.get("items_with_examples");

        let mut crate_details = CrateDetails {
            name: krate.get("name"),
            version: krate.get("version"),
            description: krate.get("description"),
            owners: Vec::new(),
            dependencies: krate.get("dependencies"),
            readme: krate.get("readme"),
            rustdoc: krate.get("description_long"),
            release_time: krate.get("release_time"),
            build_status: krate.get("build_status"),
            last_successful_build: None,
            rustdoc_status: krate.get("rustdoc_status"),
            archive_storage: krate.get("archive_storage"),
            repository_url: krate.get("repository_url"),
            homepage_url: krate.get("homepage_url"),
            keywords: krate.get("keywords"),
            have_examples: krate.get("have_examples"),
            target_name: krate.get("target_name"),
            releases,
            github_metadata,
            metadata,
            is_library: krate.get("is_library"),
            license: krate.get("license"),
            documentation_url: krate.get("documentation_url"),
            documented_items: documented_items.map(|v| v as f32),
            total_items: total_items.map(|v| v as f32),
            total_items_needing_examples: total_items_needing_examples.map(|v| v as f32),
            items_with_examples: items_with_examples.map(|v| v as f32),
            crate_id,
            release_id,
        };

        // get owners
        let owners = conn
            .query(
                "SELECT login, avatar
                 FROM owners
                 INNER JOIN owner_rels ON owner_rels.oid = owners.id
                 WHERE cid = $1",
                &[&crate_id],
            )
            .unwrap();

        crate_details.owners = owners
            .into_iter()
            .map(|row| (row.get("login"), row.get("avatar")))
            .collect();

        if !crate_details.build_status {
            crate_details.last_successful_build = crate_details
                .releases
                .iter()
                .filter(|release| release.build_status && !release.yanked)
                .map(|release| release.version.to_string())
                .next();
        }

        Some(crate_details)
    }

    /// Returns the latest non-yanked, non-prerelease release of this crate (or latest
    /// yanked/prereleased if that is all that exist).
    pub fn latest_release(&self) -> &Release {
        self.releases
            .iter()
            .find(|release| !release.version.is_prerelease() && !release.yanked)
            .unwrap_or(&self.releases[0])
    }
}

fn releases_for_crate(conn: &mut Client, crate_id: i32) -> Vec<Release> {
    let mut releases: Vec<Release> = conn
        .query(
            "SELECT 
                version,
                build_status,
                yanked,
                is_library
             FROM releases
             WHERE 
                 releases.crate_id = $1",
            &[&crate_id],
        )
        .unwrap()
        .into_iter()
        .filter_map(|row| {
            let version: String = row.get("version");
            semver::Version::parse(&version)
                .map(|semversion| Release {
                    version: semversion,
                    build_status: row.get("build_status"),
                    yanked: row.get("yanked"),
                    is_library: row.get("is_library"),
                })
                .ok()
        })
        .collect();

    releases.sort_by_key(|r| r.version.clone());
    releases.reverse();
    releases
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct CrateDetailsPage {
    details: CrateDetails,
}

impl_webpage! {
    CrateDetailsPage = "crate/details.html",
}

pub fn crate_details_handler(req: &mut Request) -> IronResult<Response> {
    let router = extension!(req, Router);
    // this handler must always called with a crate name
    let name = cexpect!(req, router.find("name"));
    let req_version = router.find("version");

    let mut conn = extension!(req, Pool).get()?;

    match match_version(&mut conn, &name, req_version).and_then(|m| m.assume_exact())? {
        MatchSemver::Exact((version, _)) => {
            let details = cexpect!(req, CrateDetails::new(&mut conn, &name, &version));

            CrateDetailsPage { details }.into_response(req)
        }

        MatchSemver::Semver((version, _)) => {
            let url = ctry!(
                req,
                Url::parse(&format!(
                    "{}/crate/{}/{}",
                    redirect_base(req),
                    name,
                    version
                )),
            );

            Ok(super::redirect(url))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::api::CrateOwner;
    use crate::test::{wrapper, TestDatabase};
    use failure::Error;
    use kuchiki::traits::TendrilSink;
    use std::collections::HashMap;

    fn assert_last_successful_build_equals(
        db: &TestDatabase,
        package: &str,
        version: &str,
        expected_last_successful_build: Option<&str>,
    ) -> Result<(), Error> {
        let details = CrateDetails::new(&mut db.conn(), package, version)
            .ok_or_else(|| failure::err_msg("could not fetch crate details"))?;

        assert_eq!(
            details.last_successful_build,
            expected_last_successful_build.map(|s| s.to_string()),
        );
        Ok(())
    }

    #[test]
    fn test_last_successful_build_when_last_releases_failed_or_yanked() {
        wrapper(|env| {
            let db = env.db();

            env.fake_release().name("foo").version("0.0.1").create()?;
            env.fake_release().name("foo").version("0.0.2").create()?;
            env.fake_release()
                .name("foo")
                .version("0.0.3")
                .build_result_failed()
                .create()?;
            env.fake_release()
                .name("foo")
                .version("0.0.4")
                .yanked(true)
                .create()?;
            env.fake_release()
                .name("foo")
                .version("0.0.5")
                .build_result_failed()
                .yanked(true)
                .create()?;

            assert_last_successful_build_equals(&db, "foo", "0.0.1", None)?;
            assert_last_successful_build_equals(&db, "foo", "0.0.2", None)?;
            assert_last_successful_build_equals(&db, "foo", "0.0.3", Some("0.0.2"))?;
            assert_last_successful_build_equals(&db, "foo", "0.0.4", None)?;
            assert_last_successful_build_equals(&db, "foo", "0.0.5", Some("0.0.2"))?;
            Ok(())
        });
    }

    #[test]
    fn test_last_successful_build_when_all_releases_failed_or_yanked() {
        wrapper(|env| {
            let db = env.db();

            env.fake_release()
                .name("foo")
                .version("0.0.1")
                .build_result_failed()
                .create()?;
            env.fake_release()
                .name("foo")
                .version("0.0.2")
                .build_result_failed()
                .create()?;
            env.fake_release()
                .name("foo")
                .version("0.0.3")
                .yanked(true)
                .create()?;

            assert_last_successful_build_equals(&db, "foo", "0.0.1", None)?;
            assert_last_successful_build_equals(&db, "foo", "0.0.2", None)?;
            assert_last_successful_build_equals(&db, "foo", "0.0.3", None)?;
            Ok(())
        });
    }

    #[test]
    fn test_last_successful_build_with_intermittent_releases_failed_or_yanked() {
        wrapper(|env| {
            let db = env.db();

            env.fake_release().name("foo").version("0.0.1").create()?;
            env.fake_release()
                .name("foo")
                .version("0.0.2")
                .build_result_failed()
                .create()?;
            env.fake_release()
                .name("foo")
                .version("0.0.3")
                .yanked(true)
                .create()?;
            env.fake_release().name("foo").version("0.0.4").create()?;

            assert_last_successful_build_equals(&db, "foo", "0.0.1", None)?;
            assert_last_successful_build_equals(&db, "foo", "0.0.2", Some("0.0.4"))?;
            assert_last_successful_build_equals(&db, "foo", "0.0.3", None)?;
            assert_last_successful_build_equals(&db, "foo", "0.0.4", None)?;
            Ok(())
        });
    }

    #[test]
    fn test_releases_should_be_sorted() {
        wrapper(|env| {
            let db = env.db();

            // Add new releases of 'foo' out-of-order since CrateDetails should sort them descending
            env.fake_release().name("foo").version("0.1.0").create()?;
            env.fake_release().name("foo").version("0.1.1").create()?;
            env.fake_release()
                .name("foo")
                .version("0.3.0")
                .build_result_failed()
                .create()?;
            env.fake_release().name("foo").version("1.0.0").create()?;
            env.fake_release().name("foo").version("0.12.0").create()?;
            env.fake_release()
                .name("foo")
                .version("0.2.0")
                .yanked(true)
                .create()?;
            env.fake_release()
                .name("foo")
                .version("0.2.0-alpha")
                .create()?;
            env.fake_release()
                .name("foo")
                .version("0.0.1")
                .build_result_failed()
                .binary(true)
                .create()?;

            let details = CrateDetails::new(&mut db.conn(), "foo", "0.2.0").unwrap();
            assert_eq!(
                details.releases,
                vec![
                    Release {
                        version: semver::Version::parse("1.0.0")?,
                        build_status: true,
                        yanked: false,
                        is_library: true,
                    },
                    Release {
                        version: semver::Version::parse("0.12.0")?,
                        build_status: true,
                        yanked: false,
                        is_library: true,
                    },
                    Release {
                        version: semver::Version::parse("0.3.0")?,
                        build_status: false,
                        yanked: false,
                        is_library: true,
                    },
                    Release {
                        version: semver::Version::parse("0.2.0")?,
                        build_status: true,
                        yanked: true,
                        is_library: true,
                    },
                    Release {
                        version: semver::Version::parse("0.2.0-alpha")?,
                        build_status: true,
                        yanked: false,
                        is_library: true,
                    },
                    Release {
                        version: semver::Version::parse("0.1.1")?,
                        build_status: true,
                        yanked: false,
                        is_library: true,
                    },
                    Release {
                        version: semver::Version::parse("0.1.0")?,
                        build_status: true,
                        yanked: false,
                        is_library: true,
                    },
                    Release {
                        version: semver::Version::parse("0.0.1")?,
                        build_status: false,
                        yanked: false,
                        is_library: false,
                    },
                ]
            );

            Ok(())
        });
    }

    #[test]
    fn test_latest_version() {
        wrapper(|env| {
            let db = env.db();

            env.fake_release().name("foo").version("0.0.1").create()?;
            env.fake_release().name("foo").version("0.0.3").create()?;
            env.fake_release().name("foo").version("0.0.2").create()?;

            for version in &["0.0.1", "0.0.2", "0.0.3"] {
                let details = CrateDetails::new(&mut db.conn(), "foo", version).unwrap();
                assert_eq!(
                    details.latest_release().version,
                    semver::Version::parse("0.0.3")?
                );
            }

            Ok(())
        })
    }

    #[test]
    fn test_latest_version_ignores_prerelease() {
        wrapper(|env| {
            let db = env.db();

            env.fake_release().name("foo").version("0.0.1").create()?;
            env.fake_release()
                .name("foo")
                .version("0.0.3-pre.1")
                .create()?;
            env.fake_release().name("foo").version("0.0.2").create()?;

            for version in &["0.0.1", "0.0.2", "0.0.3-pre.1"] {
                let details = CrateDetails::new(&mut db.conn(), "foo", version).unwrap();
                assert_eq!(
                    details.latest_release().version,
                    semver::Version::parse("0.0.2")?
                );
            }

            Ok(())
        })
    }

    #[test]
    fn test_latest_version_ignores_yanked() {
        wrapper(|env| {
            let db = env.db();

            env.fake_release().name("foo").version("0.0.1").create()?;
            env.fake_release()
                .name("foo")
                .version("0.0.3")
                .yanked(true)
                .create()?;
            env.fake_release().name("foo").version("0.0.2").create()?;

            for version in &["0.0.1", "0.0.2", "0.0.3"] {
                let details = CrateDetails::new(&mut db.conn(), "foo", version).unwrap();
                assert_eq!(
                    details.latest_release().version,
                    semver::Version::parse("0.0.2")?
                );
            }

            Ok(())
        })
    }

    #[test]
    fn test_latest_version_only_yanked() {
        wrapper(|env| {
            let db = env.db();

            env.fake_release()
                .name("foo")
                .version("0.0.1")
                .yanked(true)
                .create()?;
            env.fake_release()
                .name("foo")
                .version("0.0.3")
                .yanked(true)
                .create()?;
            env.fake_release()
                .name("foo")
                .version("0.0.2")
                .yanked(true)
                .create()?;

            for version in &["0.0.1", "0.0.2", "0.0.3"] {
                let details = CrateDetails::new(&mut db.conn(), "foo", version).unwrap();
                assert_eq!(
                    details.latest_release().version,
                    semver::Version::parse("0.0.3")?
                );
            }

            Ok(())
        })
    }

    #[test]
    fn releases_dropdowns_is_correct() {
        wrapper(|env| {
            env.fake_release()
                .name("binary")
                .version("0.1.0")
                .binary(true)
                .create()?;

            let page = kuchiki::parse_html()
                .one(env.frontend().get("/crate/binary/0.1.0").send()?.text()?);
            let warning = page.select_first("a.pure-menu-link.warn").unwrap();

            assert_eq!(
                warning
                    .as_node()
                    .as_element()
                    .unwrap()
                    .attributes
                    .borrow()
                    .get("title")
                    .unwrap(),
                "binary-0.1.0 is not a library"
            );

            Ok(())
        });
    }

    #[test]
    fn test_updating_owners() {
        wrapper(|env| {
            let db = env.db();

            env.fake_release()
                .name("foo")
                .version("0.0.1")
                .add_owner(CrateOwner {
                    login: "foobar".into(),
                    avatar: "https://example.org/foobar".into(),
                    name: "Foo Bar".into(),
                    email: "foobar@example.org".into(),
                })
                .create()?;

            let details = CrateDetails::new(&mut db.conn(), "foo", "0.0.1").unwrap();
            assert_eq!(
                details.owners,
                vec![("foobar".into(), "https://example.org/foobar".into())]
            );

            // Adding a new owner, and changing details on an existing owner
            env.fake_release()
                .name("foo")
                .version("0.0.2")
                .add_owner(CrateOwner {
                    login: "foobar".into(),
                    avatar: "https://example.org/foobarv2".into(),
                    name: "Foo Bar".into(),
                    email: "foobar@example.org".into(),
                })
                .add_owner(CrateOwner {
                    login: "barfoo".into(),
                    avatar: "https://example.org/barfoo".into(),
                    name: "Bar Foo".into(),
                    email: "foobar@example.org".into(),
                })
                .create()?;

            let details = CrateDetails::new(&mut db.conn(), "foo", "0.0.1").unwrap();
            let mut owners = details.owners;
            owners.sort();
            assert_eq!(
                owners,
                vec![
                    ("barfoo".into(), "https://example.org/barfoo".into()),
                    ("foobar".into(), "https://example.org/foobarv2".into())
                ]
            );

            // Removing an existing owner
            env.fake_release()
                .name("foo")
                .version("0.0.3")
                .add_owner(CrateOwner {
                    login: "barfoo".into(),
                    avatar: "https://example.org/barfoo".into(),
                    name: "Bar Foo".into(),
                    email: "foobar@example.org".into(),
                })
                .create()?;

            let details = CrateDetails::new(&mut db.conn(), "foo", "0.0.1").unwrap();
            assert_eq!(
                details.owners,
                vec![("barfoo".into(), "https://example.org/barfoo".into())]
            );

            // Changing owner details on another of their crates applies the change to both
            env.fake_release()
                .name("bar")
                .version("0.0.1")
                .add_owner(CrateOwner {
                    login: "barfoo".into(),
                    avatar: "https://example.org/barfoov2".into(),
                    name: "Bar Foo".into(),
                    email: "foobar@example.org".into(),
                })
                .create()?;

            let details = CrateDetails::new(&mut db.conn(), "foo", "0.0.1").unwrap();
            assert_eq!(
                details.owners,
                vec![("barfoo".into(), "https://example.org/barfoov2".into())]
            );

            Ok(())
        });
    }

    #[test]
    fn feature_flags_report_empty() {
        wrapper(|env| {
            env.fake_release()
                .name("library")
                .version("0.1.0")
                .features(HashMap::new())
                .create()?;

            let page = kuchiki::parse_html().one(
                env.frontend()
                    .get("/crate/library/0.1.0/features")
                    .send()?
                    .text()?,
            );
            assert!(page.select_first(r#"p[data-id="empty-features"]"#).is_ok());
            Ok(())
        });
    }

    #[test]
    fn feature_private_feature_flags_are_hidden() {
        wrapper(|env| {
            let features = [("_private".into(), Vec::new())]
                .iter()
                .cloned()
                .collect::<HashMap<String, Vec<String>>>();
            env.fake_release()
                .name("library")
                .version("0.1.0")
                .features(features)
                .create()?;

            let page = kuchiki::parse_html().one(
                env.frontend()
                    .get("/crate/library/0.1.0/features")
                    .send()?
                    .text()?,
            );
            assert!(page.select_first(r#"p[data-id="empty-features"]"#).is_ok());
            Ok(())
        });
    }

    #[test]
    fn feature_flags_without_default() {
        wrapper(|env| {
            let features = [("feature1".into(), Vec::new())]
                .iter()
                .cloned()
                .collect::<HashMap<String, Vec<String>>>();
            env.fake_release()
                .name("library")
                .version("0.1.0")
                .features(features)
                .create()?;

            let page = kuchiki::parse_html().one(
                env.frontend()
                    .get("/crate/library/0.1.0/features")
                    .send()?
                    .text()?,
            );
            assert!(page.select_first(r#"p[data-id="empty-features"]"#).is_err());
            let def_len = page
                .select_first(r#"b[data-id="default-feature-len"]"#)
                .unwrap();
            assert_eq!(def_len.text_contents(), "0");
            Ok(())
        });
    }

    #[test]
    fn feature_flags_with_nested_default() {
        wrapper(|env| {
            let features = [
                ("default".into(), vec!["feature1".into()]),
                ("feature1".into(), vec!["feature2".into()]),
                ("feature2".into(), Vec::new()),
            ]
            .iter()
            .cloned()
            .collect::<HashMap<String, Vec<String>>>();
            env.fake_release()
                .name("library")
                .version("0.1.0")
                .features(features)
                .create()?;

            let page = kuchiki::parse_html().one(
                env.frontend()
                    .get("/crate/library/0.1.0/features")
                    .send()?
                    .text()?,
            );
            assert!(page.select_first(r#"p[data-id="empty-features"]"#).is_err());
            let def_len = page
                .select_first(r#"b[data-id="default-feature-len"]"#)
                .unwrap();
            assert_eq!(def_len.text_contents(), "3");
            Ok(())
        });
    }

    #[test]
    fn feature_flags_report_null() {
        wrapper(|env| {
            let id = env
                .fake_release()
                .name("library")
                .version("0.1.0")
                .create()?;

            env.db()
                .conn()
                .query("UPDATE releases SET features = NULL WHERE id = $1", &[&id])?;

            let page = kuchiki::parse_html().one(
                env.frontend()
                    .get("/crate/library/0.1.0/features")
                    .send()?
                    .text()?,
            );
            assert!(page.select_first(r#"p[data-id="null-features"]"#).is_ok());
            Ok(())
        });
    }

    #[test]
    fn platform_links_are_direct_and_without_nofollow() {
        wrapper(|env| {
            env.fake_release()
                .name("dummy")
                .version("0.4.0")
                .rustdoc_file("dummy/index.html")
                .rustdoc_file("x86_64-pc-windows-msvc/dummy/index.html")
                .default_target("x86_64-unknown-linux-gnu")
                .add_target("x86_64-pc-windows-msvc")
                .create()?;

            let response = env.frontend().get("/crate/dummy/0.4.0").send()?;
            assert!(response.status().is_success());

            let platform_links: Vec<(String, String)> = kuchiki::parse_html()
                .one(response.text()?)
                .select(r#"a[aria-label="Platform"] + ul li a"#)
                .expect("invalid selector")
                .map(|el| {
                    let attributes = el.attributes.borrow();
                    let url = attributes.get("href").expect("href").to_string();
                    let rel = attributes.get("rel").unwrap_or("").to_string();
                    (url, rel)
                })
                .collect();

            assert_eq!(platform_links.len(), 2);

            for (url, rel) in platform_links {
                assert!(!url.contains("/target-redirect/"));
                assert_eq!(rel, "");
            }

            Ok(())
        });
    }
}
