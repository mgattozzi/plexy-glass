use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use smol_str::SmolStr;
use tokio::process::Command;
use tokio::time;

use crate::{EvalContext, ResolvedStyle, StyledText, Widget};

pub struct GitBranchWidget {
    pub style: ResolvedStyle,
    pub interval: Option<Duration>,
    pub icon: SmolStr,
}

impl GitBranchWidget {
    pub const fn new(style: ResolvedStyle, interval: Option<Duration>, icon: SmolStr) -> Self {
        Self {
            style,
            interval,
            icon,
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
        // No internal cwd-keyed cache: git-branch always declares an interval,
        // so the engine only re-evaluates it on that schedule (default 5s). A
        // cwd-keyed cache pinned the branch forever and missed a `git checkout`
        // in the same directory (the most common case). The interval is the
        // throttle; this just runs git when asked, like the other interval
        // widgets (memory/cpu/battery).
        //
        // Bounded + kill_on_drop: the render coordinator awaits widget
        // evaluation while holding the window-manager lock, so a hung `git`
        // (dead network mount, wedged fsmonitor) would wedge every keystroke,
        // resize, and teardown in the session indefinitely. 2s matches
        // `read_clipboard`'s bound; on timeout the dropped future kills the
        // child and we render empty, same as a non-repo cwd.
        let fut = Command::new("git")
            .args(["-C", path, "symbolic-ref", "--short", "HEAD"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = time::timeout(Duration::from_secs(2), fut).await;
        match output {
            Ok(Ok(o)) if o.status.success() => {
                let branch_raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let text = if self.icon.is_empty() {
                    SmolStr::new(format!(" {branch_raw}"))
                } else {
                    SmolStr::new(format!("{} {branch_raw}", self.icon))
                };
                StyledText::single(text, self.style)
            }
            _ => StyledText::empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;
    use std::{env, fs, process};

    use super::*;

    fn ctx_with_cwd(cwd: &str) -> EvalContext<'_> {
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
            dragging_window: None,
            remote: false,
        }
    }

    #[tokio::test]
    async fn non_repo_dir_renders_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap().to_string();
        let mut w = GitBranchWidget::new(ResolvedStyle::default(), None, SmolStr::new(""));
        let out = w.evaluate(&ctx_with_cwd(&cwd)).await;
        assert!(out.segments.is_empty());
    }

    #[tokio::test]
    async fn branch_change_in_same_cwd_is_reflected() {
        // Regression: the old cwd-keyed cache pinned the branch forever, so a
        // `git checkout` in the same directory showed the stale branch. With the
        // cache gone, a second evaluation of the same cwd reports the new branch.
        if process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // needs a real git; skip where unavailable
        }
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap().to_string();
        let git = |args: &[&str]| {
            process::Command::new("git")
                .args(["-C", &cwd])
                .args(args)
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        assert!(
            git(&["commit", "-q", "--allow-empty", "-m", "init"])
                .status
                .success()
        );

        let mut w = GitBranchWidget::new(ResolvedStyle::default(), None, SmolStr::new(""));
        let first = w.evaluate(&ctx_with_cwd(&cwd)).await;
        let first_branch = first.segments[0].text.as_str().trim().to_string();
        assert!(
            !first_branch.is_empty(),
            "expected a branch on the fresh repo"
        );

        assert!(
            git(&["checkout", "-q", "-b", "feature-xyz"])
                .status
                .success()
        );
        let second = w.evaluate(&ctx_with_cwd(&cwd)).await;
        assert_eq!(second.segments[0].text.as_str().trim(), "feature-xyz");
        assert_ne!(first_branch, "feature-xyz");
    }

    #[tokio::test]
    async fn hung_git_is_bounded_and_renders_empty() {
        use std::os::unix::fs::PermissionsExt;

        // A stub `git` that hangs far past the widget's 2s bound. `exec` so
        // the sleeper IS the direct child and kill_on_drop reaps it (a forked
        // grandchild would survive the kill and linger as a test artifact).
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("git");
        fs::write(&stub, "#!/bin/sh\nexec sleep 10\n").unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();

        let old_path = env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{old_path}", dir.path().display());
        // SAFETY: nextest runs each test in its own process. Under plain
        // `cargo test` the only cross-test effect is an extra leading PATH
        // entry containing nothing but `git`, which no sibling test resolves.
        unsafe { env::set_var("PATH", &new_path) };

        let cwd = dir.path().to_str().unwrap().to_string();
        let mut w = GitBranchWidget::new(ResolvedStyle::default(), None, SmolStr::new(""));
        let start = Instant::now();
        let out = w.evaluate(&ctx_with_cwd(&cwd)).await;
        let elapsed = start.elapsed();

        assert!(out.segments.is_empty());
        assert!(
            elapsed < Duration::from_secs(4),
            "evaluation must be bounded by the 2s timeout, took {elapsed:?}"
        );
    }
}
