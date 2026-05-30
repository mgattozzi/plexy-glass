use crate::{ClickAction, EvalContext, ResolvedStyle, Segment, StyledText, Widget};
use async_trait::async_trait;
use smol_str::SmolStr;
use std::time::Duration;

pub struct WindowListWidget {
    pub active_style: ResolvedStyle,
    pub inactive_style: ResolvedStyle,
}

#[async_trait]
impl Widget for WindowListWidget {
    fn interval(&self) -> Option<Duration> {
        None
    }
    async fn evaluate(&mut self, ctx: &EvalContext<'_>) -> StyledText {
        let mut segments = Vec::with_capacity(ctx.windows.len());
        for (i, w) in ctx.windows.iter().enumerate() {
            let style = if i == ctx.active_window {
                self.active_style
            } else {
                self.inactive_style
            };
            let label = format!(" {} {} ", i + 1, w.name);
            segments.push(Segment {
                text: SmolStr::new(label),
                style,
                click_action: Some(ClickAction::SelectWindow(i)),
            });
        }
        StyledText { segments }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WindowSummary;

    #[tokio::test]
    async fn window_list_emits_one_segment_per_window() {
        let mut w = WindowListWidget {
            active_style: ResolvedStyle::default(),
            inactive_style: ResolvedStyle::default(),
        };
        let windows = vec![
            WindowSummary { name: "shell0".into(), active: true },
            WindowSummary { name: "shell1".into(), active: false },
        ];
        let ctx = EvalContext {
            session_name: "main",
            windows: &windows,
            active_window: 0,
            attached_clients: 1,
            prefix_active: false,
            active_pane_cwd: None,
            copy_mode_active: false,
            sync_active: false,
            zoom_active: false,
        };
        let out = w.evaluate(&ctx).await;
        assert_eq!(out.segments.len(), 2);
        assert!(out.segments[0].text.contains("shell0"));
        assert!(out.segments[1].text.contains("shell1"));
    }

    #[tokio::test]
    async fn window_list_emits_select_window_actions_per_window() {
        let mut w = WindowListWidget {
            active_style: ResolvedStyle::default(),
            inactive_style: ResolvedStyle::default(),
        };
        let windows = vec![
            WindowSummary { name: "alpha".into(), active: true },
            WindowSummary { name: "beta".into(), active: false },
        ];
        let ctx = EvalContext {
            session_name: "main",
            windows: &windows,
            active_window: 0,
            attached_clients: 1,
            prefix_active: false,
            active_pane_cwd: None,
            copy_mode_active: false,
            sync_active: false,
            zoom_active: false,
        };
        let out = w.evaluate(&ctx).await;
        let actions: Vec<_> = out.segments.iter().filter_map(|s| s.click_action).collect();
        assert!(actions.contains(&ClickAction::SelectWindow(0)));
        assert!(actions.contains(&ClickAction::SelectWindow(1)));
    }

    #[tokio::test]
    async fn window_list_empty_when_no_windows() {
        let mut w = WindowListWidget {
            active_style: ResolvedStyle::default(),
            inactive_style: ResolvedStyle::default(),
        };
        let ctx = EvalContext {
            session_name: "main",
            windows: &[],
            active_window: 0,
            attached_clients: 1,
            prefix_active: false,
            active_pane_cwd: None,
            copy_mode_active: false,
            sync_active: false,
            zoom_active: false,
        };
        let out = w.evaluate(&ctx).await;
        assert!(out.segments.is_empty());
    }
}
