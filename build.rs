use std::env;
use std::process::Command;

// Share the pure formatter with the crate (which unit-tests it) instead of
// duplicating the format string here.
#[path = "src/version_fmt.rs"]
mod version_fmt;

fn main() {
    // Re-run when the checked-out commit changes so the stamp stays current.
    println!("cargo:rerun-if-changed=.git/HEAD");
    let pkg = env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let sha = git(&["rev-parse", "--short", "HEAD"]);
    let date = git(&["show", "-s", "--format=%cs", "HEAD"]);
    let version = version_fmt::format_version(&pkg, sha.as_deref(), date.as_deref());
    println!("cargo:rustc-env=PLEXY_GLASS_VERSION={version}");
}

/// Run a git command and return its trimmed stdout, or `None` if git is absent,
/// this isn't a repo, or the command fails — a source tarball has no `.git`, so
/// this must degrade rather than break the build.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
