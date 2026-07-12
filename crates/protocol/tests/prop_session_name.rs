//! Property test: `SessionName::parse` accepts a name iff the daemon's old
//! `validate_name` did, and returns the same error variant when it rejects.
//!
//! This is the differential guard the Phase-4 refactor leaned on: the validation
//! logic MOVED from `registry::validate_name` into `SessionName::parse`, and this
//! proves the move was behavior-preserving over the whole random string space,
//! not just the hand-picked corners in the unit tests. The oracle is a verbatim
//! copy of the deleted `validate_name` predicate.

use hegel::{TestCase, generators as gs};
use plexy_glass_protocol::{ProtocolError, SessionName};

/// The exact predicate `registry::validate_name` used before Phase 4, returning
/// the same `ProtocolError` variants so we can check variant parity.
fn old_validate(name: &str) -> Result<(), ProtocolError> {
    if name.is_empty() || name.len() > 64 {
        return Err(ProtocolError::EmptyName);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(ProtocolError::InvalidName {
            name: name.to_string(),
        });
    }
    Ok(())
}

#[hegel::test(test_cases = 4096)]
fn parse_agrees_with_old_validate(tc: TestCase) {
    let s = tc.draw(gs::text());
    tc.note(&format!("name = {s:?}"));
    // Variant-for-variant parity (EmptyName / InvalidName / Ok), not just is_ok.
    assert_eq!(SessionName::parse(&s).map(|_| ()), old_validate(&s));
}

#[hegel::test(test_cases = 4096)]
fn accepted_names_round_trip_through_as_str(tc: TestCase) {
    let s = tc.draw(gs::text());
    if let Ok(name) = SessionName::parse(&s) {
        // A parsed name's bytes are unchanged, and re-parsing its own `as_str`
        // yields an equal value (idempotent construction).
        assert_eq!(name.as_str(), s);
        assert_eq!(SessionName::parse(name.as_str()), Ok(name));
    }
}
