use crate::{EvalContext, ResolvedStyle, StyledText, Widget};
use async_trait::async_trait;
use smol_str::SmolStr;
use std::time::Duration;

pub struct CpuLoadWidget {
    pub style: ResolvedStyle,
    pub interval: Option<Duration>,
}

#[async_trait]
impl Widget for CpuLoadWidget {
    fn interval(&self) -> Option<Duration> {
        self.interval.or(Some(Duration::from_secs(5)))
    }
    async fn evaluate(&mut self, _ctx: &EvalContext<'_>) -> StyledText {
        let load = sysinfo::System::load_average();
        let text = format!("{:.2}", load.one);
        StyledText::single(SmolStr::new(text), self.style)
    }
}

pub struct MemoryWidget {
    pub style: ResolvedStyle,
    pub interval: Option<Duration>,
    system: sysinfo::System,
}

impl MemoryWidget {
    pub fn new(style: ResolvedStyle, interval: Option<Duration>) -> Self {
        Self {
            style,
            interval,
            system: sysinfo::System::new(),
        }
    }
}

#[async_trait]
impl Widget for MemoryWidget {
    fn interval(&self) -> Option<Duration> {
        self.interval.or(Some(Duration::from_secs(5)))
    }
    async fn evaluate(&mut self, _ctx: &EvalContext<'_>) -> StyledText {
        self.system.refresh_memory();
        let total = self.system.total_memory();
        let used = total.saturating_sub(self.system.available_memory());
        let pct = if total > 0 {
            let raw = used as f64 / total as f64 * 100.0;
            raw.clamp(0.0, 100.0) as u32
        } else {
            0
        };
        let text = format!("{pct}%");
        StyledText::single(SmolStr::new(text), self.style)
    }
}

pub struct BatteryWidget {
    pub style: ResolvedStyle,
    pub interval: Option<Duration>,
}

#[async_trait]
impl Widget for BatteryWidget {
    fn interval(&self) -> Option<Duration> {
        self.interval.or(Some(Duration::from_secs(30)))
    }
    async fn evaluate(&mut self, _ctx: &EvalContext<'_>) -> StyledText {
        let Ok(manager) = battery::Manager::new() else {
            return StyledText::empty();
        };
        let Ok(batteries) = manager.batteries() else {
            return StyledText::empty();
        };
        let mut first = None;
        for b in batteries.flatten().take(1) {
            first = Some(b);
        }
        let Some(b) = first else {
            return StyledText::empty();
        };
        let pct = b
            .state_of_charge()
            .get::<battery::units::ratio::percent>();
        let icon = match b.state() {
            battery::State::Charging => "⚡",
            battery::State::Discharging
            | battery::State::Empty
            | battery::State::Full
            | battery::State::Unknown => "🔋",
            // invariant: __Nonexhaustive is a hidden doc variant that can never be constructed
            _ => "🔋",
        };
        let text = format!("{icon}{pct:.0}%");
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
        }
    }

    #[tokio::test]
    async fn cpu_load_emits_a_number() {
        let mut w = CpuLoadWidget {
            style: ResolvedStyle::default(),
            interval: None,
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert!(!out.segments.is_empty());
        let txt = out.segments[0].text.as_str();
        // Should be a float like "1.23"
        assert!(txt.parse::<f64>().is_ok(), "expected numeric load, got {txt}");
    }

    #[tokio::test]
    async fn memory_widget_emits_percentage() {
        let mut w = MemoryWidget::new(ResolvedStyle::default(), None);
        let out = w.evaluate(&ctx_empty()).await;
        assert!(out.segments[0].text.ends_with('%'));
    }

    #[tokio::test]
    async fn battery_widget_does_not_panic() {
        // Result depends on the host; we just verify it returns without panicking.
        let mut w = BatteryWidget {
            style: ResolvedStyle::default(),
            interval: None,
        };
        let _ = w.evaluate(&ctx_empty()).await;
    }
}
