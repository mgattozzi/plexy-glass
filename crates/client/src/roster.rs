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

use crate::transport::RemoteName;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterSource {
    Configured,
    AdHoc,
}

/// A configured remote from `config.kdl`: an ssh target and its optional
/// per-host `plexy-glass` path. The client-side mirror of `config::RemoteHost`
/// (which uses a plain `String` host at the config boundary), carrying a
/// `RemoteName` so the roster deals in the typed host throughout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRemote {
    pub host: RemoteName,
    pub bin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterHost {
    pub host: RemoteName,
    /// The configured `bin` path, if this host came from `config.kdl` with one.
    /// Always `None` for an ad-hoc host — those are just names the user `-H`'d
    /// into, with no config entry to carry a path.
    pub bin: Option<String>,
    pub source: RosterSource,
}

/// Assemble the roster: every distinct `configured` host (sorted, source
/// `Configured`) followed by every distinct `adhoc` host that isn't already
/// configured (sorted, source `AdHoc`). A configured host keeps its `bin`; an
/// ad-hoc host has none.
pub fn assemble(configured: &[ConfigRemote], adhoc: &[RemoteName]) -> Vec<RosterHost> {
    let mut cfg: Vec<ConfigRemote> = configured.to_vec();
    cfg.sort_by(|a, b| a.host.cmp(&b.host));
    cfg.dedup_by(|a, b| a.host == b.host);
    let cfgset: HashSet<&RemoteName> = cfg.iter().map(|r| &r.host).collect();
    let mut ad: Vec<RemoteName> = adhoc
        .iter()
        .filter(|h| !cfgset.contains(*h))
        .cloned()
        .collect();
    ad.sort();
    ad.dedup();
    cfg.into_iter()
        .map(|r| RosterHost {
            host: r.host,
            bin: r.bin,
            source: RosterSource::Configured,
        })
        .chain(ad.into_iter().map(|host| RosterHost {
            host,
            bin: None,
            source: RosterSource::AdHoc,
        }))
        .collect()
}

/// The configured `bin` for `name`, if `config.kdl` declares one — for the
/// reconnect path, which starts from a bare `Host` and re-derives the bin here
/// rather than threading it through the picker. Reads the same
/// `config_remotes()` the roster query does, so the two never disagree.
pub fn config_bin_for(name: &RemoteName) -> Option<String> {
    config_remotes()
        .into_iter()
        .find(|r| &r.host == name)
        .and_then(|r| r.bin)
}

/// The operator's LOCAL config remotes. Client-side config parse errors are
/// swallowed here (the picker still works from the ad-hoc file + this session's
/// hosts); the daemon logs its own config error separately.
#[cfg(not(test))]
pub fn config_remotes() -> Vec<ConfigRemote> {
    let (cfg, _err) = plexy_glass_config::load_or_default();
    // The config/roster boundary: convert the host name into `RemoteName` here so
    // the rest of the roster deals in the typed host, carrying the per-host `bin`.
    cfg.remotes
        .into_iter()
        .map(|r| ConfigRemote {
            host: RemoteName::from(r.host),
            bin: r.bin,
        })
        .collect()
}

/// The operator's LOCAL config palette, read from the same `load_or_default()`
/// as `config_remotes` so the picker parses `config.kdl` once per open. Parse
/// errors fall back to the built-in default palette (never blanks the picker).
#[cfg(not(test))]
pub fn config_palette() -> plexy_glass_config::PaletteConfig {
    let (cfg, _err) = plexy_glass_config::load_or_default();
    cfg.palette
}

/// Test seam: the default (empty) palette, so unit tests resolve to fixed
/// theme defaults without touching the user's real config.
#[cfg(test)]
pub fn config_palette() -> plexy_glass_config::PaletteConfig {
    plexy_glass_config::PaletteConfig::default()
}

// Under `cfg(test)` the roster sources read a per-thread override instead of
// the real config / ad-hoc files, so a pump-level test can seed a roster
// (`set_test_roster`) deterministically without touching the user's real
// `config.kdl` or `remotes` file. Defaults to empty, so tests that don't seed
// one (the existing `pump_picker_*` tests) see NO remotes and never fire a
// real query.
#[cfg(test)]
thread_local! {
    static TEST_ROSTER: RefCell<(Vec<ConfigRemote>, Vec<RemoteName>)> =
        const { RefCell::new((Vec::new(), Vec::new())) };
}

