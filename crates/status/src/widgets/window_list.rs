use std::time::Duration;

use async_trait::async_trait;
use smol_str::SmolStr;

use crate::{ClickAction, EvalContext, ResolvedStyle, Segment, StyledText, Widget};

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
            let style = if Some(i) == ctx.dragging_window {
                ResolvedStyle {
                    attrs: self.active_style.attrs | plexy_glass_emulator::Attrs::REVERSE,
                    ..self.active_style
                }
            } else if i == ctx.active_window {
                self.active_style
            } else {
                self.inactive_style
            };
            // tmux-style monitor flags after the name: `!` bell, `#` activity,
            // `✓`/`✗` command completion (ok/failed). Each glyph is display
            // width 1 (verified by `monitor_flags_are_single_width`).
            let mut flags = String::new();
            if w.bell {
                flags.push('!');
            }
            if w.activity {
                flags.push('#');
            }
            if w.silence {
                flags.push('~');
            }
            match w.done {
                Some(true) => flags.push('✓'),
                Some(false) => flags.push('✗'),
                None => {}
            }
            let label = format!(" {} {}{} ", i + 1, w.name, flags);
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
            WindowSummary {
                name: "shell0".into(),
                activity: false,
                bell: false,
                done: None,
                silence: false,
            },
            WindowSummary {
                name: "shell1".into(),
                activity: false,
                bell: false,
                done: None,
                silence: false,
            },
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
            dragging_window: None,
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
            WindowSummary {
                name: "alpha".into(),
                activity: false,
                bell: false,
                done: None,
                silence: false,
            },
            WindowSummary {
                name: "beta".into(),
                activity: false,
                bell: false,
                done: None,
                silence: false,
            },
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
            dragging_window: None,
        };
        let out = w.evaluate(&ctx).await;
        let actions: Vec<_> = out.segments.iter().filter_map(|s| s.click_action).collect();
        assert!(actions.contains(&ClickAction::SelectWindow(0)));
        assert!(actions.contains(&ClickAction::SelectWindow(1)));
    }

    #[tokio::test]
    async fn window_list_appends_monitor_flags() {
        let mut w = WindowListWidget {
            active_style: ResolvedStyle::default(),
            inactive_style: ResolvedStyle::default(),
        };
        let windows = vec![
            WindowSummary {
                name: "clean".into(),
                activity: false,
                bell: false,
                done: None,
                silence: false,
            },
            WindowSummary {
                name: "belled".into(),
                activity: false,
                bell: true,
                done: None,
                silence: false,
            },
            WindowSummary {
                name: "noisy".into(),
                activity: true,
                bell: true,
                done: None,
                silence: false,
            },
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
            dragging_window: None,
        };
        let out = w.evaluate(&ctx).await;
        assert!(
            !out.segments[0].text.contains('!') && !out.segments[0].text.contains('#'),
            "clean window has no flags"
        );
        assert!(out.segments[1].text.contains("belled!"), "bell → '!'");
        assert!(
            out.segments[2].text.contains("noisy!#"),
            "bell + activity → '!#'"
        );
    }

    #[tokio::test]
    async fn window_list_renders_done_flags() {
        let mut w = WindowListWidget {
            active_style: ResolvedStyle::default(),
            inactive_style: ResolvedStyle::default(),
        };
        let windows = vec![
            WindowSummary {
                name: "cur".into(),
                activity: false,
                bell: false,
                done: None,
                silence: false,
            },
            WindowSummary {
                name: "ok".into(),
                activity: false,
                bell: false,
                done: Some(true),
                silence: false,
            },
            WindowSummary {
                name: "bad".into(),
                activity: false,
                bell: false,
                done: Some(false),
                silence: false,
            },
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
            dragging_window: None,
        };
        let out = w.evaluate(&ctx).await;
        assert!(out.segments[1].text.contains("ok✓"), "exit-0 done → '✓'");
        assert!(out.segments[2].text.contains("bad✗"), "nonzero done → '✗'");
    }

    /// Every monitor flag glyph (`#`/`!`/`✓`/`✗`/`~`) is display width 1, so the
    /// window-list segment width is the name plus one column per flag, and the
    /// layout math (and the truncation in the compositor) assumes this.
    #[test]
    fn monitor_flags_are_single_width() {
        for g in ['#', '!', '✓', '✗', '~'] {
            assert_eq!(
                plexy_glass_emulator::display_width(&g.to_string()),
                1,
                "flag glyph {g:?} must be display width 1"
            );
        }
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
            dragging_window: None,
        };
        let out = w.evaluate(&ctx).await;
        assert!(out.segments.is_empty());
    }

    #[tokio::test]
    async fn dragging_window_segment_is_reversed() {
        use plexy_glass_emulator::Attrs;
        let active = ResolvedStyle {
            fg: None,
            bg: None,
            attrs: Attrs::BOLD,
        };
        let inactive = ResolvedStyle::default();
        let mut w = WindowListWidget {
            active_style: active,
            inactive_style: inactive,
        };
        let windows = vec![
            WindowSummary {
                name: "a".into(),
                activity: false,
                bell: false,
                done: None,
                silence: false,
            },
            WindowSummary {
                name: "b".into(),
                activity: false,
                bell: false,
                done: None,
                silence: false,
            },
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
            dragging_window: Some(1),
        };
        let out = w.evaluate(&ctx).await;
        assert!(
            out.segments[1].style.attrs.contains(Attrs::REVERSE),
            "dragged tab reversed"
        );
        assert!(
            !out.segments[0].style.attrs.contains(Attrs::REVERSE),
            "non-dragged not reversed"
        );
    }
}
