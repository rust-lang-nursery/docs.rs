use super::error::Nope;
use super::{match_version, redirect_base, render_markdown, MatchSemver, MetaData};
use crate::{db::Pool, impl_webpage, web::page::WebPage};
use chrono::{DateTime, NaiveDateTime, Utc};
use iron::prelude::*;
use iron::{status, Url};
use postgres::Connection;
use router::Router;
use serde::{ser::Serializer, Serialize};
use serde_json::Value;

// TODO: Add target name and versions

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CrateDetails {
    name: String,
    version: String,
    description: Option<String>,
    authors: Vec<(String, String)>,
    owners: Vec<(String, String)>,
    authors_json: Option<Value>,
    dependencies: Option<Value>,
    #[serde(serialize_with = "optional_markdown")]
    readme: Option<String>,
    #[serde(serialize_with = "optional_markdown")]
    rustdoc: Option<String>, // this is description_long in database
    release_time: DateTime<Utc>,
    build_status: bool,
    last_successful_build: Option<String>,
    rustdoc_status: bool,
    repository_url: Option<String>,
    homepage_url: Option<String>,
    keywords: Option<Value>,
    have_examples: bool, // need to check this manually
    pub target_name: String,
    releases: Vec<Release>,
    github: bool, // is crate hosted in github
    github_stars: Option<i32>,
    github_forks: Option<i32>,
    github_issues: Option<i32>,
    pub(crate) metadata: MetaData,
    is_library: bool,
    yanked: bool,
    pub(crate) doc_targets: Vec<String>,
    license: Option<String>,
    documentation_url: Option<String>,
}