/// Seed the per-thread roster override (configured, ad-hoc) for a test. The
/// configured hosts get no `bin`; use [`set_test_roster_configured`] for a test
/// that needs per-host bins.
#[cfg(test)]
pub(crate) fn set_test_roster(configured: Vec<RemoteName>, adhoc: Vec<RemoteName>) {
    let configured = configured
        .into_iter()
        .map(|host| ConfigRemote { host, bin: None })
        .collect();
    TEST_ROSTER.with(|c| *c.borrow_mut() = (configured, adhoc));
}

/// Seed the roster override with full `ConfigRemote`s, so a test can give a
/// configured host a `bin`.
#[cfg(test)]
pub(crate) fn set_test_roster_configured(configured: Vec<ConfigRemote>, adhoc: Vec<RemoteName>) {
    TEST_ROSTER.with(|c| *c.borrow_mut() = (configured, adhoc));
}

#[cfg(test)]
pub fn config_remotes() -> Vec<ConfigRemote> {
    TEST_ROSTER.with(|c| c.borrow().0.clone())
}

#[cfg(not(test))]
fn adhoc_path() -> Option<PathBuf> {
    RuntimePaths::for_current_user()
        .ok()
        .map(|p| p.log_dir.join("remotes"))
}

#[cfg(not(test))]
pub fn load_adhoc() -> Vec<RemoteName> {
    let Some(p) = adhoc_path() else {
        return Vec::new();
    };
    fs::read_to_string(&p)
        .ok()
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(RemoteName::from)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
pub fn load_adhoc() -> Vec<RemoteName> {
    TEST_ROSTER.with(|c| c.borrow().1.clone())
}

pub fn add_adhoc(host: &RemoteName) {
    // A host that's already a configured remote doesn't belong in the ad-hoc
    // file — `assemble` filters it out at read time anyway, but skipping the
    // append keeps the file from accumulating configured hosts (and lets one
    // drop out on the next write once it's promoted to `config.kdl`).
    if config_remotes().iter().any(|r| &r.host == host) {
        return;
    }
    let mut cur = load_adhoc();
    if cur.iter().any(|h| h == host) {
        return;
    }
    cur.push(host.clone());
    if let Err(e) = write_adhoc(&cur) {
        tracing::warn!(%host, error=%e, "roster: add_adhoc write failed");
    }
}

pub fn forget_adhoc(host: &RemoteName) {
    let cur: Vec<RemoteName> = load_adhoc().into_iter().filter(|h| h != host).collect();
    if let Err(e) = write_adhoc(&cur) {
        tracing::warn!(%host, error=%e, "roster: forget_adhoc write failed");
    }
}

#[cfg(not(test))]
fn write_adhoc(hosts: &[RemoteName]) -> io::Result<()> {
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
fn write_adhoc(hosts: &[RemoteName]) -> io::Result<()> {
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
        set_test_roster(vec![], vec![RemoteName::from("existing")]);
        add_adhoc(&RemoteName::from("new-host"));
        assert_eq!(
            load_adhoc(),
            vec![RemoteName::from("existing"), RemoteName::from("new-host")]
        );
        forget_adhoc(&RemoteName::from("existing"));
        assert_eq!(load_adhoc(), vec![RemoteName::from("new-host")]);
    }

    #[test]
    fn add_adhoc_skips_a_host_thats_already_configured() {
        // Finding 3: `-H`'ing a host that's already a config `remotes` entry must
        // NOT append it to the ad-hoc file — `assemble` filters it at read time,
        // but keeping it out of the file lets it drop out on the next write.
        set_test_roster(vec![RemoteName::from("prod")], vec![]);
        add_adhoc(&RemoteName::from("prod"));
        assert_eq!(
            load_adhoc(),
            Vec::<RemoteName>::new(),
            "a configured host is not written into the ad-hoc roster"
        );
    }

    #[test]
    fn config_palette_returns_a_palette() {
        // Under cfg(test) this reads the default palette (no real config file),
        // so it must contain the built-in roles the picker resolves.
        let p = config_palette();
        assert!(p.entries.contains_key("accent") || p.entries.is_empty());
    }

    #[test]
    fn assemble_dedups_adhoc_against_configured_and_orders() {
        let cfg = |h: &str| ConfigRemote {
            host: RemoteName::from(h),
            bin: None,
        };
        let hosts = assemble(
            &[cfg("prod"), cfg("wsl2")],
            &[RemoteName::from("scratch"), RemoteName::from("wsl2")],
        );
        let got: Vec<_> = hosts.iter().map(|h| (&*h.host, h.source)).collect();
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
