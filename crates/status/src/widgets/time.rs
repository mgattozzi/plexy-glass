use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use smol_str::SmolStr;

use crate::{EvalContext, ResolvedStyle, StyledText, Widget};

pub struct TimeWidget {
    pub format: String,
    pub interval: Option<Duration>,
    pub style: ResolvedStyle,
    pub icon: SmolStr,
    /// When true, format the time in UTC (so `%Z` renders `UTC`); otherwise
    /// the local timezone.
    pub utc: bool,
}

#[async_trait]
impl Widget for TimeWidget {
    fn interval(&self) -> Option<Duration> {
        self.interval.or(Some(Duration::from_secs(1)))
    }
    async fn evaluate(&mut self, _ctx: &EvalContext<'_>) -> StyledText {
        use std::fmt::Write as _;
        // chrono's `Display` returns `Err` on a malformed format specifier;
        // `.to_string()` would then panic ("a Display implementation returned an
        // error unexpectedly") and kill the spawned status tick task, silently
        // freezing the bar. A user `time format="…"` is unvalidated config, so
        // degrade to a safe default on error instead of panicking. `format_now`
        // closes over the chosen clock (UTC or local) so both the primary and
        // fallback formats use the same one.
        let format_now = |fmt: &str, out: &mut String| -> fmt::Result {
            if self.utc {
                write!(out, "{}", chrono::Utc::now().format(fmt))
            } else {
                write!(out, "{}", chrono::Local::now().format(fmt))
            }
        };
        let mut body = String::new();
        if format_now(&self.format, &mut body).is_err() {
            body.clear();
            let _ = format_now("%H:%M", &mut body);
        }
        let text = if self.icon.is_empty() {
            body
        } else {
            format!("{} {body}", self.icon)
        };
        StyledText::single(SmolStr::new(text), self.style)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_empty<'a>() -> EvalContext<'a> {
        EvalContext {
            session_name: "main",
            windows: &[],
            active_window: 0,
            attached_clients: 1,
            prefix_active: false,
            active_pane_cwd: None,
            copy_mode_active: false,
            sync_active: false,
            zoom_active: false,
            dragging_window: None,
            remote: false,
        }
    }

    #[tokio::test]
    async fn time_widget_prefixes_clock_icon() {
        let mut w = TimeWidget {
            format: "%H:%M".into(),
            interval: None,
            style: ResolvedStyle::default(),
            icon: SmolStr::new("\u{25f7}"), // unicode clock
            utc: false,
        };
        let out = w.evaluate(&ctx_empty()).await;
        let text: String = out.segments.iter().map(|s| s.text.as_str()).collect();
        assert!(
            text.starts_with("\u{25f7} "),
            "leads with clock + space: {text:?}"
        );
    }

    #[tokio::test]
    async fn time_emits_non_empty() {
        let mut w = TimeWidget {
            format: "%H:%M".to_string(),
            interval: None,
            style: ResolvedStyle::default(),
            icon: SmolStr::new(""),
            utc: false,
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert!(!out.segments[0].text.is_empty());
        // %H:%M -> "12:34" 5 chars when icon is empty.
        assert_eq!(out.segments[0].text.len(), 5);
    }

    #[tokio::test]
    async fn time_bad_format_degrades_instead_of_panicking() {
        // "%Q" is an invalid chrono specifier; the old `.to_string()` panicked
        // and killed the tick task. It must now fall back to the default.
        let mut w = TimeWidget {
            format: "%Q".to_string(),
            interval: None,
            style: ResolvedStyle::default(),
            icon: SmolStr::new(""),
            utc: false,
        };
        let out = w.evaluate(&ctx_empty()).await;
        // Fallback "%H:%M" -> "12:34".
        assert_eq!(out.segments[0].text.len(), 5);
        assert!(out.segments[0].text.contains(':'));
    }

    #[tokio::test]
    async fn time_utc_renders_utc_timezone() {
        let mut w = TimeWidget {
            format: "%H:%M %Z".to_string(),
            interval: None,
            style: ResolvedStyle::default(),
            icon: SmolStr::new(""),
            utc: true,
        };
        let out = w.evaluate(&ctx_empty()).await;
        let text = out.segments[0].text.as_str();
        assert!(
            text.contains("UTC"),
            "UTC clock %Z must render `UTC`: {text:?}"
        );
        assert!(text.contains(':'), "24h time present: {text:?}");
    }
}
