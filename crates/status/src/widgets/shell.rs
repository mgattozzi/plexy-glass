use crate::{EvalContext, ResolvedStyle, StyledText, Widget};
use async_trait::async_trait;
use smol_str::SmolStr;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time;

const OUTPUT_CAP: usize = 200;

pub struct ShellWidget {
    pub command: String,
    pub args: Vec<String>,
    pub interval: Option<Duration>,
    pub timeout: Duration,
    pub style: ResolvedStyle,
}

#[async_trait]
impl Widget for ShellWidget {
    fn interval(&self) -> Option<Duration> {
        self.interval.or(Some(Duration::from_secs(5)))
    }
    async fn evaluate(&mut self, _ctx: &EvalContext<'_>) -> StyledText {
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // kill_on_drop: on timeout the `output()` future is dropped, but
            // without this the child lives on (reparented to PID 1, holding
            // its stdout pipe), and a hanging command respawns every refresh
            // interval, accumulating orphan processes without bound.
            .kill_on_drop(true);

        let result = time::timeout(self.timeout, cmd.output()).await;
        let text = match result {
            Ok(Ok(o)) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                // Cap the widget's visual width (display columns), grapheme-safe.
                plexy_glass_emulator::truncate_to_width(&s, OUTPUT_CAP as u16).to_string()
            }
            _ => "\u{2026}".to_string(),
        };
        if text.is_empty() {
            return StyledText::empty();
        }
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
    async fn shell_widget_runs_command() {
        let mut w = ShellWidget {
            command: "echo".to_string(),
            args: vec!["hello-shell".to_string()],
            interval: None,
            timeout: Duration::from_secs(2),
            style: ResolvedStyle::default(),
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert_eq!(out.segments[0].text.as_str(), "hello-shell");
    }

    #[tokio::test]
    async fn shell_widget_timeout_emits_ellipsis() {
        let mut w = ShellWidget {
            command: "sleep".to_string(),
            args: vec!["5".to_string()],
            interval: None,
            timeout: Duration::from_millis(100),
            style: ResolvedStyle::default(),
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert_eq!(out.segments[0].text.as_str(), "\u{2026}");
    }

    #[tokio::test]
    async fn shell_widget_missing_command_emits_ellipsis() {
        let mut w = ShellWidget {
            command: "this-command-does-not-exist-67890".to_string(),
            args: vec![],
            interval: None,
            timeout: Duration::from_secs(1),
            style: ResolvedStyle::default(),
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert_eq!(out.segments[0].text.as_str(), "\u{2026}");
    }
}