fn optional_markdown<S>(markdown: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if let Some(ref markdown) = markdown {
        Some(render_markdown(&markdown))
    } else {
        None
    }
    .serialize(serializer)
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct Release {
    pub version: String,
    pub build_status: bool,
    pub yanked: bool,
}

impl CrateDetails {
    pub fn new(conn: &Connection, name: &str, version: &str) -> Option<CrateDetails> {
        // get all stuff, I love you rustfmt
        let query = "
            SELECT
                crates.id AS crate_id,
                releases.id AS release_id,
                crates.name,
                releases.version,
                releases.description,
                releases.authors,
                releases.dependencies,
                releases.readme,
                releases.description_long,
                releases.release_time,
                releases.build_status,
                releases.rustdoc_status,
                releases.repository_url,
                releases.homepage_url,
                releases.keywords,
                releases.have_examples,
                releases.target_name,
                ARRAY(SELECT releases.version FROM releases WHERE releases.crate_id = crates.id) AS versions,
                crates.github_stars,
                crates.github_forks,
                crates.github_issues,
                releases.is_library,
                releases.yanked,
                releases.doc_targets,
                releases.license,
                releases.documentation_url,
                releases.default_target
            FROM releases
            INNER JOIN crates ON releases.crate_id = crates.id
            WHERE crates.name = $1 AND releases.version = $2;";

        let rows = conn.query(query, &[&name, &version]).unwrap();

        let krate = if rows.is_empty() {
            return None;
        } else {
            rows.get(0)
        };

        let crate_id: i32 = krate.get("crate_id");
        let release_id: i32 = krate.get("release_id");

        // sort versions with semver
        let releases = {
            let versions: Vec<String> = krate.get("versions");
            let mut versions: Vec<semver::Version> = versions
                .iter()
                .filter_map(|version| semver::Version::parse(&version).ok())
                .collect();

            versions.sort();
            versions.reverse();
            versions
                .iter()
                .map(|version| map_to_release(&conn, crate_id, version.to_string()))
                .collect()
        };

        let metadata = MetaData {
            name: krate.get("name"),
            version: krate.get("version"),
            description: krate.get("description"),
            rustdoc_status: krate.get("rustdoc_status"),
            target_name: krate.get("target_name"),
            default_target: krate.get("default_target"),
        };

        let doc_targets = {
            let data: Value = krate.get("doc_targets");
            data.as_array()
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|item| item.as_str().map(|s| s.to_owned()))
                        .collect()
                })
                .unwrap_or_else(Vec::new)
        };

        let mut crate_details = CrateDetails {
            name: krate.get("name"),
            version: krate.get("version"),
            description: krate.get("description"),
            authors: Vec::new(),
            owners: Vec::new(),
            authors_json: krate.get("authors"),
            dependencies: krate.get("dependencies"),
            readme: krate.get("readme"),
            rustdoc: krate.get("description_long"),
            release_time: DateTime::from_utc(krate.get::<_, NaiveDateTime>("release_time"), Utc),
            build_status: krate.get("build_status"),
            last_successful_build: None,
            rustdoc_status: krate.get("rustdoc_status"),
            repository_url: krate.get("repository_url"),
            homepage_url: krate.get("homepage_url"),
            keywords: krate.get("keywords"),
            have_examples: krate.get("have_examples"),
            target_name: krate.get("target_name"),
            releases,
            github: false,
            github_stars: krate.get("github_stars"),
            github_forks: krate.get("github_forks"),
            github_issues: krate.get("github_issues"),
            metadata,
            is_library: krate.get("is_library"),
            yanked: krate.get("yanked"),
            doc_targets,
            license: krate.get("license"),
            documentation_url: krate.get("documentation_url"),
        };

        if let Some(repository_url) = crate_details.repository_url.clone() {
            crate_details.github = repository_url.starts_with("http://github.com")
                || repository_url.starts_with("https://github.com");
        }

        // get authors
        let authors = conn
            .query(
                "SELECT name, slug
                 FROM authors
                 INNER JOIN author_rels ON author_rels.aid = authors.id
                 WHERE rid = $1",
                &[&release_id],
            )
            .unwrap();

        crate_details.authors = authors
            .into_iter()
            .map(|row| (row.get("name"), row.get("slug")))
            .collect();

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
                .map(|release| release.version.to_owned())
                .next();
        }

        Some(crate_details)
    }

    /// Returns the latest non-yanked release of this crate (or latest yanked if they are all
    /// yanked).
    pub fn latest_release(&self) -> &Release {
        self.releases
            .iter()
            .find(|release| !release.yanked)
            .unwrap_or(&self.releases[0])
    }

    #[cfg(test)]
    pub fn default_tester(release_time: DateTime<Utc>) -> Self {
        Self {
            name: "rcc".to_string(),
            version: "100.0.0".to_string(),
            description: None,
            authors: vec![],
            owners: vec![],
            authors_json: None,
            dependencies: None,
            readme: None,
            rustdoc: None,
            release_time,
            build_status: true,
            last_successful_build: None,
            rustdoc_status: true,
            repository_url: None,
            homepage_url: None,
            keywords: None,
            yanked: false,
            have_examples: true,
            target_name: "x86_64-unknown-linux-gnu".to_string(),
            releases: vec![],
            github: true,
            github_stars: None,
            github_forks: None,
            github_issues: None,
            metadata: MetaData {
                name: "serde".to_string(),
                version: "1.0.0".to_string(),
                description: Some("serde does stuff".to_string()),
                target_name: None,
                rustdoc_status: true,
                default_target: "x86_64-unknown-linux-gnu".to_string(),
            },
            is_library: true,
            doc_targets: vec![],
            license: None,
            documentation_url: None,
        }
    }
}

