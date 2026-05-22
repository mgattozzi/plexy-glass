use crate::{EvalContext, ResolvedStyle, StyledText, Widget};
use async_trait::async_trait;
use smol_str::SmolStr;
use std::time::Duration;

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
        let mut cmd = tokio::process::Command::new(&self.command);
        cmd.args(&self.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let result = tokio::time::timeout(self.timeout, cmd.output()).await;
        let text = match result {
            Ok(Ok(o)) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let truncated: String = s.chars().take(OUTPUT_CAP).collect();
                truncated
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
