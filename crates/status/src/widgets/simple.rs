use crate::{EvalContext, ResolvedStyle, StyledText, Widget};
use async_trait::async_trait;
use nix::unistd;
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
    pub icon: SmolStr,
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
        if !self.icon.is_empty() {
            buf.push_str(&self.icon);
            buf.push(' ');
        }
        buf.push_str(ctx.session_name);
        for _ in 0..self.pad_right {
            buf.push(' ');
        }
        StyledText::single_clickable(
            SmolStr::new(buf),
            self.style,
            crate::ClickAction::Detach,
        )
    }
}

pub struct HostnameWidget {
    pub style: ResolvedStyle,
    pub interval: Option<Duration>,
    pub icon: SmolStr,
    cached: Option<SmolStr>,
}

impl HostnameWidget {
    pub const fn new(style: ResolvedStyle, interval: Option<Duration>, icon: SmolStr) -> Self {
        Self {
            style,
            interval,
            icon,
            cached: None,
        }
    }
}

#[async_trait]
impl Widget for HostnameWidget {
    fn interval(&self) -> Option<Duration> {
        self.interval.or(Some(Duration::from_mins(1)))
    }
    async fn evaluate(&mut self, _ctx: &EvalContext<'_>) -> StyledText {
        if self.cached.is_none() {
            let name = unistd::gethostname()
                .ok()
                .and_then(|s| s.into_string().ok())
                .unwrap_or_default();
            let body = if self.icon.is_empty() {
                name
            } else {
                format!("{} {name}", self.icon)
            };
            self.cached = Some(SmolStr::new(body));
        }
        // invariant: we just populated self.cached above if it was None
        let text = self.cached.clone().unwrap_or_default();
        StyledText::single(text, self.style)
    }
}

pub struct AttachedClientsWidget {
    pub style: ResolvedStyle,
    pub min_count: u8,
    pub icon: SmolStr,
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
        let body = format!("*{}", ctx.attached_clients);
        let text = if self.icon.is_empty() {
            body
        } else {
            format!("{} {body}", self.icon)
        };
        StyledText::single(SmolStr::new(text), self.style)
    }
}

pub struct PrefixIndicatorWidget {
    pub style: ResolvedStyle,
    pub content: SmolStr,
    pub icon: SmolStr,
}

#[async_trait]
impl Widget for PrefixIndicatorWidget {
    fn interval(&self) -> Option<Duration> {
        None
    }
    async fn evaluate(&mut self, ctx: &EvalContext<'_>) -> StyledText {
        // The prefix-pending cue is the most time-sensitive: when the prefix is
        // armed it must show even while COPY/SYNC/Z owns the segment, so it
        // composes (`SYNC·PFX`) rather than being masked. The tag is the
        // configured content (trimmed), so a rebind of the indicator text follows.
        let pfx = ctx.prefix_active;
        let tag = self.content.trim();
        if ctx.copy_mode_active {
            let t = if pfx { format!(" COPY·{tag} ") } else { " COPY ".to_string() };
            return StyledText::single_clickable(
                SmolStr::new(t),
                self.style,
                crate::ClickAction::ExitCopyMode,
            );
        }
        if ctx.sync_active {
            let t = if pfx { format!(" SYNC·{tag} ") } else { " SYNC ".to_string() };
            return StyledText::single_clickable(
                SmolStr::new(t),
                self.style,
                crate::ClickAction::ToggleSyncPanes,
            );
        }
        if ctx.zoom_active {
            let t = if pfx { format!(" Z·{tag} ") } else { " Z ".to_string() };
            return StyledText::single(SmolStr::new(t), self.style);
        }
        if !pfx {
            return StyledText::empty();
        }
        let text = if self.icon.is_empty() {
            self.content.clone()
        } else {
            SmolStr::new(format!("{} {}", self.icon, self.content))
        };
        StyledText::single(text, self.style)
    }
}

