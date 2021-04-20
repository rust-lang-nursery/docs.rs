use crate::error::Result;
use crate::repositories::{GitHub, GitLab, RateLimitReached};
use crate::utils::MetadataPackage;
use crate::{db::Pool, Config};
use chrono::{DateTime, Utc};
use log::{debug, info, trace, warn};
use once_cell::sync::Lazy;
use postgres::Client;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fmt;

pub trait RepositoryForge {
    /// Result used both as the `host` column in the DB and to match repository URLs during
    /// backfill.
    fn host(&self) -> &'static str;

    /// FontAwesome icon used in the front-end.
    fn icon(&self) -> &'static str;

    /// How many items we can query in one graphql request.
    fn chunk_size(&self) -> usize;

    /// Used by both backfill_repositories and load_repository. When the repository is missing
    /// `None` is returned.
    fn fetch_repository(&self, name: &RepositoryName) -> Result<Option<Repository>>;

    /// Used by update_all_crates.
    ///
    /// The returned struct will contain all the information needed for `RepositoriesUpdater` to
    /// update repositories that are still present and delete the missing ones.
    fn fetch_repositories(&self, ids: &[String]) -> Result<FetchRepositoriesResult>;
}

#[derive(Debug)]
pub struct Repository {
    pub id: String,
    pub name_with_owner: String,
    pub description: Option<String>,
    pub last_activity_at: Option<DateTime<Utc>>,
    pub stars: i64,
    pub forks: i64,
    pub issues: i64,
}

#[derive(Default, Debug)]
pub struct FetchRepositoriesResult {
    pub present: HashMap<String, Repository>,
    pub missing: Vec<String>,
}

pub struct RepositoryStatsUpdater {
    updaters: Vec<Box<dyn RepositoryForge + Send + Sync>>,
    pool: Pool,
}

impl fmt::Debug for RepositoryStatsUpdater {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepositoryStatsUpdater {{ updaters: ")?;
        f.debug_list()
            .entries(self.updaters.iter().map(|u| u.host()))
            .finish()?;
        write!(f, " }}")
    }
}

impl RepositoryStatsUpdater {
    pub fn new(config: &Config, pool: Pool) -> Self {
        let mut updaters: Vec<Box<dyn RepositoryForge + Send + Sync>> = Vec::new();
        if let Ok(Some(updater)) = GitHub::new(&config) {
            updaters.push(Box::new(updater));
        }
        if let Ok(updater) = GitLab::new("gitlab.com", &config.gitlab_accesstoken) {
            updaters.push(Box::new(updater));
        }
        if let Ok(updater) = GitLab::new("gitlab.freedesktop.org", &None) {
            updaters.push(Box::new(updater));
        }
        Self { updaters, pool }
    }

    pub(crate) fn load_repository(&self, metadata: &MetadataPackage) -> Result<Option<i32>> {
        let url = match &metadata.repository {
            Some(url) => url,
            None => {
                debug!("did not collect stats as no repository URL was present");
                return Ok(None);
            }
        };
        let mut conn = self.pool.get()?;
        self.load_repository_inner(&mut conn, url)
    }

    fn load_repository_inner(&self, conn: &mut Client, url: &str) -> Result<Option<i32>> {
        let name = match repository_name(url) {
            Some(name) => name,
            None => return Ok(None),
        };

        // Avoid querying the APIs for repositories we already loaded.
        if let Some(row) = conn.query_opt(
            "SELECT id FROM repositories WHERE name = $1 AND host = $2 LIMIT 1;",
            &[&format!("{}/{}", name.owner, name.repo), &name.host],
        )? {
            return Ok(Some(row.get("id")));
        }
        if let Some(updater) = self.updaters.iter().find(|u| u.host() == name.host) {
            let res = match updater.fetch_repository(&name) {
                Ok(Some(repo)) => self.store_repository(conn, updater.host(), repo),
                Ok(None) => {
                    warn!(
                        "failed to fetch repository `{}` on host `{}`",
                        url,
                        updater.host()
                    );
                    return Ok(None);
                }
                Err(err) => Err(err),
            };
            return match res {
                Ok(repo_id) => Ok(Some(repo_id)),
                Err(err) => failure::bail!("failed to collect `{}` stats: {}", updater.host(), err),
            };
        }
        // It means that none of our updaters have a matching host.
        Ok(None)
    }

