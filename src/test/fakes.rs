use super::TestDatabase;
use crate::docbuilder::{BuildResult, DocCoverage};
use crate::index::api::{CrateData, CrateOwner, ReleaseData};
use crate::storage::Storage;
use crate::utils::{Dependency, MetadataPackage, Target};
use chrono::{DateTime, Utc};
use failure::{Error, ResultExt};
use postgres::Client;
use std::collections::HashMap;
use std::sync::Arc;

#[must_use = "FakeRelease does nothing until you call .create()"]
pub(crate) struct FakeRelease<'a> {
    db: &'a TestDatabase,
    storage: Arc<Storage>,
    package: MetadataPackage,
    builds: Vec<FakeBuild>,
    /// name, content
    source_files: Vec<(&'a str, &'a [u8])>,
    /// name, content
    rustdoc_files: Vec<(&'a str, &'a [u8])>,
    doc_targets: Vec<String>,
    default_target: Option<&'a str>,
    registry_crate_data: CrateData,
    registry_release_data: ReleaseData,
    has_docs: bool,
    has_examples: bool,
    /// This stores the content, while `package.readme` stores the filename
    readme: Option<&'a str>,
    github_stats: Option<FakeGithubStats>,
    doc_coverage: Option<DocCoverage>,
}

pub(crate) struct FakeBuild {
    s3_build_log: Option<String>,
    db_build_log: Option<String>,
    result: BuildResult,
}

const DEFAULT_CONTENT: &[u8] =
    b"<html><head></head><body>default content for test/fakes</body></html>";

impl<'a> FakeRelease<'a> {
    pub(super) fn new(db: &'a TestDatabase, storage: Arc<Storage>) -> Self {
        FakeRelease {
            db,
            storage,
            package: MetadataPackage {
                id: "fake-package-id".into(),
                name: "fake-package".into(),
                version: "1.0.0".into(),
                license: Some("MIT".into()),
                repository: Some("https://git.example.com".into()),
                homepage: Some("https://www.example.com".into()),
                description: Some("Fake package".into()),
                documentation: Some("https://docs.example.com".into()),
                dependencies: vec![Dependency {
                    name: "fake-dependency".into(),
                    req: "^1.0.0".into(),
                    kind: None,
                    rename: None,
                    optional: false,
                }],
                targets: vec![Target::dummy_lib("fake_package".into(), None)],
                readme: None,
                keywords: vec!["fake".into(), "package".into()],
                features: [
                    ("default".into(), vec!["feature1".into(), "feature3".into()]),
                    ("feature1".into(), Vec::new()),
                    ("feature2".into(), vec!["feature1".into()]),
                    ("feature3".into(), Vec::new()),
                ]
                .iter()
                .cloned()
                .collect::<HashMap<String, Vec<String>>>(),
            },
            builds: vec![],
            source_files: Vec::new(),
            rustdoc_files: Vec::new(),
            doc_targets: Vec::new(),
            default_target: None,
            registry_crate_data: CrateData { owners: Vec::new() },
            registry_release_data: ReleaseData {
                release_time: Utc::now(),
                yanked: false,
                downloads: 0,
            },
            has_docs: true,
            has_examples: false,
            readme: None,
            github_stats: None,
            doc_coverage: None,
        }
    }

    pub(crate) fn downloads(mut self, downloads: i32) -> Self {
        self.registry_release_data.downloads = downloads;
        self
    }

    pub(crate) fn description(mut self, new: impl Into<String>) -> Self {
        self.package.description = Some(new.into());
        self
    }

    pub(crate) fn release_time(mut self, new: DateTime<Utc>) -> Self {
        self.registry_release_data.release_time = new;
        self
    }

    pub(crate) fn name(mut self, new: &str) -> Self {
        self.package.name = new.into();
        self.package.id = format!("{}-id", new);
        self.package.targets[0].name = new.into();
        self
    }

    pub(crate) fn version(mut self, new: &str) -> Self {
        self.package.version = new.into();
        self
    }

    pub(crate) fn repo(mut self, repo: impl Into<String>) -> Self {
        self.package.repository = Some(repo.into());
        self
    }

    /// Shortcut to add a single unsuccessful build with default data
    // TODO: How should `has_docs` actually be handled?
    pub(crate) fn build_result_failed(self) -> Self {
        assert!(
            self.builds.is_empty(),
            "cannot use custom builds with build_result_failed"
        );
        Self {
            has_docs: false,
            builds: vec![FakeBuild::default().successful(false)],
            ..self
        }
    }

    pub(crate) fn builds(self, builds: Vec<FakeBuild>) -> Self {
        assert!(self.builds.is_empty());
        assert!(!builds.is_empty());
        Self { builds, ..self }
    }

    pub(crate) fn yanked(mut self, new: bool) -> Self {
        self.registry_release_data.yanked = new;
        self
    }

    /// Since we switched to LOL HTML, all data must have a valid <head> and <body>.
    /// To avoid duplicating them in every test, this just makes up some content.
    pub(crate) fn rustdoc_file(mut self, path: &'a str) -> Self {
        self.rustdoc_files.push((path, DEFAULT_CONTENT));
        self
    }

