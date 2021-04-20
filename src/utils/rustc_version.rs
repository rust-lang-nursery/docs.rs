use crate::error::Result;
use failure::err_msg;
use regex::Regex;

/// Parses rustc commit hash from rustc version string
pub fn parse_rustc_version<S: AsRef<str>>(version: S) -> Result<String> {
    let version_regex = Regex::new(r" ([\w.-]+) \((\w+) (\d+)-(\d+)-(\d+)\)")?;
    let captures = version_regex
        .captures(version.as_ref())
        .ok_or_else(|| err_msg("Failed to parse rustc version"))?;

    Ok(format!(
        "{}{}{}-{}-{}",
        captures.get(3).unwrap().as_str(),
        captures.get(4).unwrap().as_str(),
        captures.get(5).unwrap().as_str(),
        captures.get(1).unwrap().as_str(),
        captures.get(2).unwrap().as_str()
    ))
}

#[test]
fn test_parse_rustc_version() {
    assert_eq!(
        parse_rustc_version("rustc 1.10.0-nightly (57ef01513 2016-05-23)").unwrap(),
        "20160523-1.10.0-nightly-57ef01513"
    );
    assert_eq!(
        parse_rustc_version("docsrs 0.2.0 (ba9ae23 2016-05-26)").unwrap(),
        "20160526-0.2.0-ba9ae23"
    );
}
