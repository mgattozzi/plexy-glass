//! The client-side session-picker roster: the union of config `remotes`
//! (declared in `config.kdl`) and an ad-hoc client-side file recording hosts
//! the user has `-H`'d into. Pure assemble/dedup logic plus the file I/O and
//! config read that feed it; wiring into the picker itself is a later task.

#[cfg(test)]
use std::cell::RefCell;
use std::collections::HashSet;
#[cfg(not(test))]
use std::fs;
use std::io;
#[cfg(not(test))]
use std::io::Write;
#[cfg(not(test))]
use std::path::PathBuf;

#[cfg(not(test))]
use plexy_glass_daemon::RuntimePaths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterSource {
    Configured,
    AdHoc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterHost {
    pub host: String,
    pub source: RosterSource,
}

/// Assemble the roster: every distinct `configured` host (sorted, source
/// `Configured`) followed by every distinct `adhoc` host that isn't already
/// configured (sorted, source `AdHoc`).
pub fn assemble(configured: &[String], adhoc: &[String]) -> Vec<RosterHost> {
    let mut cfg: Vec<String> = configured.to_vec();
    cfg.sort();
    cfg.dedup();
    let cfgset: HashSet<&String> = cfg.iter().collect();
    let mut ad: Vec<String> = adhoc
        .iter()
        .filter(|h| !cfgset.contains(*h))
        .cloned()
        .collect();
    ad.sort();
    ad.dedup();
    cfg.into_iter()
        .map(|host| RosterHost {
            host,
            source: RosterSource::Configured,
        })
        .chain(ad.into_iter().map(|host| RosterHost {
            host,
            source: RosterSource::AdHoc,
        }))
        .collect()
}

/// The operator's LOCAL config remotes. Client-side config parse errors are
/// swallowed here (the picker still works from the ad-hoc file + this session's
/// hosts); the daemon logs its own config error separately.
#[cfg(not(test))]
pub fn config_remotes() -> Vec<String> {
    let (cfg, _err) = plexy_glass_config::load_or_default();
    cfg.remotes
}

// Under `cfg(test)` the roster sources read a per-thread override instead of
// the real config / ad-hoc files, so a pump-level test can seed a roster
// (`set_test_roster`) deterministically without touching the user's real
// `config.kdl` or `remotes` file. Defaults to empty, so tests that don't seed
// one (the existing `pump_picker_*` tests) see NO remotes and never fire a
// real query.
#[cfg(test)]
thread_local! {
    static TEST_ROSTER: RefCell<(Vec<String>, Vec<String>)> =
        const { RefCell::new((Vec::new(), Vec::new())) };
}

/// Seed the per-thread roster override (configured, ad-hoc) for a test.
#[cfg(test)]
pub(crate) fn set_test_roster(configured: Vec<String>, adhoc: Vec<String>) {
    TEST_ROSTER.with(|c| *c.borrow_mut() = (configured, adhoc));
}

#[cfg(test)]
pub fn config_remotes() -> Vec<String> {
    TEST_ROSTER.with(|c| c.borrow().0.clone())
}

#[cfg(not(test))]
fn adhoc_path() -> Option<PathBuf> {
    RuntimePaths::for_current_user()
        .ok()
        .map(|p| p.log_dir.join("remotes"))
}

#[cfg(not(test))]
pub fn load_adhoc() -> Vec<String> {
    let Some(p) = adhoc_path() else {
        return Vec::new();
    };
    fs::read_to_string(&p)
        .ok()
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
pub fn load_adhoc() -> Vec<String> {
    TEST_ROSTER.with(|c| c.borrow().1.clone())
}

pub fn add_adhoc(host: &str) {
    let mut cur = load_adhoc();
    if cur.iter().any(|h| h == host) {
        return;
    }
    cur.push(host.to_string());
    if let Err(e) = write_adhoc(&cur) {
        tracing::warn!(%host, error=%e, "roster: add_adhoc write failed");
    }
}

pub fn forget_adhoc(host: &str) {
    let cur: Vec<String> = load_adhoc().into_iter().filter(|h| h != host).collect();
    if let Err(e) = write_adhoc(&cur) {
        tracing::warn!(%host, error=%e, "roster: forget_adhoc write failed");
    }
}

#[cfg(not(test))]
fn write_adhoc(hosts: &[String]) -> io::Result<()> {
    let Some(p) = adhoc_path() else {
        return Ok(());
    };
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = fs::File::create(&p)?;
    for h in hosts {
        writeln!(f, "{h}")?;
    }
    Ok(())
}

// Under test, `add_adhoc`/`forget_adhoc` must not touch the operator's real
// ad-hoc roster file (this crate has no per-test isolated HOME/XDG dir the way
// the daemon crate's `test_env::isolate` gives the persist layer — see
// `#[cfg(not(test))] adhoc_path` above). Route the write through the same
// per-thread `TEST_ROSTER` override `load_adhoc` already reads under test, so
// a pump-level test can seed + forget a host and observe the round trip
// deterministically.
// The `io::Result` return never actually errors here (there's no fallible I/O,
// just a thread_local write) — kept only so this matches the `#[cfg(not(test))]`
// signature above, which callers (`add_adhoc`/`forget_adhoc`) invoke identically
// in both configurations.
#[cfg(test)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "signature must match the #[cfg(not(test))] fallible version callers share"
)]
fn write_adhoc(hosts: &[String]) -> io::Result<()> {
    TEST_ROSTER.with(|c| c.borrow_mut().1 = hosts.to_vec());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_adhoc_and_forget_adhoc_round_trip_through_the_test_hook() {
        // Task 6: `write_adhoc` is cfg(test)-gated to update `TEST_ROSTER`
        // instead of the real remotes file, so `add_adhoc`/`forget_adhoc` are
        // safe to call from a test without touching the operator's disk.
        set_test_roster(vec![], vec!["existing".into()]);
        add_adhoc("new-host");
        assert_eq!(load_adhoc(), vec!["existing".to_string(), "new-host".to_string()]);
        forget_adhoc("existing");
        assert_eq!(load_adhoc(), vec!["new-host".to_string()]);
    }

    #[test]
    fn assemble_dedups_adhoc_against_configured_and_orders() {
        let hosts = assemble(
            &["prod".into(), "wsl2".into()],
            &["scratch".into(), "wsl2".into()],
        );
        let got: Vec<_> = hosts.iter().map(|h| (h.host.as_str(), h.source)).collect();
        assert_eq!(
            got,
            vec![
                ("prod", RosterSource::Configured),
                ("wsl2", RosterSource::Configured),
                ("scratch", RosterSource::AdHoc),
            ]
        );
    }
}
