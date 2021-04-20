use crate::{
    db::Pool,
    impl_webpage,
    web::{file::File, page::WebPage, MetaData, Nope},
    Config, Storage,
};
use chrono::{DateTime, Utc};
use iron::{IronResult, Request, Response};
use router::Router;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct BuildDetails {
    id: i32,
    rustc_version: String,
    docsrs_version: String,
    build_status: bool,
    build_time: DateTime<Utc>,
    output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BuildDetailsPage {
    metadata: MetaData,
    build_details: BuildDetails,
}

impl_webpage! {
    BuildDetailsPage = "crate/build_details.html",
}

pub fn build_details_handler(req: &mut Request) -> IronResult<Response> {
    let storage = extension!(req, Storage);
    let config = extension!(req, Config);
    let router = extension!(req, Router);
    let name = cexpect!(req, router.find("name"));
    let version = cexpect!(req, router.find("version"));
    let id: i32 = ctry!(req, cexpect!(req, router.find("id")).parse());

    let mut conn = extension!(req, Pool).get()?;

    let row = ctry!(
        req,
        conn.query_opt(
            "SELECT
                builds.rustc_version,
                builds.docsrs_version,
                builds.build_status,
                builds.build_time,
                builds.output,
                releases.default_target
             FROM builds
             INNER JOIN releases ON releases.id = builds.rid
             INNER JOIN crates ON releases.crate_id = crates.id
             WHERE builds.id = $1 AND crates.name = $2 AND releases.version = $3",
            &[&id, &name, &version]
        )
    );

    let build_details = if let Some(row) = row {
        let output = if let Some(output) = row.get("output") {
            output
        } else {
            let target: String = row.get("default_target");
            let path = format!("build-logs/{}/{}.txt", id, target);
            let file = ctry!(req, File::from_path(&storage, &path, &config));
            ctry!(req, String::from_utf8(file.0.content))
        };
        BuildDetails {
            id,
            rustc_version: row.get("rustc_version"),
            docsrs_version: row.get("docsrs_version"),
            build_status: row.get("build_status"),
            build_time: row.get("build_time"),
            output,
        }
    } else {
        return Err(Nope::BuildNotFound.into());
    };

    BuildDetailsPage {
        metadata: cexpect!(req, MetaData::from_crate(&mut conn, &name, &version)),
        build_details,
    }
    .into_response(req)
}

#[cfg(test)]
mod tests {
    use crate::test::{wrapper, FakeBuild};
    use kuchiki::traits::TendrilSink;

    #[test]
    fn db_build_logs() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.1.0")
                .builds(vec![FakeBuild::default()
                    .no_s3_build_log()
                    .db_build_log("A build log")])
                .create()?;

            let page = kuchiki::parse_html().one(
                env.frontend()
                    .get("/crate/foo/0.1.0/builds")
                    .send()?
                    .text()?,
            );

            let node = page.select("ul > li a.release").unwrap().next().unwrap();
            let attrs = node.attributes.borrow();
            let url = attrs.get("href").unwrap();

            let page = kuchiki::parse_html().one(env.frontend().get(url).send()?.text()?);

            let log = page.select("pre").unwrap().next().unwrap().text_contents();

            assert!(log.contains("A build log"));

            Ok(())
        });
    }

    #[test]
    fn s3_build_logs() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.1.0")
                .builds(vec![FakeBuild::default().s3_build_log("A build log")])
                .create()?;

            let page = kuchiki::parse_html().one(
                env.frontend()
                    .get("/crate/foo/0.1.0/builds")
                    .send()?
                    .text()?,
            );

            let node = page.select("ul > li a.release").unwrap().next().unwrap();
            let attrs = node.attributes.borrow();
            let url = attrs.get("href").unwrap();

            let page = kuchiki::parse_html().one(env.frontend().get(url).send()?.text()?);

            let log = page.select("pre").unwrap().next().unwrap().text_contents();

            assert!(log.contains("A build log"));

            Ok(())
        });
    }

    #[test]
    fn both_build_logs() {
        wrapper(|env| {
            env.fake_release()
                .name("foo")
                .version("0.1.0")
                .builds(vec![FakeBuild::default()
                    .s3_build_log("A build log")
                    .db_build_log("Another build log")])
                .create()?;

            let page = kuchiki::parse_html().one(
                env.frontend()
                    .get("/crate/foo/0.1.0/builds")
                    .send()?
                    .text()?,
            );

            let node = page.select("ul > li a.release").unwrap().next().unwrap();
            let attrs = node.attributes.borrow();
            let url = attrs.get("href").unwrap();

            let page = kuchiki::parse_html().one(env.frontend().get(url).send()?.text()?);

            let log = page.select("pre").unwrap().next().unwrap().text_contents();

            // Relatively arbitrarily the DB is prioritised
            assert!(log.contains("Another build log"));

            Ok(())
        });
    }

    #[test]
    fn non_existing_build() {
        wrapper(|env| {
            env.fake_release().name("foo").version("0.1.0").create()?;

            let res = env.frontend().get("/crate/foo/0.1.0/builds/42").send()?;
            assert_eq!(res.status(), 404);
            // TODO: blocked on https://github.com/rust-lang/docs.rs/issues/55
            // assert!(res.text()?.contains("no such build"));

            Ok(())
        });
    }
}