pub struct CwdWidget {
    pub style: ResolvedStyle,
    pub max_components: Option<u8>,
    pub icon: SmolStr,
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
        let body = if let Some(max) = self.max_components {
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
            icon: SmolStr::new(""),
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert_eq!(out.segments[0].text.as_str(), " main  ");
    }

    #[tokio::test]
    async fn session_widget_prefixes_icon() {
        let mut w = SessionWidget {
            style: ResolvedStyle::default(),
            pad_left: 0,
            pad_right: 0,
            icon: SmolStr::new("\u{25c6}"),
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert!(
            out.segments[0].text.as_str().starts_with("\u{25c6} "),
            "expected icon prefix: {:?}",
            out.segments[0].text.as_str()
        );
    }

    #[tokio::test]
    async fn attached_clients_hides_below_min() {
        let mut w = AttachedClientsWidget {
            style: ResolvedStyle::default(),
            min_count: 2,
            icon: SmolStr::new(""),
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert!(out.segments.is_empty());
    }

    #[tokio::test]
    async fn attached_clients_shows_at_min() {
        let mut w = AttachedClientsWidget {
            style: ResolvedStyle::default(),
            min_count: 2,
            icon: SmolStr::new(""),
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
            icon: SmolStr::new(""),
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
            icon: SmolStr::new(""),
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
            icon: SmolStr::new(""),
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert!(out.segments.is_empty());
    }

    #[tokio::test]
    async fn prefix_indicator_shown_when_active() {
        let mut w = PrefixIndicatorWidget {
            style: ResolvedStyle::default(),
            content: "PFX".into(),
            icon: SmolStr::new(""),
        };
        let ctx = EvalContext {
            prefix_active: true,
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), "PFX");
    }

    #[tokio::test]
    async fn prefix_indicator_composes_pfx_over_active_mode() {
        let mut w = PrefixIndicatorWidget {
            style: ResolvedStyle::default(),
            content: "PFX".into(),
            icon: SmolStr::new(""),
        };
        // SYNC active AND prefix armed: PFX must not be masked, it composes.
        let ctx = EvalContext {
            sync_active: true,
            prefix_active: true,
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), " SYNC·PFX ");
        // Zoom + prefix composes too.
        let ctx = EvalContext {
            zoom_active: true,
            prefix_active: true,
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), " Z·PFX ");
        // Copy mode + prefix composes too (the independent COPY arm).
        let ctx = EvalContext {
            copy_mode_active: true,
            prefix_active: true,
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), " COPY·PFX ");
        // Sync alone (no prefix) is unchanged.
        let ctx = EvalContext {
            sync_active: true,
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), " SYNC ");
    }

    #[tokio::test]
    async fn prefix_indicator_shows_copy_when_in_copy_mode() {
        let mut w = PrefixIndicatorWidget {
            style: ResolvedStyle::default(),
            content: "PFX".into(),
            icon: SmolStr::new(""),
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
            icon: SmolStr::new(""),
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
            icon: SmolStr::new(""),
        };
        let ctx = EvalContext {
            copy_mode_active: true,
            sync_active: true,
            ..ctx_empty()
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), " COPY ");
    }

    #[tokio::test]
    async fn prefix_indicator_shows_z_when_zoomed() {
        let mut w = PrefixIndicatorWidget {
            style: ResolvedStyle::default(),
            content: "PFX".into(),
            icon: SmolStr::new(""),
        };
        let ctx = EvalContext { zoom_active: true, ..ctx_empty() };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), " Z ");
    }

    #[tokio::test]
    async fn sync_beats_zoom_in_priority() {
        let mut w = PrefixIndicatorWidget {
            style: ResolvedStyle::default(),
            content: "PFX".into(),
            icon: SmolStr::new(""),
        };
        let ctx = EvalContext { sync_active: true, zoom_active: true, ..ctx_empty() };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments[0].text.as_str(), " SYNC ");
    }
}