    pub(crate) fn rustdoc_file_with(mut self, path: &'a str, data: &'a [u8]) -> Self {
        self.rustdoc_files.push((path, data));
        self
    }

    pub(crate) fn source_file(mut self, path: &'a str, data: &'a [u8]) -> Self {
        self.source_files.push((path, data));
        self
    }

    pub(crate) fn default_target(mut self, target: &'a str) -> Self {
        self = self.add_target(target);
        self.default_target = Some(target);
        self
    }

    pub(crate) fn add_target(mut self, target: &str) -> Self {
        self.doc_targets.push(target.into());
        self
    }

    pub(crate) fn binary(mut self, bin: bool) -> Self {
        self.has_docs = !bin;
        if bin {
            for target in self.package.targets.iter_mut() {
                target.crate_types = vec!["bin".into()];
            }
        }
        self
    }

    pub(crate) fn keywords(mut self, keywords: Vec<String>) -> Self {
        self.package.keywords = keywords;
        self
    }

    pub(crate) fn add_platform<S: Into<String>>(mut self, platform: S) -> Self {
        let platform = platform.into();
        let name = self.package.targets[0].name.clone();
        let target = Target::dummy_lib(name, Some(platform.clone()));
        self.package.targets.push(target);
        self.doc_targets.push(platform);
        self
    }

    /// NOTE: this should be markdown. It will be rendered as HTML when served.
    pub(crate) fn readme(mut self, content: &'a str) -> Self {
        self.readme = Some(content);
        self.source_file("README.md", content.as_bytes())
    }

    pub(crate) fn add_owner(mut self, owner: CrateOwner) -> Self {
        self.registry_crate_data.owners.push(owner);
        self
    }

    pub(crate) fn doc_coverage(self, doc_coverage: DocCoverage) -> Self {
        Self {
            doc_coverage: Some(doc_coverage),
            ..self
        }
    }

    pub(crate) fn features(mut self, features: HashMap<String, Vec<String>>) -> Self {
        self.package.features = features;
        self
    }

    pub(crate) fn github_stats(
        mut self,
        repo: impl Into<String>,
        stars: i32,
        forks: i32,
        issues: i32,
    ) -> Self {
        self.github_stats = Some(FakeGithubStats {
            repo: repo.into(),
            stars,
            forks,
            issues,
        });
        self
    }

    /// Returns the release_id
    pub(crate) fn create(mut self) -> Result<i32, Error> {
        use std::fs;
        use std::path::Path;

        let tempdir = tempfile::Builder::new().prefix("docs.rs-fake").tempdir()?;
        let package = self.package;
        let db = self.db;
        let mut rustdoc_files = self.rustdoc_files;
        let storage = self.storage;

        // Upload all source files as rustdoc files
        // In real life, these would be highlighted HTML, but for testing we just use the files themselves.
        for (source_path, data) in &self.source_files {
            if let Some(src) = source_path.strip_prefix("src/") {
                let mut updated = ["src", &package.name, src].join("/");
                updated += ".html";
                let source_html = format!(
                    "<html><head></head><body>{}</body></html>",
                    std::str::from_utf8(data).expect("invalid utf8")
                );
                rustdoc_files.push((
                    Box::leak(Box::new(updated)),
                    Box::leak(source_html.into_bytes().into_boxed_slice()),
                ));
            }
        }

        let upload_files = |prefix: &str, files: &[(&str, &[u8])], target: Option<&str>| {
            let mut path_prefix = tempdir.path().join(prefix);
            if let Some(target) = target {
                path_prefix.push(target);
            }
            fs::create_dir(&path_prefix)?;

            for (path, data) in files {
                if path.starts_with('/') {
                    failure::bail!("absolute paths not supported");
                }
                // allow `src/main.rs`
                if let Some(parent) = Path::new(path).parent() {
                    let path = path_prefix.join(parent);
                    fs::create_dir_all(&path)
                        .with_context(|_| format!("failed to create {}", path.display()))?;
                }
                let file = path_prefix.join(&path);
                log::debug!("writing file {}", file.display());
                fs::write(file, data)?;
            }

            let prefix = format!(
                "{}/{}/{}/{}",
                prefix,
                package.name,
                package.version,
                target.unwrap_or("")
            );
            log::debug!("adding directory {} from {}", prefix, path_prefix.display());
            crate::db::add_path_into_database(&storage, &prefix, path_prefix)
        };

        let (source_meta, mut algs) = upload_files("source", &self.source_files, None)?;
        log::debug!("added source files {}", source_meta);

        // If the test didn't add custom builds, inject a default one
        if self.builds.is_empty() {
            self.builds.push(FakeBuild::default());
        }
        let last_build_result = &self.builds.last().unwrap().result;

        if last_build_result.successful {
            let index = [&package.name, "index.html"].join("/");
            if package.is_library() && !rustdoc_files.iter().any(|(path, _)| path == &index) {
                rustdoc_files.push((&index, DEFAULT_CONTENT));
            }

            let (rustdoc_meta, new_algs) = upload_files("rustdoc", &rustdoc_files, None)?;
            algs.extend(new_algs);
            log::debug!("added rustdoc files {}", rustdoc_meta);

            for target in &package.targets[1..] {
                let platform = target.src_path.as_ref().unwrap();
                upload_files("rustdoc", &rustdoc_files, Some(platform))?;
                log::debug!("added platform files for {}", platform);
            }
        }

        let repository = match self.github_stats {
            Some(stats) => Some(stats.create(&mut self.db.conn())?),
            None => None,
        };

        let crate_dir = tempdir.path();
        if let Some(markdown) = self.readme {
            fs::write(crate_dir.join("README.md"), markdown)?;
        }

        // Many tests rely on the default-target being linux, so it should not
        // be set to docsrs_metadata::HOST_TARGET, because then tests fail on all
        // non-linux platforms.
        let default_target = self.default_target.unwrap_or("x86_64-unknown-linux-gnu");
        let release_id = crate::db::add_package_into_database(
            &mut db.conn(),
            &package,
            crate_dir,
            last_build_result,
            default_target,
            source_meta,
            self.doc_targets,
            &self.registry_release_data,
            self.has_docs,
            self.has_examples,
            algs,
            repository,
        )?;
        crate::db::update_crate_data_in_database(
            &mut db.conn(),
            &package.name,
            &self.registry_crate_data,
        )?;
        for build in &self.builds {
            build.create(&mut db.conn(), &*storage, release_id, default_target)?;
        }
        if let Some(coverage) = self.doc_coverage {
            crate::db::add_doc_coverage(&mut db.conn(), release_id, coverage)?;
        }

        Ok(release_id)
    }
}

