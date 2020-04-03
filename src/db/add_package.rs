
use crate::utils::MetadataPackage;
use crate::docbuilder::BuildResult;
use regex::Regex;

use std::io::prelude::*;
use std::io::BufReader;
use std::path::Path;
use std::fs;

use time::Timespec;
use rustc_serialize::json::{Json, ToJson};
use slug::slugify;
use reqwest::Client;
use reqwest::header::ACCEPT;
use semver::Version;
use postgres::Connection;
use time;
use crate::error::Result;
use failure::err_msg;

/// Adds a package into database.
///
/// Package must be built first.
///
/// NOTE: `source_files` refers to the files originally in the crate,
/// not the files generated by rustdoc.
pub(crate) fn add_package_into_database(conn: &Connection,
                                 metadata_pkg: &MetadataPackage,
                                 source_dir: &Path,
                                 res: &BuildResult,
                                 default_target: &str,
                                 source_files: Option<Json>,
                                 doc_targets: Vec<String>,
                                 cratesio_data: &CratesIoData,
                                 has_docs: bool,
                                 has_examples: bool)
                                 -> Result<i32> {
    debug!("Adding package into database");
    let crate_id = initialize_package_in_database(&conn, metadata_pkg)?;
    let dependencies = convert_dependencies(metadata_pkg);
    let rustdoc = get_rustdoc(metadata_pkg, source_dir).unwrap_or(None);
    let readme = get_readme(metadata_pkg, source_dir).unwrap_or(None);
    let is_library = metadata_pkg.is_library();

    let release_id: i32 = {
        let rows = conn.query("SELECT id FROM releases WHERE crate_id = $1 AND version = $2",
                                   &[&crate_id, &metadata_pkg.version.to_string()])?;

        if rows.is_empty() {
            let rows = conn.query("INSERT INTO releases (
                                            crate_id, version, release_time,
                                            dependencies, target_name, yanked, build_status,
                                            rustdoc_status, test_status, license, repository_url,
                                            homepage_url, description, description_long, readme,
                                            authors, keywords, have_examples, downloads, files,
                                            doc_targets, is_library, doc_rustc_version,
                                            documentation_url, default_target
                                        )
                                        VALUES ( $1,  $2,  $3,  $4, $5, $6,  $7, $8, $9, $10,
                                                 $11, $12, $13, $14, $15, $16, $17, $18, $19,
                                                 $20, $21, $22, $23, $24, $25
                                        )
                                        RETURNING id",
                                       &[&crate_id,
                                         &metadata_pkg.version,
                                         &cratesio_data.release_time,
                                         &dependencies.to_json(),
                                         &metadata_pkg.package_name(),
                                         &cratesio_data.yanked,
                                         &res.successful,
                                         &has_docs,
                                         &false, // TODO: Add test status somehow
                                         &metadata_pkg.license,
                                         &metadata_pkg.repository,
                                         &metadata_pkg.homepage,
                                         &metadata_pkg.description,
                                         &rustdoc,
                                         &readme,
                                         &metadata_pkg.authors.to_json(),
                                         &metadata_pkg.keywords.to_json(),
                                         &has_examples,
                                         &cratesio_data.downloads,
                                         &source_files,
                                         &doc_targets.to_json(),
                                         &is_library,
                                         &res.rustc_version,
                                         &metadata_pkg.documentation,
                                         &default_target])?;
            // return id
            rows.get(0).get(0)

        } else {
            conn.query("UPDATE releases
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
                                 authors = $16,
                                 keywords = $17,
                                 have_examples = $18,
                                 downloads = $19,
                                 files = $20,
                                 doc_targets = $21,
                                 is_library = $22,
                                 doc_rustc_version = $23,
                                 documentation_url = $24,
                                 default_target = $25
                             WHERE crate_id = $1 AND version = $2",
                            &[&crate_id,
                              &metadata_pkg.version.to_string(),
                              &cratesio_data.release_time,
                              &dependencies.to_json(),
                              &metadata_pkg.package_name(),
                              &cratesio_data.yanked,
                              &res.successful,
                              &has_docs,
                              &false, // TODO: Add test status somehow
                              &metadata_pkg.license,
                              &metadata_pkg.repository,
                              &metadata_pkg.homepage,
                              &metadata_pkg.description,
                              &rustdoc,
                              &readme,
                              &metadata_pkg.authors.to_json(),
                              &metadata_pkg.keywords.to_json(),
                              &has_examples,
                              &cratesio_data.downloads,
                              &source_files,
                              &doc_targets.to_json(),
                              &is_library,
                              &res.rustc_version,
                              &metadata_pkg.documentation,
                              &default_target])?;
            rows.get(0).get(0)
        }
    };


    add_keywords_into_database(&conn, &metadata_pkg, &release_id)?;
    add_authors_into_database(&conn, &metadata_pkg, &release_id)?;
    add_owners_into_database(&conn, &cratesio_data.owners, &crate_id)?;


    // Update versions
    {
        let metadata_version = Version::parse(&metadata_pkg.version)?;
        let mut versions: Json = conn.query("SELECT versions FROM crates WHERE id = $1",
                                                 &[&crate_id])?
            .get(0)
            .get(0);
        if let Some(versions_array) = versions.as_array_mut() {
            let mut found = false;
            for version in versions_array.clone() {
                let version = Version::parse(version.as_string().unwrap())?;
                if version == metadata_version {
                    found = true;
                }
            }
            if !found {
                versions_array.push(metadata_pkg.version.to_string().to_json());
            }
        }
        let _ = conn.query("UPDATE crates SET versions = $1 WHERE id = $2",
                           &[&versions, &crate_id]);
    }

    Ok(release_id)
}