fn map_to_release(conn: &Connection, crate_id: i32, version: String) -> Release {
    let rows = conn
        .query(
            "SELECT build_status, yanked
         FROM releases
         WHERE releases.crate_id = $1 and releases.version = $2;",
            &[&crate_id, &version],
        )
        .unwrap();

    let (build_status, yanked) = if !rows.is_empty() {
        (rows.get(0).get(0), rows.get(0).get(1))
    } else {
        Default::default()
    };

    Release {
        version,
        build_status,
        yanked,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct CrateDetailsPage {
    details: Option<CrateDetails>,
}

impl_webpage! {
    CrateDetailsPage = "crate/details.html",
}

pub fn crate_details_handler(req: &mut Request) -> IronResult<Response> {
    let router = extension!(req, Router);
    // this handler must always called with a crate name
    let name = cexpect!(req, router.find("name"));
    let req_version = router.find("version");

    let conn = extension!(req, Pool).get()?;

    match match_version(&conn, &name, req_version).and_then(|m| m.assume_exact()) {
        Some(MatchSemver::Exact((version, _))) => {
            let details = CrateDetails::new(&conn, &name, &version);

            CrateDetailsPage { details }.into_response(req)
        }

        Some(MatchSemver::Semver((version, _))) => {
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

        None => Err(IronError::new(Nope::CrateNotFound, status::NotFound)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::TestDatabase;
    use chrono::Utc;
    use failure::Error;
    use serde_json::json;

    fn assert_last_successful_build_equals(
        db: &TestDatabase,
        package: &str,
        version: &str,
        expected_last_successful_build: Option<&str>,
    ) -> Result<(), Error> {
        let details = CrateDetails::new(&db.conn(), package, version)
            .ok_or_else(|| failure::err_msg("could not fetch crate details"))?;

        assert_eq!(
            details.last_successful_build,
            expected_last_successful_build.map(|s| s.to_string()),
        );
        Ok(())
    }

    #[test]
    fn test_last_successful_build_when_last_releases_failed_or_yanked() {
        crate::test::wrapper(|env| {
            let db = env.db();

            db.fake_release().name("foo").version("0.0.1").create()?;
            db.fake_release().name("foo").version("0.0.2").create()?;
            db.fake_release()
                .name("foo")
                .version("0.0.3")
                .build_result_successful(false)
                .create()?;
            db.fake_release()
                .name("foo")
                .version("0.0.4")
                .yanked(true)
                .create()?;
            db.fake_release()
                .name("foo")
                .version("0.0.5")
                .build_result_successful(false)
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
        crate::test::wrapper(|env| {
            let db = env.db();

            db.fake_release()
                .name("foo")
                .version("0.0.1")
                .build_result_successful(false)
                .create()?;
            db.fake_release()
                .name("foo")
                .version("0.0.2")
                .build_result_successful(false)
                .create()?;
            db.fake_release()
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
        crate::test::wrapper(|env| {
            let db = env.db();

            db.fake_release().name("foo").version("0.0.1").create()?;
            db.fake_release()
                .name("foo")
                .version("0.0.2")
                .build_result_successful(false)
                .create()?;
            db.fake_release()
                .name("foo")
                .version("0.0.3")
                .yanked(true)
                .create()?;
            db.fake_release().name("foo").version("0.0.4").create()?;

            assert_last_successful_build_equals(&db, "foo", "0.0.1", None)?;
            assert_last_successful_build_equals(&db, "foo", "0.0.2", Some("0.0.4"))?;
            assert_last_successful_build_equals(&db, "foo", "0.0.3", None)?;
            assert_last_successful_build_equals(&db, "foo", "0.0.4", None)?;
            Ok(())
        });
    }

    #[test]
    fn test_releases_should_be_sorted() {
        crate::test::wrapper(|env| {
            let db = env.db();

            // Add new releases of 'foo' out-of-order since CrateDetails should sort them descending
            db.fake_release().name("foo").version("0.1.0").create()?;
            db.fake_release().name("foo").version("0.1.1").create()?;
            db.fake_release()
                .name("foo")
                .version("0.3.0")
                .build_result_successful(false)
                .create()?;
            db.fake_release().name("foo").version("1.0.0").create()?;
            db.fake_release().name("foo").version("0.12.0").create()?;
            db.fake_release()
                .name("foo")
                .version("0.2.0")
                .yanked(true)
                .create()?;
            db.fake_release()
                .name("foo")
                .version("0.2.0-alpha")
                .create()?;

            let details = CrateDetails::new(&db.conn(), "foo", "0.2.0").unwrap();
            assert_eq!(
                details.releases,
                vec![
                    Release {
                        version: "1.0.0".to_string(),
                        build_status: true,
                        yanked: false
                    },
                    Release {
                        version: "0.12.0".to_string(),
                        build_status: true,
                        yanked: false
                    },
                    Release {
                        version: "0.3.0".to_string(),
                        build_status: false,
                        yanked: false
                    },
                    Release {
                        version: "0.2.0".to_string(),
                        build_status: true,
                        yanked: true
                    },
                    Release {
                        version: "0.2.0-alpha".to_string(),
                        build_status: true,
                        yanked: false
                    },
                    Release {
                        version: "0.1.1".to_string(),
                        build_status: true,
                        yanked: false
                    },
                    Release {
                        version: "0.1.0".to_string(),
                        build_status: true,
                        yanked: false
                    },
                ]
            );

            Ok(())
        });
    }

    #[test]
    fn test_latest_version() {
        crate::test::wrapper(|env| {
            let db = env.db();

            db.fake_release().name("foo").version("0.0.1").create()?;
            db.fake_release().name("foo").version("0.0.3").create()?;
            db.fake_release().name("foo").version("0.0.2").create()?;

            for version in &["0.0.1", "0.0.2", "0.0.3"] {
                let details = CrateDetails::new(&db.conn(), "foo", version).unwrap();
                assert_eq!(details.latest_release().version, "0.0.3");
            }

            Ok(())
        })
    }

    #[test]
    fn test_latest_version_ignores_yanked() {
        crate::test::wrapper(|env| {
            let db = env.db();

            db.fake_release().name("foo").version("0.0.1").create()?;
            db.fake_release()
                .name("foo")
                .version("0.0.3")
                .yanked(true)
                .create()?;
            db.fake_release().name("foo").version("0.0.2").create()?;

            for version in &["0.0.1", "0.0.2", "0.0.3"] {
                let details = CrateDetails::new(&db.conn(), "foo", version).unwrap();
                assert_eq!(details.latest_release().version, "0.0.2");
            }

            Ok(())
        })
    }

    #[test]
    fn test_latest_version_only_yanked() {
        crate::test::wrapper(|env| {
            let db = env.db();

            db.fake_release()
                .name("foo")
                .version("0.0.1")
                .yanked(true)
                .create()?;
            db.fake_release()
                .name("foo")
                .version("0.0.3")
                .yanked(true)
                .create()?;
            db.fake_release()
                .name("foo")
                .version("0.0.2")
                .yanked(true)
                .create()?;

            for version in &["0.0.1", "0.0.2", "0.0.3"] {
                let details = CrateDetails::new(&db.conn(), "foo", version).unwrap();
                assert_eq!(details.latest_release().version, "0.0.3");
            }

            Ok(())
        })
    }

    #[test]
    fn serialize_crate_details() {
        let time = Utc::now();
        let mut details = CrateDetails::default_tester(time);

        let mut correct_json = json!({
            "name": "rcc",
            "version": "100.0.0",
            "description": null,
            "authors": [],
            "owners": [],
            "authors_json": null,
            "dependencies": null,
            "release_time": super::super::duration_to_str(time),
            "build_status": true,
            "last_successful_build": null,
            "rustdoc_status": true,
            "repository_url": null,
            "homepage_url": null,
            "keywords": null,
            "have_examples": true,
            "target_name": "x86_64-unknown-linux-gnu",
            "releases": [],
            "github": true,
            "yanked": false,
            "github_stars": null,
            "github_forks": null,
            "github_issues": null,
            "metadata": {
                "name": "serde",
                "version": "1.0.0",
                "description": "serde does stuff",
                "target_name": null,
                "rustdoc_status": true,
                "default_target": "x86_64-unknown-linux-gnu"
            },
            "is_library": true,
            "doc_targets": [],
            "license": null,
            "documentation_url": null
        });

        assert_eq!(correct_json, serde_json::to_value(&details).unwrap());

        let authors = vec![("Somebody".to_string(), "somebody@somebody.com".to_string())];
        let owners = vec![("Owner".to_string(), "owner@ownsstuff.com".to_string())];
        let description = "serde does stuff".to_string();

        correct_json["description"] = Value::String(description.clone());
        correct_json["owners"] = serde_json::to_value(&owners).unwrap();
        correct_json["authors_json"] = serde_json::to_value(&authors).unwrap();
        correct_json["authors"] = serde_json::to_value(&authors).unwrap();

        details.description = Some(description);
        details.owners = owners;
        details.authors_json = Some(serde_json::to_value(&authors).unwrap());
        details.authors = authors;

        assert_eq!(correct_json, serde_json::to_value(&details).unwrap());
    }

    #[test]
    fn serialize_releases() {
        let release = Release {
            version: "idkman".to_string(),
            build_status: true,
            yanked: true,
        };

        let correct_json = json!({
            "version": "idkman",
            "build_status": true,
            "yanked": true,
        });

        assert_eq!(correct_json, serde_json::to_value(&release).unwrap());
    }
}
