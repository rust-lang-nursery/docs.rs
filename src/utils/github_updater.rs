
use ::db::connect_db;
use regex::Regex;
use DocBuilderError;
use time;



/// Fields we need use in cratesfyi
#[derive(Debug)]
struct GitHubFields {
    pub description: String,
    pub stars: i64,
    pub forks: i64,
    pub issues: i64,
    pub last_commit: time::Timespec,
}


/// Updates github fields in crates table
pub fn github_updater() -> Result<(), DocBuilderError> {
    let conn = try!(connect_db());

    // TODO: This query assumes repository field in Cargo.toml is
    //       always the same across all versions of a crate
    for row in &try!(conn.query("SELECT DISTINCT ON (crates.name) \
                                        crates.name, \
                                        crates.id, \
                                        releases.repository_url \
                                 FROM crates \
                                 INNER JOIN releases ON releases.crate_id = crates.id \
                                 WHERE releases.repository_url ~ '^https*://github.com' AND \
                                       (crates.github_last_update < NOW() - INTERVAL '1 day' OR \
                                        crates.github_last_update IS NULL)",
                                &[])) {
        let crate_name: String = row.get(0);
        let crate_id: i32 = row.get(1);
        let repository_url: String = row.get(2);

        if let Err(err) = get_github_path(&repository_url[..])
                              .ok_or(DocBuilderError::GenericError("Failed to get github path"
                                                                       .to_string()))
                              .and_then(|path| get_github_fields(&path[..]))
                              .and_then(|fields| {
                                  conn.execute("UPDATE crates SET github_description = $1, \
                                                github_stars = $2, github_forks = $3, \
                                                github_issues = $4, github_last_commit = $5, \
                                                github_last_update = NOW() WHERE id = $6",
                                               &[&fields.description,
                                                 &(fields.stars as i32),
                                                 &(fields.forks as i32),
                                                 &(fields.issues as i32),
                                                 &(fields.last_commit),
                                                 &crate_id])
                                      .map_err(DocBuilderError::DatabaseError)
                              }) {
            debug!("Failed to update github fields of: {} {}", crate_name, err);
        }

        // sleep for rate limits
        use std::thread;
        use std::time::Duration;
        thread::sleep(Duration::from_secs(2));
    }

    Ok(())
}


fn get_github_fields(path: &str) -> Result<GitHubFields, DocBuilderError> {
    use rustc_serialize::json::Json;

    let body = {
        use std::io::Read;
        use hyper::client::Client;
        use hyper::header::{UserAgent, Authorization, Basic};
        use hyper::status::StatusCode;
        use std::env;

        let client = Client::new();
        let mut body = String::new();

        let mut resp = try!(client.get(&format!("https://api.github.com/repos/{}", path)[..])
                                  .header(UserAgent(format!("cratesfyi/{}",
                                                            env!("CARGO_PKG_VERSION"))))
                                  .header(Authorization(Basic {
                                      username: env::var("CRATESFYI_GITHUB_USERNAME")
                                                    .ok()
                                                    .and_then(|u| Some(u.to_string()))
                                                    .unwrap_or("".to_string()),
                                      password: env::var("CRATESFYI_GITHUB_ACCESSTOKEN").ok(),
                                  }))
                                  .send());

        if resp.status != StatusCode::Ok {
            return Err(DocBuilderError::GenericError("Failed to get github data".to_string()));
        }

        try!(resp.read_to_string(&mut body));
        body
    };

    let json = try!(Json::from_str(&body[..]));
    let obj = json.as_object().unwrap();

    Ok(GitHubFields {
        description: obj.get("description").and_then(|d| d.as_string()).unwrap_or("").to_string(),
        stars: obj.get("stargazers_count").and_then(|d| d.as_i64()).unwrap_or(0),
        forks: obj.get("forks_count").and_then(|d| d.as_i64()).unwrap_or(0),
        issues: obj.get("open_issues").and_then(|d| d.as_i64()).unwrap_or(0),
        last_commit: time::strptime(obj.get("pushed_at")
                                       .and_then(|d| d.as_string())
                                       .unwrap_or(""),
                                    "%Y-%m-%dT%H:%M:%S")
                         .unwrap_or(time::now())
                         .to_timespec(),
    })
}



fn get_github_path(url: &str) -> Option<String> {
    let re = Regex::new(r"https?://github\.com/([\w\._-]+)/([\w\._-]+)").unwrap();
    match re.captures(url) {
        Some(cap) => {
            let username = cap.at(1).unwrap();
            let reponame = cap.at(2).unwrap();
            Some(format!("{}/{}", username, if reponame.ends_with(".git") {
                reponame.split(".git").nth(0).unwrap()
            } else {
                reponame
            }))
        },
        None => None,
    }
}



#[cfg(test)]
mod test {
    extern crate env_logger;
    use super::{get_github_path, get_github_fields, github_updater};

    #[test]
    fn test_get_github_path() {
        assert_eq!(get_github_path("https://github.com/onur/cratesfyi"),
                   Some("onur/cratesfyi".to_string()));
        assert_eq!(get_github_path("http://github.com/onur/cratesfyi"),
                   Some("onur/cratesfyi".to_string()));
        assert_eq!(get_github_path("https://github.com/onur/cratesfyi.git"),
                   Some("onur/cratesfyi".to_string()));
        assert_eq!(get_github_path("https://github.com/onur23cmD_M_R_L_/crates_fy-i"),
                   Some("onur23cmD_M_R_L_/crates_fy-i".to_string()));
        assert_eq!(get_github_path("https://github.com/docopt/docopt.rs"),
                   Some("docopt/docopt.rs".to_string()));
    }


    #[test]
    #[ignore]
    fn test_get_github_fields() {
        let _ = env_logger::init();
        let fields = get_github_fields("onur/cratesfyi");
        assert!(fields.is_ok());

        let fields = fields.unwrap();
        assert!(fields.description != "".to_string());
        assert!(fields.stars >= 0);
        assert!(fields.forks >= 0);
        assert!(fields.issues >= 0);

        use time;
        assert!(fields.last_commit <= time::now().to_timespec());
    }


    #[test]
    #[ignore]
    fn test_github_updater() {
        let _ = env_logger::init();
        assert!(github_updater().is_ok());
    }
}
