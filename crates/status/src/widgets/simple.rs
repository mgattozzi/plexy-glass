use crate::{EvalContext, ResolvedStyle, StyledText, Widget};
use async_trait::async_trait;
use smol_str::SmolStr;
use std::time::Duration;

pub struct TextWidget {
    pub text: SmolStr,
    pub style: ResolvedStyle,
}

#[async_trait]
impl Widget for TextWidget {
    fn interval(&self) -> Option<Duration> {
        None
    }
    async fn evaluate(&mut self, _ctx: &EvalContext<'_>) -> StyledText {
        StyledText::single(self.text.clone(), self.style)
    }
}

pub struct SeparatorWidget {
    pub ch: char,
    pub style: ResolvedStyle,
}

#[async_trait]
impl Widget for SeparatorWidget {
    fn interval(&self) -> Option<Duration> {
        None
    }
    async fn evaluate(&mut self, _ctx: &EvalContext<'_>) -> StyledText {
        let s = SmolStr::new(self.ch.to_string());
        StyledText::single(s, self.style)
    }
}

pub struct SessionWidget {
    pub style: ResolvedStyle,
    pub pad_left: u8,
    pub pad_right: u8,
}

#[async_trait]
impl Widget for SessionWidget {
    fn interval(&self) -> Option<Duration> {
        None
    }
    async fn evaluate(&mut self, ctx: &EvalContext<'_>) -> StyledText {
        let mut buf = String::new();
        for _ in 0..self.pad_left {
            buf.push(' ');
        }
        buf.push_str(ctx.session_name);
        for _ in 0..self.pad_right {
            buf.push(' ');
        }
        StyledText::single(SmolStr::new(buf), self.style)
    }
}

pub struct HostnameWidget {
    pub style: ResolvedStyle,
    pub interval: Option<Duration>,
    cached: Option<SmolStr>,
}

impl HostnameWidget {
    pub fn new(style: ResolvedStyle, interval: Option<Duration>) -> Self {
        Self {
            style,
            interval,
            cached: None,
        }
    }
}

#[async_trait]
impl Widget for HostnameWidget {
    fn interval(&self) -> Option<Duration> {
        self.interval.or(Some(Duration::from_secs(60)))
    }
    async fn evaluate(&mut self, _ctx: &EvalContext<'_>) -> StyledText {
        if self.cached.is_none() {
            let name = hostname::get()
                .ok()
                .and_then(|s| s.into_string().ok())
                .unwrap_or_default();
            self.cached = Some(SmolStr::new(name));
        }
        // invariant: we just populated self.cached above if it was None
        let text = self.cached.clone().unwrap_or_default();
        StyledText::single(text, self.style)
    }
}

pub struct AttachedClientsWidget {
    pub style: ResolvedStyle,
    pub min_count: u8,
}

#[async_trait]
impl Widget for AttachedClientsWidget {
    fn interval(&self) -> Option<Duration> {
        None
    }
    async fn evaluate(&mut self, ctx: &EvalContext<'_>) -> StyledText {
        if ctx.attached_clients < self.min_count {
            return StyledText::empty();
        }
        let text = SmolStr::new(format!("*{}", ctx.attached_clients));
        StyledText::single(text, self.style)
    }
}

pub struct PrefixIndicatorWidget {
    pub style: ResolvedStyle,
    pub content: SmolStr,
}

#[async_trait]
impl Widget for PrefixIndicatorWidget {
    fn interval(&self) -> Option<Duration> {
        None
    }
    async fn evaluate(&mut self, ctx: &EvalContext<'_>) -> StyledText {
        if ctx.copy_mode_active {
            return StyledText::single(SmolStr::new(" COPY "), self.style);
        }
        if ctx.sync_active {
            return StyledText::single(SmolStr::new(" SYNC "), self.style);
        }
        if !ctx.prefix_active {
            return StyledText::empty();
        }
        StyledText::single(self.content.clone(), self.style)
    }
}

pub struct CwdWidget {
    pub style: ResolvedStyle,
    pub max_components: Option<u8>,
}

