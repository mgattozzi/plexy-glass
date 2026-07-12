//! A validated session name.
//!
//! Every session name enters the daemon's logic through [`SessionName::parse`],
//! which runs the one and only name check (non-empty, ≤64 bytes, and every char
//! in `[A-Za-z0-9_-]`). Once parsed, an invalid name is unrepresentable: it's the
//! registry's map key and the identity a session is stored under, so a bad name
//! can never be keyed on or acted upon.
//!
//! The wire deliberately does NOT carry `SessionName` — the message fields stay
//! `String` (`AttachOrCreate.name`, `SwitchSession.name`, `KillSession.name`) so
//! a malformed name from any client still DECODES and then errors gracefully with
//! `ServerMsg::Error(EmptyName | InvalidName)`, instead of failing the frame
//! decode (which would drop the connection with no reply). The daemon parses
//! `String -> SessionName` at the registry boundary; our own client parses before
//! it sends, so it can never put an invalid name on the wire in the first place.

use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;

use crate::errors::ProtocolError;

/// Longest accepted session name, in bytes. Names are ASCII by the char check,
/// so bytes and chars coincide.
const MAX_LEN: usize = 64;

/// A session name that has passed validation. Construct one with
/// [`SessionName::parse`]; there is no other way to make one, which is the whole
/// point.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionName(String);

impl SessionName {
    /// Parse a raw name, or return the graceful protocol error the daemon replies
    /// with. This is the single validation site (it replaced the daemon's old
    /// `validate_name`): non-empty, ≤64 bytes, every char in `[A-Za-z0-9_-]`.
    pub fn parse(name: &str) -> Result<Self, ProtocolError> {
        if name.is_empty() || name.len() > MAX_LEN {
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
        Ok(Self(name.to_string()))
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for SessionName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

// Read paths (`HashMap<SessionName, _>::get` / `remove`) look up by `&str`
// without parsing: an invalid name simply isn't in the map, so a lookup of one
// finds nothing — exactly the old `get`/`kill` behavior (they never validated).
// `Borrow<str>` is sound here because the derived `Hash`/`Eq` delegate to the
// inner `String`, which hashes and compares identically to the borrowed `str`.
impl Borrow<str> for SessionName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// The exact predicate the daemon's `validate_name` used, kept here as the
    /// reference oracle the differential test (and `prop_session_name`) check
    /// `SessionName::parse` against. Returns the same error variant so we can
    /// assert variant-for-variant parity, not just is_ok parity.
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

    #[test]
    fn parse_accepts_valid_names() {
        for ok in [
            "main",
            "a",
            "dev-1",
            "my_session",
            "A-Z_0-9",
            &"x".repeat(64),
        ] {
            let n = SessionName::parse(ok).expect("valid name");
            assert_eq!(n.as_str(), ok);
        }
    }

    #[test]
    fn parse_rejects_empty_and_too_long() {
        assert_eq!(SessionName::parse(""), Err(ProtocolError::EmptyName));
        // 65 chars is over the limit; the old code folded that into EmptyName.
        assert_eq!(
            SessionName::parse(&"x".repeat(65)),
            Err(ProtocolError::EmptyName)
        );
    }

    #[test]
    fn parse_rejects_bad_chars() {
        for bad in ["has space", "a.b", "a/b", "café", "tab\there", "emoji😀"] {
            assert!(
                matches!(
                    SessionName::parse(bad),
                    Err(ProtocolError::InvalidName { .. })
                ),
                "{bad:?} should be InvalidName"
            );
        }
    }

    #[test]
    fn differential_matches_old_validate_name() {
        // Hand-picked corners; the property test covers the broad random space.
        let cases = [
            "",
            "main",
            "a",
            &"x".repeat(64),
            &"x".repeat(65),
            "has space",
            "a.b",
            "under_score-and-dash",
            "café",
            "\u{0}",
            "MixedCase123",
        ];
        for s in cases {
            assert_eq!(
                SessionName::parse(s).map(|_| ()),
                old_validate(s),
                "parse and old validate_name must agree on {s:?}"
            );
        }
    }

    #[test]
    fn borrows_as_str_for_map_lookup() {
        let mut map: HashMap<SessionName, u8> = HashMap::new();
        map.insert(SessionName::parse("main").unwrap(), 7);
        // Lookup by &str goes through Borrow<str> — no parse needed.
        assert_eq!(map.get("main"), Some(&7));
        assert_eq!(map.get("nope"), None);
        assert_eq!(map.remove("main"), Some(7));
    }

    #[test]
    fn deref_and_display() {
        let n = SessionName::parse("main").unwrap();
        assert_eq!(&*n, "main");
        assert_eq!(n.len(), 4); // via Deref<str>
        assert_eq!(n.to_string(), "main"); // via Display
    }
}
