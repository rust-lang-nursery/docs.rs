use crate::db::file::add_path_into_database;
use crate::db::{
    add_build_into_database, add_doc_coverage, add_package_into_database,
    add_path_into_remote_archive, update_crate_data_in_database, Pool,
};
use crate::docbuilder::{crates::crates_from_path, Limits};
use crate::error::Result;
use crate::index::api::ReleaseData;
use crate::repositories::RepositoryStatsUpdater;
use crate::storage::CompressionAlgorithms;
use crate::utils::{copy_dir_all, parse_rustc_version, CargoMetadata};
use crate::{db::blacklist::is_blacklisted, utils::MetadataPackage};
use crate::{Config, Context, Index, Metrics, Storage};
use docsrs_metadata::{Metadata, DEFAULT_TARGETS, HOST_TARGET};
use log::{debug, info, warn, LevelFilter};
use postgres::Client;
use rustwide::cmd::{Command, SandboxBuilder, SandboxImage};
use rustwide::logging::{self, LogStorage};
use rustwide::toolchain::ToolchainError;
use rustwide::{Build, Crate, Toolchain, Workspace, WorkspaceBuilder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

const USER_AGENT: &str = "docs.rs builder (https://github.com/rust-lang/docs.rs)";
const DUMMY_CRATE_NAME: &str = "empty-library";
const DUMMY_CRATE_VERSION: &str = "1.0.0";

pub enum PackageKind<'a> {
    Local(&'a Path),
    CratesIo,
    Registry(&'a str),
}

pub struct RustwideBuilder {
    workspace: Workspace,
    toolchain: Toolchain,
    config: Arc<Config>,
    db: Pool,
    storage: Arc<Storage>,
    metrics: Arc<Metrics>,
    index: Arc<Index>,
    rustc_version: String,
    repository_stats_updater: Arc<RepositoryStatsUpdater>,
    skip_build_if_exists: bool,
}

impl RustwideBuilder {
    pub fn init(context: &dyn Context) -> Result<Self> {
        let config = context.config()?;

        let mut builder = WorkspaceBuilder::new(&config.rustwide_workspace, USER_AGENT)
            .running_inside_docker(config.inside_docker);
        if let Some(custom_image) = &config.local_docker_image {
            builder = builder.sandbox_image(SandboxImage::local(&custom_image)?);
        }
        if cfg!(test) {
            builder = builder.fast_init(true);
        }

        let workspace = builder.init()?;
        workspace.purge_all_build_dirs()?;

        let toolchain = Toolchain::dist(&config.toolchain);

        Ok(RustwideBuilder {
            workspace,
            toolchain,
            config,
            db: context.pool()?,
            storage: context.storage()?,
            metrics: context.metrics()?,
            index: context.index()?,
            rustc_version: String::new(),
            repository_stats_updater: context.repository_stats_updater()?,
            skip_build_if_exists: false,
        })
    }

    pub fn set_skip_build_if_exists(&mut self, should: bool) {
        self.skip_build_if_exists = should;
    }

    fn prepare_sandbox(&self, limits: &Limits) -> SandboxBuilder {
        SandboxBuilder::new()
            .cpu_limit(self.config.build_cpu_limit.map(|limit| limit as f32))
            .memory_limit(Some(limits.memory()))
            .enable_networking(limits.networking())
    }

    pub fn update_toolchain(&mut self) -> Result<()> {
        // Ignore errors if detection fails.
        let old_version = self.detect_rustc_version().ok();

        let mut targets_to_install = DEFAULT_TARGETS
            .iter()
            .map(|&t| t.to_string()) // &str has a specialized ToString impl, while &&str goes through Display
            .collect::<HashSet<_>>();

        let installed_targets = match self.toolchain.installed_targets(&self.workspace) {
            Ok(targets) => targets,
            Err(err) => {
                if let Some(&ToolchainError::NotInstalled) = err.downcast_ref::<ToolchainError>() {
                    Vec::new()
                } else {
                    return Err(err);
                }
            }
        };

        // The extra targets are intentionally removed *before* trying to update.
        //
        // If a target is installed locally and it goes missing the next update, rustup will block
        // the update to avoid leaving the system in a broken state. This is not a behavior we want
        // though when we also remove the target from the list managed by docs.rs: we want that
        // target gone, and we don't care if it's missing in the next update.
        //
        // Removing it beforehand works fine, and prevents rustup from blocking the update later in
        // the method.
        //
        // Note that this means that non tier-one targets will be uninstalled on every update,
        // and will not be reinstalled until explicitly requested by a crate.
        for target in installed_targets {
            if !targets_to_install.remove(&target) {
                self.toolchain.remove_target(&self.workspace, &target)?;
            }
        }

        self.toolchain.install(&self.workspace)?;

        for target in &targets_to_install {
            self.toolchain.add_target(&self.workspace, target)?;
        }
        // NOTE: rustup will automatically refuse to update the toolchain
        // if `rustfmt` is not available in the newer version
        // NOTE: this ignores the error so that you can still run a build without rustfmt.
        // This should only happen if you run a build for the first time when rustfmt isn't available.
        if let Err(err) = self.toolchain.add_component(&self.workspace, "rustfmt") {
            log::warn!("failed to install rustfmt: {}", err);
            log::info!("continuing anyway, since this must be the first build");
        }

        self.rustc_version = self.detect_rustc_version()?;
        if old_version.as_deref() != Some(&self.rustc_version) {
            self.add_essential_files()?;
        }

        Ok(())
    }

    fn detect_rustc_version(&self) -> Result<String> {
        info!("detecting rustc's version...");
        let res = Command::new(&self.workspace, self.toolchain.rustc())
            .args(&["--version"])
            .log_output(false)
            .run_capture()?;
        let mut iter = res.stdout_lines().iter();
        if let (Some(line), None) = (iter.next(), iter.next()) {
            info!("found rustc {}", line);
            Ok(line.clone())
        } else {
            Err(::failure::err_msg(
                "invalid output returned by `rustc --version`",
            ))
        }
    }

    pub fn add_essential_files(&mut self) -> Result<()> {
        self.rustc_version = self.detect_rustc_version()?;
        let rustc_version = parse_rustc_version(&self.rustc_version)?;

        info!("building a dummy crate to get essential files");

        let mut conn = self.db.get()?;
        let limits = Limits::for_crate(&mut conn, DUMMY_CRATE_NAME)?;

        let mut build_dir = self
            .workspace
            .build_dir(&format!("essential-files-{}", rustc_version));
        build_dir.purge()?;

        // This is an empty library crate that is supposed to always build.
        let krate = Crate::crates_io(DUMMY_CRATE_NAME, DUMMY_CRATE_VERSION);
        krate.fetch(&self.workspace)?;

        build_dir
            .build(&self.toolchain, &krate, self.prepare_sandbox(&limits))
            .run(|build| {
                let metadata = Metadata::from_crate_root(&build.host_source_dir())?;

                let res = self.execute_build(HOST_TARGET, true, build, &limits, &metadata, true)?;
                if !res.result.successful {
                    failure::bail!("failed to build dummy crate for {}", self.rustc_version);
                }

                info!("copying essential files for {}", self.rustc_version);
                let source = build.host_target_dir().join("doc");
                let dest = tempfile::Builder::new()
                    .prefix("essential-files")
                    .tempdir()?;
                copy_dir_all(source, &dest)?;
                add_path_into_database(&self.storage, "", &dest)?;
                conn.query(
                    "INSERT INTO config (name, value) VALUES ('rustc_version', $1) \
                     ON CONFLICT (name) DO UPDATE SET value = $1;",
                    &[&Value::String(self.rustc_version.clone())],
                )?;

                Ok(())
            })?;

        build_dir.purge()?;
        krate.purge_from_cache(&self.workspace)?;
        Ok(())
    }

    pub fn build_world(&mut self) -> Result<()> {
        crates_from_path(
            &self.config.registry_index_path.clone(),
            &mut |name, version| {
                let registry_url = self.config.registry_url.clone();
                let package_kind = registry_url
                    .as_ref()
                    .map(|r| PackageKind::Registry(r.as_str()))
                    .unwrap_or(PackageKind::CratesIo);
                if let Err(err) = self.build_package(name, version, package_kind) {
                    warn!("failed to build package {} {}: {}", name, version, err);
                }
            },
        )
    }

    pub fn build_local_package(&mut self, path: &Path) -> Result<bool> {
        self.update_toolchain()?;
        let metadata =
            CargoMetadata::load(&self.workspace, &self.toolchain, path).map_err(|err| {
                err.context(format!("failed to load local package {}", path.display()))
            })?;
        let package = metadata.root();
        self.build_package(&package.name, &package.version, PackageKind::Local(path))
    }

    pub fn build_package(
        &mut self,
        name: &str,
        version: &str,
        kind: PackageKind<'_>,
    ) -> Result<bool> {
        let mut conn = self.db.get()?;

        if !self.should_build(&mut conn, name, version)? {
            return Ok(false);
        }

        self.update_toolchain()?;

        info!("building package {} {}", name, version);

        if is_blacklisted(&mut conn, name)? {
            info!("skipping build of {}, crate has been blacklisted", name);
            return Ok(false);
        }

        let limits = Limits::for_crate(&mut conn, name)?;
        #[cfg(target_os = "linux")]
        if !self.config.disable_memory_limit {
            let mem_info = procfs::Meminfo::new().context("failed to read /proc/meminfo")?;
            let available = mem_info
                .mem_available
                .expect("kernel version too old for determining memory limit");
            if limits.memory() as u64 > available {
                failure::bail!("not enough memory to build {} {}: needed {} MiB, have {} MiB\nhelp: set DOCSRS_DISABLE_MEMORY_LIMIT=true to force a build",
                    name, version, limits.memory() / 1024 / 1024, available / 1024 / 1024
                );
            } else {
                debug!(
                    "had enough memory: {} MiB <= {} MiB",
                    limits.memory() / 1024 / 1024,
                    available / 1024 / 1024
                );
            }
        }

        let mut build_dir = self.workspace.build_dir(&format!("{}-{}", name, version));
        build_dir.purge()?;

        let krate = match kind {
            PackageKind::Local(path) => Crate::local(path),
            PackageKind::CratesIo => Crate::crates_io(name, version),
            PackageKind::Registry(registry) => Crate::registry(registry, name, version),
        };
        krate.fetch(&self.workspace)?;

        let local_storage = tempfile::Builder::new().prefix("docsrs-docs").tempdir()?;

        let successful = build_dir
            .build(&self.toolchain, &krate, self.prepare_sandbox(&limits))
            .run(|build| {
                use docsrs_metadata::BuildTargets;

                let mut has_docs = false;
                let mut successful_targets = Vec::new();
                let metadata = Metadata::from_crate_root(&build.host_source_dir())?;
                let BuildTargets {
                    default_target,
                    other_targets,
                } = metadata.targets(self.config.include_default_targets);

                // Perform an initial build
                let res =
                    self.execute_build(default_target, true, &build, &limits, &metadata, false)?;
                if res.result.successful {
                    if let Some(name) = res.cargo_metadata.root().library_name() {
                        let host_target = build.host_target_dir();
                        has_docs = host_target.join("doc").join(name).is_dir();
                    }
                }

                let mut algs = HashSet::new();
                if has_docs {
                    debug!("adding documentation for the default target to the database");
                    self.copy_docs(&build.host_target_dir(), local_storage.path(), "", true)?;

                    successful_targets.push(res.target.clone());

                    // Then build the documentation for all the targets
                    // Limit the number of targets so that no one can try to build all 200000 possible targets
                    for target in other_targets.into_iter().take(limits.targets()) {
                        debug!("building package {} {} for {}", name, version, target);
                        self.build_target(
                            target,
                            &build,
                            &limits,
                            &local_storage.path(),
                            &mut successful_targets,
                            &metadata,
                        )?;
                    }

                    let (_, new_alg) = add_path_into_remote_archive(
                        &self.storage,
                        &format!("rustdoc/{0}/rustdoc-{0}-{1}.zip", name, version),
                        local_storage.path(),
                    )?;
                    algs.insert(new_alg);
                };

                // Store the sources even if the build fails
                debug!("adding sources into database");
                // TODO: move generating the archive path to somewhere else
                let archive = format!("sources/{0}/sources-{0}-{1}.zip", name, version);
                let (files_list, new_alg) =
                    add_path_into_remote_archive(&self.storage, &archive, build.host_source_dir())?;
                algs.insert(new_alg);

                let has_examples = build.host_source_dir().join("examples").is_dir();
                if res.result.successful {
                    self.metrics.successful_builds.inc();
                } else if res.cargo_metadata.root().is_library() {
                    self.metrics.failed_builds.inc();
                } else {
                    self.metrics.non_library_builds.inc();
                }

                let release_data = match self.index.api().get_release_data(name, version) {
                    Ok(data) => data,
                    Err(err) => {
                        warn!("{:#?}", err);
                        ReleaseData::default()
                    }
                };

                let cargo_metadata = res.cargo_metadata.root();
                let repository = self.get_repo(cargo_metadata)?;

                let release_id = add_package_into_database(
                    &mut conn,
                    cargo_metadata,
                    &build.host_source_dir(),
                    &res.result,
                    &res.target,
                    files_list,
                    successful_targets,
                    &release_data,
                    has_docs,
                    has_examples,
                    algs,
                    repository,
                    true,
                )?;

                if let Some(doc_coverage) = res.doc_coverage {
                    add_doc_coverage(&mut conn, release_id, doc_coverage)?;
                }

                let build_id = add_build_into_database(&mut conn, release_id, &res.result)?;
                let build_log_path = format!("build-logs/{}/{}.txt", build_id, default_target);
                self.storage.store_one(build_log_path, res.build_log)?;

                // Some crates.io crate data is mutable, so we proactively update it during a release
                match self.index.api().get_crate_data(name) {
                    Ok(crate_data) => update_crate_data_in_database(&mut conn, name, &crate_data)?,
                    Err(err) => warn!("{:#?}", err),
                }

                Ok(res.result.successful)
            })?;

        build_dir.purge()?;
        krate.purge_from_cache(&self.workspace)?;
        local_storage.close()?;
        Ok(successful)
    }

    fn build_target(
        &self,
        target: &str,
        build: &Build,
        limits: &Limits,
        local_storage: &Path,
        successful_targets: &mut Vec<String>,
        metadata: &Metadata,
    ) -> Result<()> {
        let target_res = self.execute_build(target, false, build, limits, metadata, false)?;
        if target_res.result.successful {
            // Cargo is not giving any error and not generating documentation of some crates
            // when we use a target compile options. Check documentation exists before
            // adding target to successfully_targets.
            if build.host_target_dir().join(target).join("doc").is_dir() {
                debug!("adding documentation for target {} to the database", target,);
                self.copy_docs(&build.host_target_dir(), local_storage, target, false)?;
                successful_targets.push(target.to_string());
            }
        }
        Ok(())
    }

    fn get_coverage(
        &self,
        target: &str,
        build: &Build,
        metadata: &Metadata,
        limits: &Limits,
    ) -> Result<Option<DocCoverage>> {
        let rustdoc_flags = vec![
            "--output-format".to_string(),
            "json".to_string(),
            "--show-coverage".to_string(),
        ];

        #[derive(serde::Deserialize)]
        struct FileCoverage {
            total: i32,
            with_docs: i32,
            total_examples: i32,
            with_examples: i32,
        }

        let mut coverage = DocCoverage {
            total_items: 0,
            documented_items: 0,
            total_items_needing_examples: 0,
            items_with_examples: 0,
        };

        self.prepare_command(build, target, metadata, limits, rustdoc_flags)?
            .process_lines(&mut |line, _| {
                if line.starts_with('{') && line.ends_with('}') {
                    let parsed = match serde_json::from_str::<HashMap<String, FileCoverage>>(line) {
                        Ok(parsed) => parsed,
                        Err(_) => return,
                    };
                    for file in parsed.values() {
                        coverage.total_items += file.total;
                        coverage.documented_items += file.with_docs;
                        coverage.total_items_needing_examples += file.total_examples;
                        coverage.items_with_examples += file.with_examples;
                    }
                }
            })
            .log_output(false)
            .run()?;

        Ok(
            if coverage.total_items == 0 && coverage.documented_items == 0 {
                None
            } else {
                Some(coverage)
            },
        )
    }

    fn execute_build(
        &self,
        target: &str,
        is_default_target: bool,
        build: &Build,
        limits: &Limits,
        metadata: &Metadata,
        create_essential_files: bool,
    ) -> Result<FullBuildResult> {
        let cargo_metadata =
            CargoMetadata::load(&self.workspace, &self.toolchain, &build.host_source_dir())?;

        let mut rustdoc_flags = vec![if create_essential_files {
            "--emit=unversioned-shared-resources,toolchain-shared-resources"
        } else {
            "--emit=invocation-specific"
        }
        .to_string()];
        rustdoc_flags.extend(vec![
            "--resource-suffix".to_string(),
            format!("-{}", parse_rustc_version(&self.rustc_version)?),
        ]);

        let mut storage = LogStorage::new(LevelFilter::Info);
        storage.set_max_size(limits.max_log_size());

        let successful = logging::capture(&storage, || {
            self.prepare_command(build, target, metadata, limits, rustdoc_flags)
                .and_then(|command| command.run().map_err(failure::Error::from))
                .is_ok()
        });
        let doc_coverage = if successful {
            self.get_coverage(target, build, metadata, limits)?
        } else {
            None
        };
        // If we're passed a default_target which requires a cross-compile,
        // cargo will put the output in `target/<target>/doc`.
        // However, if this is the default build, we don't want it there,
        // we want it in `target/doc`.
        // NOTE: don't rename this if the build failed, because `target/<target>/doc` won't exist.
        if successful && target != HOST_TARGET && is_default_target {
            // mv target/$target/doc target/doc
            let target_dir = build.host_target_dir();
            let old_dir = target_dir.join(target).join("doc");
            let new_dir = target_dir.join("doc");
            debug!("rename {} to {}", old_dir.display(), new_dir.display());
            std::fs::rename(old_dir, new_dir)?;
        }

        Ok(FullBuildResult {
            result: BuildResult {
                rustc_version: self.rustc_version.clone(),
                docsrs_version: format!("docsrs {}", crate::BUILD_VERSION),
                successful,
            },
            doc_coverage,
            cargo_metadata,
            build_log: storage.to_string(),
            target: target.to_string(),
        })
    }

    fn prepare_command<'ws, 'pl>(
        &self,
        build: &'ws Build,
        target: &str,
        metadata: &Metadata,
        limits: &Limits,
        mut rustdoc_flags_extras: Vec<String>,
    ) -> Result<Command<'ws, 'pl>> {
        // If the explicit target is not a tier one target, we need to install it.
        if !docsrs_metadata::DEFAULT_TARGETS.contains(&target) {
            // This is a no-op if the target is already installed.
            self.toolchain.add_target(&self.workspace, target)?;
        }

        // Add docs.rs specific arguments
        let mut cargo_args = vec![
            // We know that `metadata` unconditionally passes `-Z rustdoc-map`.
            // Don't copy paste this, since that fact is not stable and may change in the future.
            "-Zunstable-options".into(),
            r#"--config=doc.extern-map.registries.crates-io="https://docs.rs""#.into(),
        ];
        if let Some(cpu_limit) = self.config.build_cpu_limit {
            cargo_args.push(format!("-j{}", cpu_limit));
        }
        if target != HOST_TARGET {
            cargo_args.push("--target".into());
            cargo_args.push(target.into());
        };

        #[rustfmt::skip]
        const UNCONDITIONAL_ARGS: &[&str] = &[
            "--static-root-path", "/",
            "--cap-lints", "warn",
            "--disable-per-crate-search",
        ];

        rustdoc_flags_extras.extend(UNCONDITIONAL_ARGS.iter().map(|&s| s.to_owned()));
        let cargo_args = metadata.cargo_args(&cargo_args, &rustdoc_flags_extras);

        let mut command = build
            .cargo()
            .timeout(Some(limits.timeout()))
            .no_output_timeout(None);

        for (key, val) in metadata.environment_variables() {
            command = command.env(key, val);
        }

        Ok(command.args(&cargo_args))
    }

    fn copy_docs(
        &self,
        target_dir: &Path,
        local_storage: &Path,
        target: &str,
        is_default_target: bool,
    ) -> Result<()> {
        let source = target_dir.join(target).join("doc");

        let mut dest = local_storage.to_path_buf();
        // only add target name to destination directory when we are copying a non-default target.
        // this is allowing us to host documents in the root of the crate documentation directory.
        // for example winapi will be available in docs.rs/winapi/$version/winapi/ for it's
        // default target: x86_64-pc-windows-msvc. But since it will be built under
        // target/x86_64-pc-windows-msvc we still need target in this function.
        if !is_default_target {
            dest = dest.join(target);
        }

        info!("{} {}", source.display(), dest.display());
        copy_dir_all(source, dest).map_err(Into::into)
    }

    fn should_build(&self, conn: &mut Client, name: &str, version: &str) -> Result<bool> {
        if self.skip_build_if_exists {
            // Check whether no successful builds are present in the database.
            Ok(conn
                .query(
                    "SELECT 1 FROM crates, releases, builds
                     WHERE crates.id = releases.crate_id AND releases.id = builds.rid
                       AND crates.name = $1 AND releases.version = $2
                       AND builds.build_status = TRUE;",
                    &[&name, &version],
                )?
                .is_empty())
        } else {
            Ok(true)
        }
    }

    fn get_repo(&self, metadata: &MetadataPackage) -> Result<Option<i32>> {
        self.repository_stats_updater.load_repository(metadata)
    }
}

struct FullBuildResult {
    result: BuildResult,
    target: String,
    cargo_metadata: CargoMetadata,
    doc_coverage: Option<DocCoverage>,
    build_log: String,
}

#[derive(Clone, Copy)]
pub(crate) struct DocCoverage {
    /// The total items that could be documented in the current crate, used to calculate
    /// documentation coverage.
    pub(crate) total_items: i32,
    /// The items of the crate that are documented, used to calculate documentation coverage.
    pub(crate) documented_items: i32,
    /// The total items that could have code examples in the current crate, used to calculate
    /// documentation coverage.
    pub(crate) total_items_needing_examples: i32,
    /// The items of the crate that have a code example, used to calculate documentation coverage.
    pub(crate) items_with_examples: i32,
}

pub(crate) struct BuildResult {
    pub(crate) rustc_version: String,
    pub(crate) docsrs_version: String,
    pub(crate) successful: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::{assert_redirect, assert_success, wrapper};

    #[test]
    #[ignore]
    fn test_build_crate() {
        wrapper(|env| {
            let crate_ = DUMMY_CRATE_NAME;
            let crate_path = crate_.replace("-", "_");
            let version = DUMMY_CRATE_VERSION;
            let default_target = "x86_64-unknown-linux-gnu";

            assert_eq!(env.config().include_default_targets, true);

            let mut builder = RustwideBuilder::init(env).unwrap();
            builder
                .build_package(crate_, version, PackageKind::CratesIo)
                .map(|_| ())?;

            // check release record in the db (default and other targets)
            let mut conn = env.db().conn();
            let rows = conn
                .query(
                    "SELECT 
                        r.rustdoc_status,
                        r.default_target,
                        r.doc_targets
                    FROM 
                        crates as c 
                        INNER JOIN releases AS r ON c.id = r.crate_id
                    WHERE 
                        c.name = $1 AND 
                        r.version = $2",
                    &[&crate_, &version],
                )
                .unwrap();
            let row = rows.get(0).unwrap();

            assert_eq!(row.get::<_, bool>("rustdoc_status"), true);
            assert_eq!(row.get::<_, String>("default_target"), default_target);

            let mut targets: Vec<String> = row
                .get::<_, Value>("doc_targets")
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            targets.sort();
            assert_eq!(
                targets,
                vec![
                    "i686-pc-windows-msvc",
                    "i686-unknown-linux-gnu",
                    "x86_64-apple-darwin",
                    "x86_64-pc-windows-msvc",
                    "x86_64-unknown-linux-gnu",
                ]
            );

            let storage = env.storage();
            let web = env.frontend();

            let base = format!("rustdoc/{}/{}", crate_, version);

            // default target was built and is accessible
            assert!(storage.exists(&format!("{}/{}/index.html", base, crate_path))?);
            assert_success(&format!("/{}/{}/{}", crate_, version, crate_path), web)?;

            // other targets too
            for target in DEFAULT_TARGETS {
                let target_docs_present =
                    storage.exists(&format!("{}/{}/{}/index.html", base, target, crate_path))?;

                let target_url = format!(
                    "/{}/{}/{}/{}/index.html",
                    crate_, version, target, crate_path
                );

                if target == &default_target {
                    assert!(!target_docs_present);
                    assert_redirect(
                        &target_url,
                        &format!("/{}/{}/{}/index.html", crate_, version, crate_path),
                        web,
                    )?;
                } else {
                    assert!(target_docs_present);
                    assert_success(&target_url, web)?;
                }
            }

            Ok(())
        })
    }
}
