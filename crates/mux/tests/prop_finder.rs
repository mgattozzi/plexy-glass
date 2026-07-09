//! Invariants of the shared finder core: the cursor stays selectable, the
//! selection is always a real filtered index (or None), and filtering is an
//! order-preserving subset with empty-filter identity.

use hegel::{TestCase, generators as gs};
use plexy_glass_mux::{FilterList, filtered_indices};

fn draw_haystacks(tc: &TestCase) -> Vec<String> {
    let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(30));
    (0..n)
        .map(|_| tc.draw(gs::text().max_size(8)).to_lowercase())
        .collect()
}

#[hegel::test(test_cases = 400)]
fn filtered_indices_is_order_preserving_subset(tc: TestCase) {
    let hs = draw_haystacks(&tc);
    let filter = tc.draw(gs::text().max_size(4));
    let idx = filtered_indices(&hs, &filter);
    // Subset of 0..len, strictly increasing (order-preserving, no dup).
    assert!(idx.iter().all(|&i| i < hs.len()));
    assert!(
        idx.windows(2).all(|w| w[0] < w[1]),
        "indices strictly increasing"
    );
    // Empty filter is the identity.
    if filter.is_empty() {
        assert_eq!(idx, (0..hs.len()).collect::<Vec<_>>());
    }
}

#[hegel::test(test_cases = 400)]
fn cursor_and_selected_stay_valid_under_edits(tc: TestCase) {
    let hs = draw_haystacks(&tc);
    let mut f = FilterList::new();
    // Apply a random sequence of edits.
    let steps = tc.draw(gs::integers::<usize>().min_value(0).max_value(20));
    for _ in 0..steps {
        match tc.draw(gs::integers::<u8>().min_value(0).max_value(5)) {
            0 => f.push(tc.draw(gs::integers::<u8>().min_value(b'a').max_value(b'e')) as char),
            1 => {
                f.backspace(&hs);
            }
            2 => {
                f.down(&hs);
            }
            3 => {
                f.up();
            }
            4 => {
                f.end(&hs);
            }
            _ => {
                f.home();
            }
        }
        // Invariant: `selected` is either None (empty filtered view) or a real
        // member of the current filtered set.
        let vis = filtered_indices(&hs, &f.filter);
        match f.selected(&hs) {
            None => assert!(vis.is_empty(), "None only when nothing matches"),
            Some(i) => assert!(vis.contains(&i), "selected must be a filtered index"),
        }
    }
}