    pub fn update_all_crates(&self) -> Result<()> {
        let mut conn = self.pool.get()?;
        'updaters: for updater in &self.updaters {
            info!("started updating `{}` repositories stats", updater.host());

            let needs_update = conn
                .query(
                    "SELECT host_id
                     FROM repositories
                     WHERE host = $1 AND updated_at < NOW() - INTERVAL '1 day';",
                    &[&updater.host()],
                )?
                .into_iter()
                .map(|row| row.get(0))
                .collect::<Vec<String>>();

            if needs_update.is_empty() {
                info!(
                    "no `{}` repositories stats needed to be updated",
                    updater.host()
                );
                continue;
            }
            // FIXME: The collect can be avoided if we use Itertools::chunks:
            // https://docs.rs/itertools/0.10.0/itertools/trait.Itertools.html#method.chunks.
            for chunk in needs_update.chunks(updater.chunk_size()) {
                let res = match updater.fetch_repositories(chunk) {
                    Ok(r) => r,
                    Err(err) => {
                        if err.downcast_ref::<RateLimitReached>().is_some() {
                            warn!(
                                "rate limit reached, skipping the `{}` repository stats updater",
                                updater.host()
                            );
                            continue 'updaters;
                        }
                        return Err(err);
                    }
                };
                for node in res.missing {
                    self.delete_repository(&mut conn, &node, updater.host())?;
                }
                for (_, repo) in res.present {
                    self.store_repository(&mut conn, updater.host(), repo)?;
                }
            }
            info!("finished updating `{}` repositories stats", updater.host());
        }
        Ok(())
    }

    pub fn backfill_repositories(&self) -> Result<()> {
        let mut conn = self.pool.get()?;
        for updater in &self.updaters {
            info!(
                "started backfilling `{}` repositories stats",
                updater.host()
            );

            let needs_backfilling = conn.query(
                "SELECT releases.id, crates.name, releases.version, releases.repository_url
                 FROM releases
                 INNER JOIN crates ON (crates.id = releases.crate_id)
                 WHERE repository_id IS NULL AND repository_url LIKE $1;",
                &[&format!("%{}%", updater.host())],
            )?;

            let mut missing_urls = HashSet::new();
            for row in &needs_backfilling {
                let id: i32 = row.get("id");
                let name: String = row.get("name");
                let version: String = row.get("version");
                let url: String = row.get("repository_url");

                if missing_urls.contains(&url) {
                    debug!("{} {} points to a known missing repo", name, version);
                } else if let Some(node_id) = self.load_repository_inner(&mut conn, &url)? {
                    conn.execute(
                        "UPDATE releases SET repository_id = $1 WHERE id = $2;",
                        &[&node_id, &id],
                    )?;
                    info!(
                        "backfilled `{}` repositories for {} {}",
                        updater.host(),
                        name,
                        version,
                    );
                } else {
                    debug!(
                        "{} {} does not point to a {} repository",
                        name,
                        version,
                        updater.host(),
                    );
                    missing_urls.insert(url);
                }
            }
        }

        Ok(())
    }

    pub fn get_icon_name(&self, host: &str) -> &'static str {
        for updater in &self.updaters {
            if updater.host() == host {
                return updater.icon();
            }
        }
        // The default icon in case it doesn't match any of the "known" ones.
        "code-branch"
    }

    fn store_repository(&self, conn: &mut Client, host: &str, repo: Repository) -> Result<i32> {
        trace!(
            "storing {} repository stats for {}",
            host,
            repo.name_with_owner,
        );
        let data = conn.query_one(
            "INSERT INTO repositories (
                 host, host_id, name, description, last_commit, stars, forks, issues, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
             ON CONFLICT (host, host_id) DO
             UPDATE SET
                 name = $3,
                 description = $4,
                 last_commit = $5,
                 stars = $6,
                 forks = $7,
                 issues = $8,
                 updated_at = NOW()
             RETURNING id;",
            &[
                &host,
                &repo.id,
                &repo.name_with_owner,
                &repo.description,
                &repo.last_activity_at,
                &(repo.stars as i32),
                &(repo.forks as i32),
                &(repo.issues as i32),
            ],
        )?;
        Ok(data.get(0))
    }

    fn delete_repository(&self, conn: &mut Client, host_id: &str, host: &str) -> Result<()> {
        trace!(
            "removing repository stats for host ID `{}` and host `{}`",
            host_id,
            host
        );
        conn.execute(
            "DELETE FROM repositories WHERE host_id = $1 AND host = $2;",
            &[&host_id, &host],
        )?;
        Ok(())
    }
}

