//! Property tests for the roster assemble/dedup logic: the union of config
//! `remotes` and the ad-hoc client-side host list should never duplicate a
//! host, every distinct configured host appears exactly once as
//! `Configured`, no host appears as `AdHoc` if it's also configured, and a
//! configured host's `bin` rides through while ad-hoc hosts never carry one.

use std::collections::HashSet;

use hegel::{TestCase, generators as gs};
use plexy_glass_client::RemoteName;
use plexy_glass_client::roster::{ConfigRemote, RosterSource, assemble};

fn draw_names(tc: &TestCase) -> Vec<RemoteName> {
    let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(20));
    (0..n)
        .map(|_| RemoteName::from(tc.draw(gs::text().max_size(8))))
        .collect()
}

/// Configured hosts, each with a `bin` present about half the time — so the
/// dedup/source invariants are exercised alongside bin-carrying entries.
fn draw_configured(tc: &TestCase) -> Vec<ConfigRemote> {
    let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(20));
    (0..n)
        .map(|_| {
            let host = RemoteName::from(tc.draw(gs::text().max_size(8)));
            let bin = if tc.draw(gs::booleans()) {
                Some(format!("/bin/{host}"))
            } else {
                None
            };
            ConfigRemote { host, bin }
        })
        .collect()
}

#[hegel::test(test_cases = 400)]
fn assemble_never_duplicates_a_host(tc: TestCase) {
    let configured = draw_configured(&tc);
    let adhoc = draw_names(&tc);
    let hosts = assemble(&configured, &adhoc);
    let mut seen = HashSet::new();
    for h in &hosts {
        assert!(seen.insert(h.host.clone()), "duplicate host: {}", h.host);
    }
}

#[hegel::test(test_cases = 400)]
fn every_distinct_configured_host_present_exactly_once(tc: TestCase) {
    let configured = draw_configured(&tc);
    let adhoc = draw_names(&tc);
    let hosts = assemble(&configured, &adhoc);
    let distinct: HashSet<&RemoteName> = configured.iter().map(|r| &r.host).collect();
    for host in distinct {
        let count = hosts
            .iter()
            .filter(|h| &h.host == host && h.source == RosterSource::Configured)
            .count();
        assert_eq!(count, 1, "expected {host} exactly once as Configured");
    }
}

#[hegel::test(test_cases = 400)]
fn no_adhoc_host_is_also_configured(tc: TestCase) {
    let configured = draw_configured(&tc);
    let adhoc = draw_names(&tc);
    let hosts = assemble(&configured, &adhoc);
    let cfgset: HashSet<&RemoteName> = configured.iter().map(|r| &r.host).collect();
    for h in &hosts {
        if h.source == RosterSource::AdHoc {
            assert!(
                !cfgset.contains(&h.host),
                "{} appeared as AdHoc but is also configured",
                h.host
            );
        }
    }
}

/// A configured host's `bin` survives into its `RosterHost`, and an ad-hoc host
/// never carries one. `dedup_by` keeps the FIRST of a duplicated host, so the
/// expected bin is the first config entry's — the same rule `assemble` applies.
#[hegel::test(test_cases = 400)]
fn configured_bin_rides_through_and_adhoc_never_has_one(tc: TestCase) {
    let configured = draw_configured(&tc);
    let adhoc = draw_names(&tc);
    let hosts = assemble(&configured, &adhoc);

    for h in &hosts {
        match h.source {
            RosterSource::AdHoc => assert_eq!(h.bin, None, "ad-hoc host {} carried a bin", h.host),
            RosterSource::Configured => {
                // The first config entry for this host wins (sort is stable, then
                // dedup_by keeps the first), so its bin is what should ride.
                let expected = configured
                    .iter()
                    .find(|r| r.host == h.host)
                    .and_then(|r| r.bin.clone());
                assert_eq!(h.bin, expected, "wrong bin for configured {}", h.host);
            }
        }
    }
}