/// Adds a build into database
pub(crate) fn add_build_into_database(conn: &Connection,
                               release_id: &i32,
                               res: &BuildResult)
                               -> Result<i32> {
    debug!("Adding build into database");
    let rows = conn.query("INSERT INTO builds (rid, rustc_version,
                                                    cratesfyi_version,
                                                    build_status, output)
                                VALUES ($1, $2, $3, $4, $5)
                                RETURNING id",
                               &[release_id,
                                 &res.rustc_version,
                                 &res.docsrs_version,
                                 &res.successful,
                                 &res.build_log])?;
    Ok(rows.get(0).get(0))
}


fn initialize_package_in_database(conn: &Connection, pkg: &MetadataPackage) -> Result<i32> {
    let mut rows = conn.query("SELECT id FROM crates WHERE name = $1", &[&pkg.name])?;
    // insert crate into database if it is not exists
    if rows.is_empty() {
        rows = conn.query("INSERT INTO crates (name) VALUES ($1) RETURNING id",
                               &[&pkg.name])?;
    }
    Ok(rows.get(0).get(0))
}



/// Convert dependencies into Vec<(String, String, String)>
fn convert_dependencies(pkg: &MetadataPackage) -> Vec<(String, String, String)> {
    let mut dependencies: Vec<(String, String, String)> = Vec::new();
    for dependency in &pkg.dependencies {
        let name = dependency.name.clone();
        let version = dependency.req.clone();
        let kind = dependency.kind.clone().unwrap_or_else(|| "normal".into());
        dependencies.push((name, version, kind.to_string()));
    }
    dependencies
}


/// Reads readme if there is any read defined in Cargo.toml of a Package
fn get_readme(pkg: &MetadataPackage, source_dir: &Path) -> Result<Option<String>> {
    let readme_path = source_dir.join(pkg.readme.as_deref().unwrap_or("README.md"));

    if !readme_path.exists() {
        return Ok(None);
    }

    let mut reader = fs::File::open(readme_path).map(BufReader::new)?;
    let mut readme = String::new();
    reader.read_to_string(&mut readme)?;

    if readme.is_empty() {
        Ok(None)
    } else if readme.len() > 51200 {
        Ok(Some(format!("(Readme ignored due to being too long. ({} > 51200))", readme.len())))
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
        Ok(Some(format!("(Library doc comment ignored due to being too long. ({} > 51200))", rustdoc.len())))
    } else {
        Ok(Some(rustdoc))
    }
}


pub(crate) struct CratesIoData {
    pub(crate) release_time: Timespec,
    pub(crate) yanked: bool,
    pub(crate) downloads: i32,
    pub(crate) owners: Vec<CrateOwner>,
}

impl CratesIoData {
    pub(crate) fn get_from_network(pkg: &MetadataPackage) -> Result<Self> {
        let (release_time, yanked, downloads) = get_release_time_yanked_downloads(pkg)?;
        let owners = get_owners(pkg)?;

        Ok(Self {
            release_time,
            yanked,
            downloads,
            owners,
        })
    }
}


