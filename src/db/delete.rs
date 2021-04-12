use crate::Storage;
use failure::{Error, Fail};
use postgres::Client;

/// List of directories in docs.rs's underlying storage (either the database or S3) containing a
/// subdirectory named after the crate. Those subdirectories will be deleted.
static STORAGE_PATHS_TO_DELETE: &[&str] = &["rustdoc", "sources"];

#[derive(Debug, Fail)]
enum CrateDeletionError {
    #[fail(display = "crate is missing: {}", _0)]
    MissingCrate(String),
}

pub fn delete_crate(conn: &mut Client, storage: &Storage, name: &str) -> Result<(), Error> {
    let crate_id = get_id(conn, name)?;
    delete_crate_from_database(conn, name, crate_id)?;

    for prefix in STORAGE_PATHS_TO_DELETE {
        storage.delete_prefix(&format!("{}/{}/", prefix, name))?;
    }

    Ok(())
}

pub fn delete_version(
    conn: &mut Client,
    storage: &Storage,
    name: &str,
    version: &str,
) -> Result<(), Error> {
    delete_version_from_database(conn, name, version)?;

    for prefix in STORAGE_PATHS_TO_DELETE {
        storage.delete_prefix(&format!("{}/{}/{}/", prefix, name, version))?;
    }

    Ok(())
}

fn get_id(conn: &mut Client, name: &str) -> Result<i32, Error> {
    let crate_id_res = conn.query("SELECT id FROM crates WHERE name = $1", &[&name])?;
    if let Some(row) = crate_id_res.into_iter().next() {
        Ok(row.get("id"))
    } else {
        Err(CrateDeletionError::MissingCrate(name.into()).into())
    }
}

// metaprogramming!
// WARNING: these must be hard-coded and NEVER user input.
const METADATA: &[(&str, &str)] = &[
    ("keyword_rels", "rid"),
    ("builds", "rid"),
    ("compression_rels", "release"),
    ("doc_coverage", "release_id"),
];

fn delete_version_from_database(conn: &mut Client, name: &str, version: &str) -> Result<(), Error> {
    let crate_id = get_id(conn, name)?;
    let mut transaction = conn.transaction()?;
    for &(table, column) in METADATA {
        transaction.execute(
            format!("DELETE FROM {} WHERE {} IN (SELECT id FROM releases WHERE crate_id = $1 AND version = $2)", table, column).as_str(),
            &[&crate_id, &version],
        )?;
    }
    transaction.execute(
        "DELETE FROM releases WHERE crate_id = $1 AND version = $2",
        &[&crate_id, &version],
    )?;
    transaction.execute(
        "UPDATE crates SET latest_version_id = (
            SELECT id FROM releases WHERE release_time = (
                SELECT MAX(release_time) FROM releases WHERE crate_id = $1
            )
        ) WHERE id = $1",
        &[&crate_id],
    )?;

    for prefix in STORAGE_PATHS_TO_DELETE {
        transaction.execute(
            "DELETE FROM files WHERE path LIKE $1;",
            &[&format!("{}/{}/{}/%", prefix, name, version)],
        )?;
    }

    transaction.commit().map_err(Into::into)
}