struct FakeGithubStats {
    repo: String,
    stars: i32,
    forks: i32,
    issues: i32,
}

impl FakeGithubStats {
    fn create(&self, conn: &mut Client) -> Result<i32, Error> {
        let existing_count: i64 = conn
            .query_one("SELECT COUNT(*) FROM repositories;", &[])?
            .get(0);
        let host_id = base64::encode(format!("FAKE ID {}", existing_count));

        let data = conn.query(
            "INSERT INTO repositories (host, host_id, name, description, last_commit, stars, forks, issues, updated_at)
             VALUES ('github.com', $1, $2, 'Fake description!', NOW(), $3, $4, $5, NOW())
             ON CONFLICT (host, name) DO UPDATE SET stars = $3, forks = $4, issues = $5
             RETURNING id;",
            &[&host_id, &self.repo, &self.stars, &self.forks, &self.issues],
        )?;
        assert_eq!(data.len(), 1);

        Ok(data[0].get(0))
    }
}

impl FakeBuild {
    pub(crate) fn rustc_version(self, rustc_version: impl Into<String>) -> Self {
        Self {
            result: BuildResult {
                rustc_version: rustc_version.into(),
                ..self.result
            },
            ..self
        }
    }

    pub(crate) fn docsrs_version(self, docsrs_version: impl Into<String>) -> Self {
        Self {
            result: BuildResult {
                docsrs_version: docsrs_version.into(),
                ..self.result
            },
            ..self
        }
    }

    pub(crate) fn s3_build_log(self, build_log: impl Into<String>) -> Self {
        Self {
            s3_build_log: Some(build_log.into()),
            ..self
        }
    }

    pub(crate) fn db_build_log(self, build_log: impl Into<String>) -> Self {
        Self {
            db_build_log: Some(build_log.into()),
            ..self
        }
    }

    pub(crate) fn no_s3_build_log(self) -> Self {
        Self {
            s3_build_log: None,
            ..self
        }
    }

    pub(crate) fn successful(self, successful: bool) -> Self {
        Self {
            result: BuildResult {
                successful,
                ..self.result
            },
            ..self
        }
    }

    fn create(
        &self,
        conn: &mut Client,
        storage: &Storage,
        release_id: i32,
        default_target: &str,
    ) -> Result<(), Error> {
        let build_id = crate::db::add_build_into_database(conn, release_id, &self.result)?;

        if let Some(db_build_log) = self.db_build_log.as_deref() {
            conn.query(
                "UPDATE builds SET output = $2 WHERE id = $1",
                &[&build_id, &db_build_log],
            )?;
        }

        if let Some(s3_build_log) = self.s3_build_log.as_deref() {
            let path = format!("build-logs/{}/{}.txt", build_id, default_target);
            storage.store_one(path, s3_build_log)?;
        }

        Ok(())
    }
}

impl Default for FakeBuild {
    fn default() -> Self {
        Self {
            s3_build_log: Some("It works!".into()),
            db_build_log: None,
            result: BuildResult {
                rustc_version: "rustc 2.0.0-nightly (000000000 1970-01-01)".into(),
                docsrs_version: "docs.rs 1.0.0 (000000000 1970-01-01)".into(),
                successful: true,
            },
        }
    }
}