/// Get release_time, yanked and downloads from crates.io
fn get_release_time_yanked_downloads(
    pkg: &MetadataPackage,
) -> Result<(time::Timespec, bool, i32)> {
    let url = format!("https://crates.io/api/v1/crates/{}/versions", pkg.name);
    // FIXME: There is probably better way to do this
    //        and so many unwraps...
    let client = Client::new();
    let mut res = client.get(&url[..])
        .header(ACCEPT, "application/json")
        .send()?;
    let mut body = String::new();
    res.read_to_string(&mut body).unwrap();
    let json = Json::from_str(&body[..]).unwrap();
    let versions = json.as_object()
        .and_then(|o| o.get("versions"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| err_msg("Not a JSON object"))?;

    let (mut release_time, mut yanked, mut downloads) = (None, None, None);

    for version in versions {
        let version = version.as_object().ok_or_else(|| err_msg("Not a JSON object"))?;
        let version_num = version.get("num")
            .and_then(|v| v.as_string())
            .ok_or_else(|| err_msg("Not a JSON object"))?;

        if semver::Version::parse(version_num).unwrap().to_string() == pkg.version {
            let release_time_raw = version.get("created_at")
                .and_then(|c| c.as_string())
                .ok_or_else(|| err_msg("Not a JSON object"))?;
            release_time = Some(time::strptime(release_time_raw, "%Y-%m-%dT%H:%M:%S")
                .unwrap()
                .to_timespec());

            yanked = Some(version.get("yanked")
                .and_then(|c| c.as_boolean())
                .ok_or_else(|| err_msg("Not a JSON object"))?);

            downloads = Some(version.get("downloads")
                .and_then(|c| c.as_i64())
                .ok_or_else(|| err_msg("Not a JSON object"))? as i32);

            break;
        }
    }

    Ok((release_time.unwrap_or_else(time::get_time), yanked.unwrap_or(false), downloads.unwrap_or(0)))
}


/// Adds keywords into database
fn add_keywords_into_database(conn: &Connection, pkg: &MetadataPackage, release_id: &i32) -> Result<()> {
    for keyword in &pkg.keywords {
        let slug = slugify(&keyword);
        let keyword_id: i32 = {
            let rows = conn.query("SELECT id FROM keywords WHERE slug = $1", &[&slug])?;
            if !rows.is_empty() {
                rows.get(0).get(0)
            } else {
                conn.query("INSERT INTO keywords (name, slug) VALUES ($1, $2) RETURNING id",
                                &[&keyword, &slug])?
                    .get(0)
                    .get(0)
            }
        };
        // add releationship
        let _ = conn.query("INSERT INTO keyword_rels (rid, kid) VALUES ($1, $2)",
                           &[release_id, &keyword_id]);
    }

    Ok(())
}



/// Adds authors into database
fn add_authors_into_database(conn: &Connection, pkg: &MetadataPackage, release_id: &i32) -> Result<()> {

    let author_capture_re = Regex::new("^([^><]+)<*(.*?)>*$").unwrap();
    for author in &pkg.authors {
        if let Some(author_captures) = author_capture_re.captures(&author[..]) {
            let author = author_captures.get(1).map(|m| m.as_str()).unwrap_or("").trim();
            let email = author_captures.get(2).map(|m| m.as_str()).unwrap_or("").trim();
            let slug = slugify(&author);

            let author_id: i32 = {
                let rows = conn.query("SELECT id FROM authors WHERE slug = $1", &[&slug])?;
                if !rows.is_empty() {
                    rows.get(0).get(0)
                } else {
                    conn.query("INSERT INTO authors (name, email, slug) VALUES ($1, $2, $3)
                                     RETURNING id",
                                    &[&author, &email, &slug])?
                        .get(0)
                        .get(0)
                }
            };

            // add relationship
            let _ = conn.query("INSERT INTO author_rels (rid, aid) VALUES ($1, $2)",
                               &[release_id, &author_id]);
        }
    }

    Ok(())
}


pub(crate) struct CrateOwner {
    pub(crate) avatar: String,
    pub(crate) email: String,
    pub(crate) login: String,
    pub(crate) name: String,
}

/// Fetch owners from crates.io
fn get_owners(pkg: &MetadataPackage) -> Result<Vec<CrateOwner>> {
    // owners available in: https://crates.io/api/v1/crates/rand/owners
    let owners_url = format!("https://crates.io/api/v1/crates/{}/owners", pkg.name);
    let client = Client::new();
    let mut res = client.get(&owners_url[..])
        .header(ACCEPT, "application/json")
        .send()?;
    // FIXME: There is probably better way to do this
    //        and so many unwraps...
    let mut body = String::new();
    res.read_to_string(&mut body).unwrap();
    let json = Json::from_str(&body[..])?;

    let mut result = Vec::new();
    if let Some(owners) = json.as_object()
        .and_then(|j| j.get("users"))
        .and_then(|j| j.as_array()) {
        for owner in owners {
            // FIXME: I know there is a better way to do this
            let avatar = owner.as_object()
                .and_then(|o| o.get("avatar"))
                .and_then(|o| o.as_string())
                .unwrap_or("");
            let email = owner.as_object()
                .and_then(|o| o.get("email"))
                .and_then(|o| o.as_string())
                .unwrap_or("");
            let login = owner.as_object()
                .and_then(|o| o.get("login"))
                .and_then(|o| o.as_string())
                .unwrap_or("");
            let name = owner.as_object()
                .and_then(|o| o.get("name"))
                .and_then(|o| o.as_string())
                .unwrap_or("");

            if login.is_empty() {
                continue;
            }

            result.push(CrateOwner {
                avatar: avatar.to_string(),
                email: email.to_string(),
                login: login.to_string(),
                name: name.to_string(),
            });
        }
    }

    Ok(result)
}

/// Adds owners into database
fn add_owners_into_database(conn: &Connection, owners: &[CrateOwner], crate_id: &i32) -> Result<()> {
    for owner in owners {
        let owner_id: i32 = {
            let rows = conn.query("SELECT id FROM owners WHERE login = $1", &[&owner.login])?;
            if !rows.is_empty() {
                rows.get(0).get(0)
            } else {
                conn.query("INSERT INTO owners (login, avatar, name, email)
                                 VALUES ($1, $2, $3, $4)
                                 RETURNING id",
                                &[&owner.login, &owner.avatar, &owner.name, &owner.email])?
                    .get(0)
                    .get(0)
            }
        };

        // add relationship
        let _ = conn.query("INSERT INTO owner_rels (cid, oid) VALUES ($1, $2)",
                           &[crate_id, &owner_id]);
    }
    Ok(())
}
