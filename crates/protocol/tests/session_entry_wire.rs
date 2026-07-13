//! Wire-contract tests for Task 2 of the kill-and-follow-sessions plan:
//! `SessionEntry.last_active` and the resulting `PROTOCOL_VERSION` bump to 13.
//! `last_active` is appended after `SessionEntry`'s existing fields, so a
//! plain round-trip is enough to prove postcard still decodes it correctly.

use std::time::SystemTime;

use plexy_glass_protocol::{PROTOCOL_VERSION, SessionEntry};

#[test]
fn protocol_version_is_13() {
    assert_eq!(PROTOCOL_VERSION.0, 13);
}

#[test]
fn session_entry_round_trips_last_active() {
    let entry = SessionEntry {
        name: "main".to_string(),
        windows: 2,
        panes: 3,
        clients: 1,
        created: SystemTime::UNIX_EPOCH,
        last_active: SystemTime::now(),
    };
    let bytes = postcard::to_allocvec(&entry).expect("serialize");
    let back: SessionEntry = postcard::from_bytes(&bytes).expect("deserialize");
    assert_eq!(back, entry);
}
