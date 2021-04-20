use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
};

use crate::{
    db::types::Feature,
    docbuilder::{BuildResult, DocCoverage},
    error::Result,
    index::api::{CrateData, CrateOwner, ReleaseData},
    storage::CompressionAlgorithm,
    utils::MetadataPackage,
};
use log::{debug, info};
use postgres::Client;
use serde_json::Value;
use slug::slugify;

/// Adds a package into database.
///
/// Package must be built first.
///
/// NOTE: `source_files` refers to the files originally in the crate,
/// not the files generated by rustdoc.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_package_into_database(
    conn: &mut Client,
    metadata_pkg: &MetadataPackage,
    source_dir: &Path,
    res: &BuildResult,
    default_target: &str,
    source_files: Value,
    doc_targets: Vec<String>,
    registry_data: &ReleaseData,
    has_docs: bool,
    has_examples: bool,
    compression_algorithms: std::collections::HashSet<CompressionAlgorithm>,
    repository_id: Option<i32>,
    archive_storage: bool,
) -> Result<i32> {
    debug!("Adding package into database");
    let crate_id = initialize_package_in_database(conn, metadata_pkg)?;
    let dependencies = convert_dependencies(metadata_pkg);
    let rustdoc = get_rustdoc(metadata_pkg, source_dir).unwrap_or(None);
    let readme = get_readme(metadata_pkg, source_dir).unwrap_or(None);
    let features = get_features(metadata_pkg);
    let is_library = metadata_pkg.is_library();

    let rows = conn.query(
        "INSERT INTO releases (
            crate_id, version, release_time,
            dependencies, target_name, yanked, build_status,
            rustdoc_status, test_status, license, repository_url,
            homepage_url, description, description_long, readme,
            keywords, have_examples, downloads, files,
            doc_targets, is_library, doc_rustc_version,
            documentation_url, default_target, features,
            repository_id, archive_storage
         )
         VALUES (
            $1,  $2,  $3,  $4,  $5,  $6,  $7,  $8,  $9,
            $10, $11, $12, $13, $14, $15, $16, $17, $18,
            $19, $20, $21, $22, $23, $24, $25, $26, $27, 
         )
         ON CONFLICT (crate_id, version) DO UPDATE
            SET release_time = $3,
                dependencies = $4,
                target_name = $5,
                yanked = $6,
                build_status = $7,
                rustdoc_status = $8,
                test_status = $9,
                license = $10,
                repository_url = $11,
                homepage_url = $12,
                description = $13,
                description_long = $14,
                readme = $15,
                keywords = $16,
                have_examples = $17,
                downloads = $18,
                files = $19,
                doc_targets = $20,
                is_library = $21,
                doc_rustc_version = $22,
                documentation_url = $23,
                default_target = $24,
                features = $25,
                repository_id = $26
                archive_storage = $27
         RETURNING id",
        &[
            &crate_id,
            &metadata_pkg.version,
            &registry_data.release_time,
            &serde_json::to_value(&dependencies)?,
            &metadata_pkg.package_name(),
            &registry_data.yanked,
            &res.successful,
            &has_docs,
            &false, // TODO: Add test status somehow
            &metadata_pkg.license,
            &metadata_pkg.repository,
            &metadata_pkg.homepage,
            &metadata_pkg.description,
            &rustdoc,
            &readme,
            &serde_json::to_value(&metadata_pkg.keywords)?,
            &has_examples,
            &registry_data.downloads,
            &source_files,
            &serde_json::to_value(&doc_targets)?,
            &is_library,
            &res.rustc_version,
            &metadata_pkg.documentation,
            &default_target,
            &features,
            &repository_id,
            &archive_storage,
        ],
    )?;

    let release_id: i32 = rows[0].get(0);

    add_keywords_into_database(conn, &metadata_pkg, release_id)?;
    add_compression_into_database(conn, compression_algorithms.into_iter(), release_id)?;

    // Update the crates table with the new release
    conn.execute(
        "UPDATE crates
         SET latest_version_id = $2
         WHERE id = $1",
        &[&crate_id, &release_id],
    )?;

    Ok(release_id)
}

