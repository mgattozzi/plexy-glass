//! Pure formatting for the stamped `--version` string, shared by `build.rs`
//! (which supplies the git SHA + commit date) and its unit test.

/// `<pkg>-nightly (<sha> <date>)` in a git checkout, or the bare `<pkg>` when
/// the SHA/date are unavailable (a source tarball or a crates.io build). A
/// partial pair (SHA but no date, or vice versa) also falls back — never a
/// half-stamped string.
#[must_use] // pure formatter — its result is always meant to be used
pub fn format_version(pkg: &str, sha: Option<&str>, date: Option<&str>) -> String {
    match (sha, date) {
        (Some(sha), Some(date)) => format!("{pkg}-nightly ({sha} {date})"),
        _ => pkg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_sha_and_date_in_a_git_checkout() {
        assert_eq!(
            format_version("0.1.0", Some("abc1234"), Some("2026-07-08")),
            "0.1.0-nightly (abc1234 2026-07-08)"
        );
    }

    #[test]
    fn falls_back_to_bare_version_without_git() {
        assert_eq!(format_version("0.1.0", None, None), "0.1.0");
        assert_eq!(format_version("0.1.0", Some("abc1234"), None), "0.1.0");
        assert_eq!(format_version("0.1.0", None, Some("2026-07-08")), "0.1.0");
    }
}
