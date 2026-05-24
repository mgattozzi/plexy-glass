use crate::{ClickAction, ResolvedStyle};
use smol_str::SmolStr;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Segment {
    pub text: SmolStr,
    pub style: ResolvedStyle,
    /// If Some, this segment's painted columns become a click target. The
    /// daemon's status-bar click dispatcher consults the action and fires
    /// the matching command.
    pub click_action: Option<ClickAction>,
}

#[derive(Debug, Clone, Default)]
pub struct StyledText {
    pub segments: Vec<Segment>,
}

impl StyledText {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn single(text: impl Into<SmolStr>, style: ResolvedStyle) -> Self {
        Self {
            segments: vec![Segment {
                text: text.into(),
                style,
                click_action: None,
            }],
        }
    }

    pub fn single_clickable(
        text: impl Into<SmolStr>,
        style: ResolvedStyle,
        action: ClickAction,
    ) -> Self {
        Self {
            segments: vec![Segment {
                text: text.into(),
                style,
                click_action: Some(action),
            }],
        }
    }

    pub fn width(&self) -> usize {
        self.segments
            .iter()
            .map(|s| s.text.chars().count())
            .sum()
    }
}

#[async_trait::async_trait]
pub trait Widget: Send + Sync {
    fn interval(&self) -> Option<Duration>;
    async fn evaluate(&mut self, ctx: &crate::EvalContext<'_>) -> StyledText;
}