pub(crate) fn add_doc_coverage(
    conn: &mut Client,
    release_id: i32,
    doc_coverage: DocCoverage,
) -> Result<i32> {
    debug!("Adding doc coverage into database");
    let rows = conn.query(
        "INSERT INTO doc_coverage (
            release_id, total_items, documented_items,
            total_items_needing_examples, items_with_examples
        )
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (release_id) DO UPDATE
                SET
                    total_items = $2,
                    documented_items = $3,
                    total_items_needing_examples = $4,
                    items_with_examples = $5
            RETURNING release_id",
        &[
            &release_id,
            &doc_coverage.total_items,
            &doc_coverage.documented_items,
            &doc_coverage.total_items_needing_examples,
            &doc_coverage.items_with_examples,
        ],
    )?;
    Ok(rows[0].get(0))
}

/// Adds a build into database
pub(crate) fn add_build_into_database(
    conn: &mut Client,
    release_id: i32,
    res: &BuildResult,
) -> Result<i32> {
    debug!("Adding build into database");
    let rows = conn.query(
        "INSERT INTO builds (rid, rustc_version, docsrs_version, build_status)
        VALUES ($1, $2, $3, $4)
        RETURNING id",
        &[
            &release_id,
            &res.rustc_version,
            &res.docsrs_version,
            &res.successful,
        ],
    )?;
    Ok(rows[0].get(0))
}

fn initialize_package_in_database(conn: &mut Client, pkg: &MetadataPackage) -> Result<i32> {
    let mut rows = conn.query("SELECT id FROM crates WHERE name = $1", &[&pkg.name])?;
    // insert crate into database if it is not exists
    if rows.is_empty() {
        rows = conn.query(
            "INSERT INTO crates (name) VALUES ($1) RETURNING id",
            &[&pkg.name],
        )?;
    }
    Ok(rows[0].get(0))
}

/// Convert dependencies into Vec<(String, String, String)>
fn convert_dependencies(pkg: &MetadataPackage) -> Vec<(String, String, String)> {
    pkg.dependencies
        .iter()
        .map(|dependency| {
            let name = dependency.name.clone();
            let version = dependency.req.clone();
            let kind = dependency
                .kind
                .clone()
                .unwrap_or_else(|| "normal".to_string());

            (name, version, kind)
        })
        .collect()
}

/// Reads features and converts them to Vec<Feature> with default being first
fn get_features(pkg: &MetadataPackage) -> Vec<Feature> {
    let mut features = Vec::with_capacity(pkg.features.len());
    if let Some(subfeatures) = pkg.features.get("default") {
        features.push(Feature::new("default".into(), subfeatures.clone(), false));
    };
    features.extend(
        pkg.features
            .iter()
            .filter(|(name, _)| *name != "default")
            .map(|(name, subfeatures)| Feature::new(name.clone(), subfeatures.clone(), false)),
    );
    features.extend(get_optional_dependencies(pkg));
    features
}

fn get_optional_dependencies(pkg: &MetadataPackage) -> Vec<Feature> {
    pkg.dependencies
        .iter()
        .filter(|dep| dep.optional)
        .map(|dep| {
            let name = if let Some(rename) = &dep.rename {
                rename.clone()
            } else {
                dep.name.clone()
            };
            Feature::new(name, Vec::new(), true)
        })
        .collect()
}

/// Reads readme if there is any read defined in Cargo.toml of a Package
fn get_readme(pkg: &MetadataPackage, source_dir: &Path) -> Result<Option<String>> {
    let readme_path = source_dir.join(pkg.readme.as_deref().unwrap_or("README.md"));

    if !readme_path.exists() {
        return Ok(None);
    }

    let readme = fs::read_to_string(readme_path)?;

    if readme.is_empty() {
        Ok(None)
    } else if readme.len() > 51200 {
        Ok(Some(format!(
            "(Readme ignored due to being too long. ({} > 51200))",
            readme.len()
        )))
    } else {
        Ok(Some(readme))
    }
}

fn get_rustdoc(pkg: &MetadataPackage, source_dir: &Path) -> Result<Option<String>> {
    if let Some(src_path) = &pkg.targets[0].src_path {
        let src_path = Path::new(src_path);
        if src_path.is_absolute() {
            read_rust_doc(src_path)
        } else {
            read_rust_doc(&source_dir.join(src_path))
        }
    } else {
        // FIXME: should we care about metabuild targets?
        Ok(None)
    }
}

