//! Property tests for the roster assemble/dedup logic: the union of config
//! `remotes` and the ad-hoc client-side host list should never duplicate a
//! host, every distinct configured host appears exactly once as
//! `Configured`, and no host appears as `AdHoc` if it's also configured.

use std::collections::HashSet;

use hegel::{TestCase, generators as gs};
use plexy_glass_client::Host;
use plexy_glass_client::roster::{RosterSource, assemble};

fn draw_hosts(tc: &TestCase) -> Vec<Host> {
    let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(20));
    (0..n)
        .map(|_| Host::from(tc.draw(gs::text().max_size(8))))
        .collect()
}

#[hegel::test(test_cases = 400)]
fn assemble_never_duplicates_a_host(tc: TestCase) {
    let configured = draw_hosts(&tc);
    let adhoc = draw_hosts(&tc);
    let hosts = assemble(&configured, &adhoc);
    let mut seen = HashSet::new();
    for h in &hosts {
        assert!(seen.insert(h.host.clone()), "duplicate host: {}", h.host);
    }
}

#[hegel::test(test_cases = 400)]
fn every_distinct_configured_host_present_exactly_once(tc: TestCase) {
    let configured = draw_hosts(&tc);
    let adhoc = draw_hosts(&tc);
    let hosts = assemble(&configured, &adhoc);
    let distinct: HashSet<&Host> = configured.iter().collect();
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
    let configured = draw_hosts(&tc);
    let adhoc = draw_hosts(&tc);
    let hosts = assemble(&configured, &adhoc);
    let cfgset: HashSet<&Host> = configured.iter().collect();
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
