//! Process-wide panic hook that bridges panics into the daemon's tracing log.
//!
//! The daemon is spawned with stderr → /dev/null (see the client's
//! `spawn_daemon`), so the default libstd hook's stderr message is discarded.
//! Without this, daemon panics are completely invisible. See the
//! terminal-trust-hardening spec, Phase 1.

use std::sync::Once;

static INSTALLED: Once = Once::new();

/// Format a single panic log line. Pure, so it is unit-tested directly
/// (a real `PanicHookInfo` can't be constructed in a test).
fn format_panic(thread: &str, location: Option<String>, message: &str) -> String {
    let loc = location.unwrap_or_else(|| "<unknown>".to_string());
    format!("panic in thread '{thread}' at {loc}: {message}")
}

/// Extract the human string from a panic payload (`&str` or `String`).
fn payload_str(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    }
}

/// Install the tracing-logging panic hook exactly once. Chains to the previous
/// hook so a foregrounded daemon still gets the default stderr message too.
pub(crate) fn install_panic_logging() {
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            let name = thread.name().unwrap_or("unnamed").to_string();
            let location = info.location().map(|l| format!("{}:{}", l.file(), l.line()));
            let message = payload_str(info.payload());
            tracing::error!(target: "panic", "{}", format_panic(&name, location, message));
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_panic_includes_thread_location_and_message() {
        let line = format_panic("pane-reader", Some("src/pane.rs:212".into()), "boom");
        assert!(line.contains("pane-reader"), "thread: {line}");
        assert!(line.contains("src/pane.rs:212"), "location: {line}");
        assert!(line.contains("boom"), "message: {line}");
    }

    #[test]
    fn format_panic_handles_unknown_location() {
        let line = format_panic("main", None, "kaboom");
        assert!(line.contains("kaboom"));
        assert!(line.contains("<unknown>"), "must mark missing location: {line}");
    }
}