fn delete_crate_from_database(conn: &mut Client, name: &str, crate_id: i32) -> Result<(), Error> {
    let mut transaction = conn.transaction()?;

    transaction.execute(
        "DELETE FROM sandbox_overrides WHERE crate_name = $1",
        &[&name],
    )?;
    for &(table, column) in METADATA {
        transaction.execute(
            format!(
                "DELETE FROM {} WHERE {} IN (SELECT id FROM releases WHERE crate_id = $1)",
                table, column
            )
            .as_str(),
            &[&crate_id],
        )?;
    }
    transaction.execute("DELETE FROM owner_rels WHERE cid = $1;", &[&crate_id])?;
    transaction.execute("DELETE FROM releases WHERE crate_id = $1;", &[&crate_id])?;
    transaction.execute("DELETE FROM crates WHERE id = $1;", &[&crate_id])?;

    // Transactions automatically rollback when not committing, so if any of the previous queries
    // fail the whole transaction will be aborted.
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::api::CrateOwner;
    use crate::test::{assert_success, wrapper};
    use failure::Error;
    use postgres::Client;

    fn crate_exists(conn: &mut Client, name: &str) -> Result<bool, Error> {
        Ok(!conn
            .query("SELECT * FROM crates WHERE name = $1;", &[&name])?
            .is_empty())
    }

    fn release_exists(conn: &mut Client, id: i32) -> Result<bool, Error> {
        Ok(!conn
            .query("SELECT * FROM releases WHERE id = $1;", &[&id])?
            .is_empty())
    }

    #[test]
    fn test_delete_from_database() {
        wrapper(|env| {
            let db = env.db();

            // Create fake packages in the database
            let pkg1_v1_id = env
                .fake_release()
                .name("package-1")
                .version("1.0.0")
                .create()?;
            let pkg1_v2_id = env
                .fake_release()
                .name("package-1")
                .version("2.0.0")
                .create()?;
            let pkg2_id = env.fake_release().name("package-2").create()?;

            assert!(crate_exists(&mut db.conn(), "package-1")?);
            assert!(crate_exists(&mut db.conn(), "package-2")?);
            assert!(release_exists(&mut db.conn(), pkg1_v1_id)?);
            assert!(release_exists(&mut db.conn(), pkg1_v2_id)?);
            assert!(release_exists(&mut db.conn(), pkg2_id)?);

            let pkg1_id = &db
                .conn()
                .query("SELECT id FROM crates WHERE name = 'package-1';", &[])?[0]
                .get("id");

            delete_crate_from_database(&mut db.conn(), "package-1", *pkg1_id)?;

            assert!(!crate_exists(&mut db.conn(), "package-1")?);
            assert!(crate_exists(&mut db.conn(), "package-2")?);
            assert!(!release_exists(&mut db.conn(), pkg1_v1_id)?);
            assert!(!release_exists(&mut db.conn(), pkg1_v2_id)?);
            assert!(release_exists(&mut db.conn(), pkg2_id)?);

            Ok(())
        });
    }

    #[test]
    fn test_delete_version() {
        wrapper(|env| {
            fn owners(conn: &mut Client, crate_id: i32) -> Result<Vec<String>, Error> {
                Ok(conn
                    .query(
                        "SELECT name FROM owners
                        INNER JOIN owner_rels ON owners.id = owner_rels.oid
                        WHERE owner_rels.cid = $1",
                        &[&crate_id],
                    )?
                    .into_iter()
                    .map(|row| row.get(0))
                    .collect())
            }

            let db = env.db();
            let v1 = env
                .fake_release()
                .name("a")
                .version("1.0.0")
                .add_owner(CrateOwner {
                    login: "malicious actor".into(),
                    avatar: "https://example.org/malicious".into(),
                    name: "malicious actor".into(),
                    email: "malicious@example.org".into(),
                })
                .create()?;
            assert!(release_exists(&mut db.conn(), v1)?);
            let crate_id = db
                .conn()
                .query("SELECT crate_id FROM releases WHERE id = $1", &[&v1])?
                .into_iter()
                .next()
                .unwrap()
                .get(0);
            assert_eq!(
                owners(&mut db.conn(), crate_id)?,
                vec!["malicious actor".to_string()]
            );

            let v2 = env
                .fake_release()
                .name("a")
                .version("2.0.0")
                .add_owner(CrateOwner {
                    login: "Peter Rabbit".into(),
                    avatar: "https://example.org/peter".into(),
                    name: "Peter Rabbit".into(),
                    email: "peter@example.org".into(),
                })
                .create()?;
            assert!(release_exists(&mut db.conn(), v2)?);
            assert_eq!(
                owners(&mut db.conn(), crate_id)?,
                vec!["Peter Rabbit".to_string()]
            );

            delete_version(&mut db.conn(), &*env.storage(), "a", "1.0.0")?;
            assert!(!release_exists(&mut db.conn(), v1)?);
            assert!(release_exists(&mut db.conn(), v2)?);
            assert_eq!(
                owners(&mut db.conn(), crate_id)?,
                vec!["Peter Rabbit".to_string()]
            );

            let web = env.frontend();
            assert_success("/a/2.0.0/a/", web)?;
            assert_eq!(web.get("/a/1.0.0/a/").send()?.status(), 404);

            Ok(())
        })
    }
}
