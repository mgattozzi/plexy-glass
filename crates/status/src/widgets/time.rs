use crate::{EvalContext, ResolvedStyle, StyledText, Widget};
use async_trait::async_trait;
use smol_str::SmolStr;
use std::time::Duration;

pub struct TimeWidget {
    pub format: String,
    pub interval: Option<Duration>,
    pub style: ResolvedStyle,
}

#[async_trait]
impl Widget for TimeWidget {
    fn interval(&self) -> Option<Duration> {
        self.interval.or(Some(Duration::from_secs(1)))
    }
    async fn evaluate(&mut self, _ctx: &EvalContext<'_>) -> StyledText {
        let now = chrono::Local::now();
        let text = now.format(&self.format).to_string();
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
        }
    }

    #[tokio::test]
    async fn time_emits_non_empty() {
        let mut w = TimeWidget {
            format: "%H:%M".to_string(),
            interval: None,
            style: ResolvedStyle::default(),
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert!(!out.segments[0].text.is_empty());
        // %H:%M -> "12:34" 5 chars.
        assert_eq!(out.segments[0].text.len(), 5);
    }
}
