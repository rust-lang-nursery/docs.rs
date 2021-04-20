use super::{match_version, redirect_base, MatchSemver};
use crate::{
    db::Pool,
    docbuilder::Limits,
    impl_webpage,
    web::{page::WebPage, MetaData},
};
use chrono::{DateTime, Utc};
use iron::{
    headers::{
        AccessControlAllowOrigin, CacheControl, CacheDirective, ContentType, Expires, HttpDate,
    },
    status, IronResult, Request, Response, Url,
};
use router::Router;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Build {
    id: i32,
    rustc_version: String,
    docsrs_version: String,
    build_status: bool,
    build_time: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BuildsPage {
    metadata: MetaData,
    builds: Vec<Build>,
    limits: Limits,
}

impl_webpage! {
    BuildsPage = "crate/builds.html",
}

pub fn build_list_handler(req: &mut Request) -> IronResult<Response> {
    let router = extension!(req, Router);
    let name = cexpect!(req, router.find("name"));
    let req_version = router.find("version");

    let mut conn = extension!(req, Pool).get()?;
    let limits = ctry!(req, Limits::for_crate(&mut conn, name));

    let is_json = req
        .url
        .path()
        .last()
        .map_or(false, |segment| segment.ends_with(".json"));

    let version =
        match match_version(&mut conn, name, req_version).and_then(|m| m.assume_exact())? {
            MatchSemver::Exact((version, _)) => version,

            MatchSemver::Semver((version, _)) => {
                let ext = if is_json { ".json" } else { "" };
                let url = ctry!(
                    req,
                    Url::parse(&format!(
                        "{}/crate/{}/{}/builds{}",
                        redirect_base(req),
                        name,
                        version,
                        ext,
                    )),
                );

                return Ok(super::redirect(url));
            }
        };

    let query = ctry!(
        req,
        conn.query(
            "SELECT crates.name,
                releases.version,
                releases.description,
                releases.rustdoc_status,
                releases.target_name,
                builds.id,
                builds.rustc_version,
                builds.docsrs_version,
                builds.build_status,
                builds.build_time
             FROM builds
             INNER JOIN releases ON releases.id = builds.rid
             INNER JOIN crates ON releases.crate_id = crates.id
             WHERE crates.name = $1 AND releases.version = $2
             ORDER BY id DESC",
            &[&name, &version]
        )
    );

    let builds: Vec<_> = query
        .into_iter()
        .map(|row| Build {
            id: row.get("id"),
            rustc_version: row.get("rustc_version"),
            docsrs_version: row.get("docsrs_version"),
            build_status: row.get("build_status"),
            build_time: row.get("build_time"),
        })
        .collect();

    if is_json {
        let mut resp = Response::with((status::Ok, serde_json::to_string(&builds).unwrap()));
        resp.headers.set(ContentType::json());
        resp.headers.set(Expires(HttpDate(time::now())));
        resp.headers.set(CacheControl(vec![
            CacheDirective::NoCache,
            CacheDirective::NoStore,
            CacheDirective::MustRevalidate,
        ]));
        resp.headers.set(AccessControlAllowOrigin::Any);

        Ok(resp)
    } else {
        BuildsPage {
            metadata: cexpect!(req, MetaData::from_crate(&mut conn, &name, &version)),
            builds,
            limits,
        }
        .into_response(req)
    }
}

#[cfg(test)]
mod tests {
    use crate::test::{wrapper, FakeBuild};
    use chrono::{DateTime, Duration, Utc};
    use kuchiki::traits::TendrilSink;
    use reqwest::StatusCode;

    #[test]
    fn build_list() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.1.0")
                .builds(vec![
                    FakeBuild::default()
                        .rustc_version("rustc 1.0.0")
                        .docsrs_version("docs.rs 1.0.0"),
                    FakeBuild::default()
                        .successful(false)
                        .rustc_version("rustc 2.0.0")
                        .docsrs_version("docs.rs 2.0.0"),
                    FakeBuild::default()
                        .rustc_version("rustc 3.0.0")
                        .docsrs_version("docs.rs 3.0.0"),
                ])
                .create()?;

            let page = kuchiki::parse_html().one(
                env.frontend()
                    .get("/crate/foo/0.1.0/builds")
                    .send()?
                    .text()?,
            );

            let rows: Vec<_> = page
                .select("ul > li a.release")
                .unwrap()
                .map(|row| row.text_contents())
                .collect();

            assert!(rows[0].contains("rustc 3.0.0"));
            assert!(rows[0].contains("docs.rs 3.0.0"));
            assert!(rows[1].contains("rustc 2.0.0"));
            assert!(rows[1].contains("docs.rs 2.0.0"));
            assert!(rows[2].contains("rustc 1.0.0"));
            assert!(rows[2].contains("docs.rs 1.0.0"));

            Ok(())
        });
    }

    #[test]
    fn build_list_json() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.1.0")
                .builds(vec![
                    FakeBuild::default()
                        .rustc_version("rustc 1.0.0")
                        .docsrs_version("docs.rs 1.0.0"),
                    FakeBuild::default()
                        .successful(false)
                        .rustc_version("rustc 2.0.0")
                        .docsrs_version("docs.rs 2.0.0"),
                    FakeBuild::default()
                        .rustc_version("rustc 3.0.0")
                        .docsrs_version("docs.rs 3.0.0"),
                ])
                .create()?;

            let value: serde_json::Value = serde_json::from_str(
                &env.frontend()
                    .get("/crate/foo/0.1.0/builds.json")
                    .send()?
                    .text()?,
            )?;

            assert_eq!(value.pointer("/0/build_status"), Some(&true.into()));
            assert_eq!(
                value.pointer("/0/docsrs_version"),
                Some(&"docs.rs 3.0.0".into())
            );
            assert_eq!(
                value.pointer("/0/rustc_version"),
                Some(&"rustc 3.0.0".into())
            );
            assert!(value.pointer("/0/id").unwrap().is_i64());
            assert!(serde_json::from_value::<DateTime<Utc>>(
                value.pointer("/0/build_time").unwrap().clone()
            )
            .is_ok());

            assert_eq!(value.pointer("/1/build_status"), Some(&false.into()));
            assert_eq!(
                value.pointer("/1/docsrs_version"),
                Some(&"docs.rs 2.0.0".into())
            );
            assert_eq!(
                value.pointer("/1/rustc_version"),
                Some(&"rustc 2.0.0".into())
            );
            assert!(value.pointer("/1/id").unwrap().is_i64());
            assert!(serde_json::from_value::<DateTime<Utc>>(
                value.pointer("/1/build_time").unwrap().clone()
            )
            .is_ok());

            assert_eq!(value.pointer("/2/build_status"), Some(&true.into()));
            assert_eq!(
                value.pointer("/2/docsrs_version"),
                Some(&"docs.rs 1.0.0".into())
            );
            assert_eq!(
                value.pointer("/2/rustc_version"),
                Some(&"rustc 1.0.0".into())
            );
            assert!(value.pointer("/2/id").unwrap().is_i64());
            assert!(serde_json::from_value::<DateTime<Utc>>(
                value.pointer("/2/build_time").unwrap().clone()
            )
            .is_ok());

            assert!(
                value.pointer("/1/build_time").unwrap().as_str().unwrap()
                    < value.pointer("/0/build_time").unwrap().as_str().unwrap()
            );
            assert!(
                value.pointer("/2/build_time").unwrap().as_str().unwrap()
                    < value.pointer("/1/build_time").unwrap().as_str().unwrap()
            );

            Ok(())
        });
    }

    #[test]
    fn limits() {
        wrapper(|env| {
            env.fake_release().name("foo").version("0.1.0").create()?;

            env.db().conn().query(
                "INSERT INTO sandbox_overrides
                    (crate_name, max_memory_bytes, timeout_seconds, max_targets)
                 VALUES ($1, $2, $3, $4)",
                &[
                    &"foo",
                    &3072i64,
                    &(Duration::hours(2).num_seconds() as i32),
                    &1,
                ],
            )?;

            let page = kuchiki::parse_html().one(
                env.frontend()
                    .get("/crate/foo/0.1.0/builds")
                    .send()?
                    .text()?,
            );

            let header = page.select(".about h4").unwrap().next().unwrap();
            assert_eq!(header.text_contents(), "foo's sandbox limits");

            let values: Vec<_> = page
                .select(".about table tr td:last-child")
                .unwrap()
                .map(|row| row.text_contents())
                .collect();
            let values: Vec<_> = values.iter().map(|v| &**v).collect();

            dbg!(&values);
            assert!(values.contains(&"3 KB"));
            assert!(values.contains(&"2 hours"));
            assert!(values.contains(&"100 KB"));
            assert!(values.contains(&"blocked"));
            assert!(values.contains(&"1"));

            Ok(())
        });
    }

    #[test]
    fn latest_redirect() {
        wrapper(|env| {
            env.fake_release()
                .name("aquarelle")
                .version("0.1.0")
                .builds(vec![FakeBuild::default()
                    .rustc_version("rustc 1.0.0")
                    .docsrs_version("docs.rs 1.0.0")])
                .create()?;

            env.fake_release()
                .name("aquarelle")
                .version("0.2.0")
                .builds(vec![FakeBuild::default()
                    .rustc_version("rustc 1.0.0")
                    .docsrs_version("docs.rs 1.0.0")])
                .create()?;

            let resp = env
                .frontend()
                .get("/crate/aquarelle/latest/builds")
                .send()?;
            assert!(resp
                .url()
                .as_str()
                .ends_with("/crate/aquarelle/0.2.0/builds"));

            let resp_json = env
                .frontend()
                .get("/crate/aquarelle/latest/builds.json")
                .send()?;
            assert!(resp_json
                .url()
                .as_str()
                .ends_with("/crate/aquarelle/0.2.0/builds.json"));

            Ok(())
        });
    }

    #[test]
    fn crate_version_not_found() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.1.0")
                .builds(vec![FakeBuild::default()
                    .rustc_version("rustc 1.0.0")
                    .docsrs_version("docs.rs 1.0.0")])
                .create()?;

            let resp = env.frontend().get("/crate/foo/0.2.0/builds").send()?;
            dbg!(resp.url().as_str());
            assert!(resp.url().as_str().ends_with("/crate/foo/0.2.0/builds"));
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            Ok(())
        });
    }

    #[test]
    fn invalid_semver() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.1.0")
                .builds(vec![FakeBuild::default()
                    .rustc_version("rustc 1.0.0")
                    .docsrs_version("docs.rs 1.0.0")])
                .create()?;

            let resp = env.frontend().get("/crate/foo/0,1,0/builds").send()?;
            dbg!(resp.url().as_str());
            assert!(resp.url().as_str().ends_with("/crate/foo/0,1,0/builds"));
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            Ok(())
        });
    }
}