pub(crate) fn repository_name(url: &str) -> Option<RepositoryName> {
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"https?://(?P<host>[^/]+)/(?P<owner>[\w\._/-]+)/(?P<repo>[\w\._-]+)").unwrap()
    });

    let cap = RE.captures(url)?;
    let host = cap.name("host").expect("missing group 'host'").as_str();
    let owner = cap.name("owner").expect("missing group 'owner'").as_str();
    let repo = cap.name("repo").expect("missing group 'repo'").as_str();
    Some(RepositoryName {
        owner,
        repo: repo.strip_suffix(".git").unwrap_or(repo),
        host,
    })
}

#[derive(Debug, Eq, PartialEq)]
pub struct RepositoryName<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    pub host: &'a str,
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::context::Context;

    #[test]
    fn test_repository_name() {
        fn assert_name<'a, T: Into<Option<(&'a str, &'a str, &'a str)>>>(url: &str, data: T) {
            let data = data.into();
            assert_eq!(
                repository_name(url),
                data.map(|(owner, repo, host)| RepositoryName { owner, repo, host }),
            );
        }

        // gitlab checks
        assert_name(
            "https://gitlab.com/pythondude325/hexponent",
            ("pythondude325", "hexponent", "gitlab.com"),
        );
        assert_name(
            "http://gitlab.com/pythondude325/hexponent",
            ("pythondude325", "hexponent", "gitlab.com"),
        );
        assert_name(
            "https://gitlab.com/pythondude325/hexponent.git",
            ("pythondude325", "hexponent", "gitlab.com"),
        );
        assert_name(
            "https://gitlab.com/docopt/docopt.rs",
            ("docopt", "docopt.rs", "gitlab.com"),
        );
        assert_name(
            "https://gitlab.com/onur23cmD_M_R_L_/crates_fy-i",
            ("onur23cmD_M_R_L_", "crates_fy-i", "gitlab.com"),
        );
        assert_name(
            "https://gitlab.freedesktop.org/test1/test2",
            ("test1", "test2", "gitlab.freedesktop.org"),
        );
        assert_name(
            "https://gitlab.com/artgam3s/public-libraries/rust/rpa",
            ("artgam3s/public-libraries/rust", "rpa", "gitlab.com"),
        );

        assert_name("https://gitlab.com/moi/", None);
        assert_name("https://gitlab.com/moi", None);
        assert_name("https://gitlab.com", None);
        assert_name("https://gitlab.com/", None);

        // github checks
        assert_name(
            "https://github.com/rust-lang/rust",
            ("rust-lang", "rust", "github.com"),
        );
        assert_name(
            "http://github.com/rust-lang/rust",
            ("rust-lang", "rust", "github.com"),
        );
        assert_name(
            "https://github.com/rust-lang/rust.git",
            ("rust-lang", "rust", "github.com"),
        );
        assert_name(
            "https://github.com/docopt/docopt.rs",
            ("docopt", "docopt.rs", "github.com"),
        );
        assert_name(
            "https://github.com/onur23cmD_M_R_L_/crates_fy-i",
            ("onur23cmD_M_R_L_", "crates_fy-i", "github.com"),
        );

        assert_name("https://github.com/moi/", None);
        assert_name("https://github.com/moi", None);
        assert_name("https://github.com", None);
        assert_name("https://github.com/", None);

        // Unknown host
        assert_name("https://git.sr.ht/~ireas/merge-rs", None);
    }

    #[test]
    fn test_icon_name() {
        crate::test::wrapper(|env| {
            let mut config = env.base_config();
            config.github_accesstoken = Some("qsjdnfqdq".to_owned());
            let updater = RepositoryStatsUpdater::new(&config, env.pool()?);

            assert_eq!(updater.get_icon_name(""), "code-branch");
            assert_eq!(updater.get_icon_name("random"), "code-branch");
            assert_eq!(updater.get_icon_name("github"), "code-branch");
            assert_eq!(updater.get_icon_name("github.com"), "github");
            assert_eq!(updater.get_icon_name("gitlab"), "code-branch");
            assert_eq!(updater.get_icon_name("gitlab.com"), "gitlab");
            assert_eq!(updater.get_icon_name("gitlab.freedesktop.org"), "gitlab");
            assert_eq!(
                updater.get_icon_name("a.gitlab.freedesktop.org"),
                "code-branch"
            );
            Ok(())
        });
    }
}
