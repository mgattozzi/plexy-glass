use crate::{EvalContext, ResolvedStyle, StyledText, Widget};
use async_trait::async_trait;
use smol_str::SmolStr;
use std::time::Duration;

pub struct GitBranchWidget {
    pub style: ResolvedStyle,
    pub interval: Option<Duration>,
    cached_cwd: Option<String>,
    cached_branch: Option<SmolStr>,
}

impl GitBranchWidget {
    pub fn new(style: ResolvedStyle, interval: Option<Duration>) -> Self {
        Self {
            style,
            interval,
            cached_cwd: None,
            cached_branch: None,
        }
    }
}

#[async_trait]
impl Widget for GitBranchWidget {
    fn interval(&self) -> Option<Duration> {
        self.interval.or(Some(Duration::from_secs(5)))
    }
    async fn evaluate(&mut self, ctx: &EvalContext<'_>) -> StyledText {
        let Some(url) = ctx.active_pane_cwd else {
            return StyledText::empty();
        };
        let path = match url.strip_prefix("file://") {
            Some(rest) => match rest.find('/') {
                Some(i) => &rest[i..],
                None => return StyledText::empty(),
            },
            None => url,
        };
        if self.cached_cwd.as_deref() == Some(path) {
            if let Some(branch) = &self.cached_branch {
                return StyledText::single(branch.clone(), self.style);
            }
            return StyledText::empty();
        }
        // Bounded + kill_on_drop: the render coordinator awaits widget
        // evaluation while holding the window-manager lock, so a hung `git`
        // (dead network mount, wedged fsmonitor) would wedge every keystroke,
        // resize, and teardown in the session indefinitely. 2s matches
        // `read_clipboard`'s bound; on timeout the dropped future kills the
        // child and we render empty, same as a non-repo cwd.
        let fut = tokio::process::Command::new("git")
            .args(["-C", path, "symbolic-ref", "--short", "HEAD"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(Duration::from_secs(2), fut).await;
        self.cached_cwd = Some(path.to_string());
        match output {
            Ok(Ok(o)) if o.status.success() => {
                let branch_raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let branch = SmolStr::new(format!(" {branch_raw}"));
                self.cached_branch = Some(branch.clone());
                StyledText::single(branch, self.style)
            }
            _ => {
                self.cached_branch = None;
                StyledText::empty()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_cwd<'a>(cwd: &'a str) -> EvalContext<'a> {
        EvalContext {
            session_name: "main",
            windows: &[],
            active_window: 0,
            attached_clients: 1,
            prefix_active: false,
            active_pane_cwd: Some(cwd),
            copy_mode_active: false,
            sync_active: false,
            zoom_active: false,
        }
    }

    #[tokio::test]
    async fn non_repo_dir_renders_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap().to_string();
        let mut w = GitBranchWidget::new(ResolvedStyle::default(), None);
        let out = w.evaluate(&ctx_with_cwd(&cwd)).await;
        assert!(out.segments.is_empty());
    }

    #[tokio::test]
    async fn hung_git_is_bounded_and_renders_empty() {
        use std::os::unix::fs::PermissionsExt;

        // A stub `git` that hangs far past the widget's 2s bound. `exec` so
        // the sleeper IS the direct child and kill_on_drop reaps it (a forked
        // grandchild would survive the kill and linger as a test artifact).
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("git");
        std::fs::write(&stub, "#!/bin/sh\nexec sleep 10\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let old_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{old_path}", dir.path().display());
        // SAFETY: nextest runs each test in its own process. Under plain
        // `cargo test` the only cross-test effect is an extra leading PATH
        // entry containing nothing but `git`, which no sibling test resolves.
        unsafe { std::env::set_var("PATH", &new_path) };

        let cwd = dir.path().to_str().unwrap().to_string();
        let mut w = GitBranchWidget::new(ResolvedStyle::default(), None);
        let start = std::time::Instant::now();
        let out = w.evaluate(&ctx_with_cwd(&cwd)).await;
        let elapsed = start.elapsed();

        assert!(out.segments.is_empty());
        assert!(
            elapsed < Duration::from_secs(4),
            "evaluation must be bounded by the 2s timeout, took {elapsed:?}"
        );
    }
}