/// Reads rustdoc from library
fn read_rust_doc(file_path: &Path) -> Result<Option<String>> {
    let reader = fs::File::open(file_path).map(BufReader::new)?;
    let mut rustdoc = String::new();

    for line in reader.lines() {
        let line = line?;
        if line.starts_with("//!") {
            // some lines may or may not have a space between the `//!` and the start of the text
            let line = line.trim_start_matches("//!").trim_start();
            if !line.is_empty() {
                rustdoc.push_str(line);
            }
            rustdoc.push('\n');
        }
    }

    if rustdoc.is_empty() {
        Ok(None)
    } else if rustdoc.len() > 51200 {
        Ok(Some(format!(
            "(Library doc comment ignored due to being too long. ({} > 51200))",
            rustdoc.len()
        )))
    } else {
        Ok(Some(rustdoc))
    }
}

/// Adds keywords into database
fn add_keywords_into_database(
    conn: &mut Client,
    pkg: &MetadataPackage,
    release_id: i32,
) -> Result<()> {
    for keyword in &pkg.keywords {
        let slug = slugify(&keyword);
        let keyword_id: i32 = {
            let rows = conn.query("SELECT id FROM keywords WHERE slug = $1", &[&slug])?;
            if !rows.is_empty() {
                rows[0].get(0)
            } else {
                conn.query(
                    "INSERT INTO keywords (name, slug) VALUES ($1, $2) RETURNING id",
                    &[&keyword, &slug],
                )?[0]
                    .get(0)
            }
        };

        // add releationship
        let _ = conn.query(
            "INSERT INTO keyword_rels (rid, kid) VALUES ($1, $2)",
            &[&release_id, &keyword_id],
        );
    }

    Ok(())
}

pub fn update_crate_data_in_database(
    conn: &mut Client,
    name: &str,
    registry_data: &CrateData,
) -> Result<()> {
    info!("Updating crate data for {}", name);
    let crate_id = conn.query("SELECT id FROM crates WHERE crates.name = $1", &[&name])?[0].get(0);

    update_owners_in_database(conn, &registry_data.owners, crate_id)?;

    Ok(())
}

/// Adds owners into database
fn update_owners_in_database(
    conn: &mut Client,
    owners: &[CrateOwner],
    crate_id: i32,
) -> Result<()> {
    let rows = conn.query(
        "
            SELECT login
            FROM owners
            INNER JOIN owner_rels
                ON owner_rels.oid = owners.id
            WHERE owner_rels.cid = $1
        ",
        &[&crate_id],
    )?;
    let existing_owners = rows.into_iter().map(|row| -> String { row.get(0) });

    for owner in owners {
        debug!("Updating owner data for {}: {:?}", owner.login, owner);

        // Update any existing owner data since it is mutable and could have changed since last
        // time we pulled it
        let owner_id: i32 = {
            conn.query(
                "
                    INSERT INTO owners (login, avatar, name, email)
                    VALUES ($1, $2, $3, $4)
                    ON CONFLICT (login) DO UPDATE
                        SET
                            avatar = $2,
                            name = $3,
                            email = $4
                    RETURNING id
                ",
                &[&owner.login, &owner.avatar, &owner.name, &owner.email],
            )?[0]
                .get(0)
        };

        // add relationship
        conn.query(
            "INSERT INTO owner_rels (cid, oid) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&crate_id, &owner_id],
        )?;
    }

    let to_remove =
        existing_owners.filter(|login| !owners.iter().any(|owner| &owner.login == login));

    for login in to_remove {
        debug!("Removing owner relationship {}", login);
        // remove relationship
        conn.query(
            "
                DELETE FROM owner_rels
                USING owners
                WHERE owner_rels.cid = $1
                    AND owner_rels.oid = owners.id
                    AND owners.login = $2
            ",
            &[&crate_id, &login],
        )?;
    }

    Ok(())
}

/// Add the compression algorithms used for this crate to the database
fn add_compression_into_database<I>(conn: &mut Client, algorithms: I, release_id: i32) -> Result<()>
where
    I: Iterator<Item = CompressionAlgorithm>,
{
    let sql = "
    INSERT INTO compression_rels (release, algorithm)
    VALUES ($1, $2)
    ON CONFLICT DO NOTHING;";
    let prepared = conn.prepare(sql)?;
    for alg in algorithms {
        conn.query(&prepared, &[&release_id, &(alg as i32)])?;
    }
    Ok(())
}
