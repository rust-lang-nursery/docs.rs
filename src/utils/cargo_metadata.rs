use crate::error::Result;
use failure::ResultExt;
use rustwide::{cmd::Command, Toolchain, Workspace};
use std::path::Path;

pub(crate) struct CargoMetadata {
    root: ::cargo_metadata::Package,
}

impl CargoMetadata {
    pub(crate) fn load(
        workspace: &Workspace,
        toolchain: &Toolchain,
        source_dir: &Path,
    ) -> Result<Self> {
        let res = Command::new(workspace, toolchain.cargo())
            .args(&["metadata", "--format-version", "1"])
            .cd(source_dir)
            .log_output(false)
            .run_capture()?;

        let metadata = ::cargo_metadata::MetadataCommand::parse(&res.stdout_lines().join("\n"))
            .context("invalid output returned by `cargo metadata`")?;
        let resolve = metadata
            .resolve
            .ok_or(failure::err_msg("expected resolve metadata"))?;
        let root = resolve
            .root
            .ok_or(failure::err_msg("expected package root"))?;

        Ok(CargoMetadata {
            root: metadata
                .packages
                .into_iter()
                .find(|pkg| pkg.id == root)
                .unwrap(),
        })
    }

    pub(crate) fn root(&self) -> &::cargo_metadata::Package {
        &self.root
    }
}

pub(crate) use cargo_metadata::Package;

pub(crate) trait PackageExt {
    fn library_target(&self) -> Option<&cargo_metadata::Target>;
    fn is_library(&self) -> bool;
    fn normalize_package_name(&self, name: &str) -> String;
    fn package_name(&self) -> String;
    fn library_name(&self) -> Option<String>;
}

impl PackageExt for Package {
    fn library_target(&self) -> Option<&cargo_metadata::Target> {
        self.targets
            .iter()
            .find(|target| target.crate_types.iter().any(|kind| kind != "bin"))
    }

    fn is_library(&self) -> bool {
        self.library_target().is_some()
    }

    fn normalize_package_name(&self, name: &str) -> String {
        name.replace('-', "_")
    }

    fn package_name(&self) -> String {
        self.library_name()
            .unwrap_or_else(|| self.normalize_package_name(&self.targets[0].name))
    }

    fn library_name(&self) -> Option<String> {
        self.library_target()
            .map(|target| self.normalize_package_name(&target.name))
    }
}
