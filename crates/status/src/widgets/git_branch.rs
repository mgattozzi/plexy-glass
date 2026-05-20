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
        let output = tokio::process::Command::new("git")
            .args(["-C", path, "symbolic-ref", "--short", "HEAD"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await;
        self.cached_cwd = Some(path.to_string());
        match output {
            Ok(o) if o.status.success() => {
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