#[async_trait]
impl Widget for CwdWidget {
    fn interval(&self) -> Option<Duration> {
        None
    }
    async fn evaluate(&mut self, ctx: &EvalContext<'_>) -> StyledText {
        let Some(url) = ctx.active_pane_cwd else {
            return StyledText::empty();
        };
        // OSC 7 format: file://host/path. Strip scheme + optional host.
        let path = match url.strip_prefix("file://") {
            Some(rest) => match rest.find('/') {
                Some(i) => &rest[i..],
                None => return StyledText::empty(),
            },
            None => url,
        };
        let display = if let Some(max) = self.max_components {
            let parts: Vec<&str> = path
                .trim_matches('/')
                .split('/')
                .filter(|s| !s.is_empty())
                .collect();
            let n = max as usize;
            if parts.len() <= n {
                path.to_string()
            } else {
                let tail = &parts[parts.len() - n..];
                format!("…/{}", tail.join("/"))
            }
        } else {
            path.to_string()
        };
        StyledText::single(SmolStr::new(display), self.style)
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
        }
    }

    #[tokio::test]
    async fn text_widget_emits_literal() {
        let mut w = TextWidget {
            text: "hi".into(),
            style: ResolvedStyle::default(),
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert_eq!(out.segments.len(), 1);
        assert_eq!(out.segments[0].text.as_str(), "hi");
    }

    #[tokio::test]
    async fn session_widget_pads() {
        let mut w = SessionWidget {
            style: ResolvedStyle::default(),
            pad_left: 1,
            pad_right: 2,
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert_eq!(out.segments[0].text.as_str(), " main  ");
    }

    #[tokio::test]
    async fn attached_clients_hides_below_min() {
        let mut w = AttachedClientsWidget {
            style: ResolvedStyle::default(),
            min_count: 2,
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert!(out.segments.is_empty());
    }

    #[tokio::test]
    async fn attached_clients_shows_at_min() {
        let mut w = AttachedClientsWidget {
            style: ResolvedStyle::default(),
            min_count: 2,
        };
        let ctx = EvalContext {
            attached_clients: 3,
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), "*3");
    }

    #[tokio::test]
    async fn cwd_widget_strips_osc7_scheme() {
        let mut w = CwdWidget {
            style: ResolvedStyle::default(),
            max_components: None,
        };
        let ctx = EvalContext {
            active_pane_cwd: Some("file://localhost/tmp/work"),
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), "/tmp/work");
    }

    #[tokio::test]
    async fn cwd_widget_truncates_when_max_components() {
        let mut w = CwdWidget {
            style: ResolvedStyle::default(),
            max_components: Some(2),
        };
        let ctx = EvalContext {
            active_pane_cwd: Some("file:///a/b/c/d/e"),
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), "…/d/e");
    }

    #[tokio::test]
    async fn prefix_indicator_hidden_when_inactive() {
        let mut w = PrefixIndicatorWidget {
            style: ResolvedStyle::default(),
            content: "PFX".into(),
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert!(out.segments.is_empty());
    }

    #[tokio::test]
    async fn prefix_indicator_shown_when_active() {
        let mut w = PrefixIndicatorWidget {
            style: ResolvedStyle::default(),
            content: "PFX".into(),
        };
        let ctx = EvalContext {
            prefix_active: true,
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), "PFX");
    }

    #[tokio::test]
    async fn prefix_indicator_shows_copy_when_in_copy_mode() {
        let mut w = PrefixIndicatorWidget {
            style: ResolvedStyle::default(),
            content: "PFX".into(),
        };
        let ctx = EvalContext {
            copy_mode_active: true,
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), " COPY ");
    }

    #[tokio::test]
    async fn prefix_indicator_shows_sync_when_sync_active() {
        let mut w = PrefixIndicatorWidget {
            style: ResolvedStyle::default(),
            content: "PFX".into(),
        };
        let ctx = EvalContext {
            sync_active: true,
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), " SYNC ");
    }

    #[tokio::test]
    async fn copy_mode_beats_sync_in_priority() {
        let mut w = PrefixIndicatorWidget {
            style: ResolvedStyle::default(),
            content: "PFX".into(),
        };
        let ctx = EvalContext {
            copy_mode_active: true,
            sync_active: true,
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), " COPY ");
    }
}
