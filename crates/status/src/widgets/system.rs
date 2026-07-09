use std::time::Duration;

use async_trait::async_trait;
use battery::units::ratio::percent;
use smol_str::SmolStr;

use crate::{EvalContext, ResolvedStyle, StyledText, Widget};

pub struct CpuLoadWidget {
    pub style: ResolvedStyle,
    pub interval: Option<Duration>,
    pub icon: SmolStr,
}

#[async_trait]
impl Widget for CpuLoadWidget {
    fn interval(&self) -> Option<Duration> {
        self.interval.or(Some(Duration::from_secs(5)))
    }
    async fn evaluate(&mut self, _ctx: &EvalContext<'_>) -> StyledText {
        let load = sysinfo::System::load_average();
        let body = format!("{:.2}%", load.one);
        let text = if self.icon.is_empty() {
            body
        } else {
            format!("{} {body}", self.icon)
        };
        StyledText::single(SmolStr::new(text), self.style)
    }
}

pub struct MemoryWidget {
    pub style: ResolvedStyle,
    pub interval: Option<Duration>,
    pub icon: SmolStr,
    system: sysinfo::System,
}

impl MemoryWidget {
    pub fn new(style: ResolvedStyle, interval: Option<Duration>, icon: SmolStr) -> Self {
        Self {
            style,
            interval,
            icon,
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
        let body = format!("{pct}%");
        let text = if self.icon.is_empty() {
            body
        } else {
            format!("{} {body}", self.icon)
        };
        StyledText::single(SmolStr::new(text), self.style)
    }
}

pub struct BatteryWidget {
    pub style: ResolvedStyle,
    pub interval: Option<Duration>,
    pub icon: SmolStr,
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
        let Some(b) = batteries.flatten().next() else {
            return StyledText::empty();
        };
        let pct = b.state_of_charge().get::<percent>();
        // Plain ASCII prefix. The compositor's status painter walks chars
        // 1:1 against terminal cells and doesn't reserve a spacer for
        // wide characters, so a "🔋" or "⚡" here would garble subsequent
        // cells in the right zone.
        let charge_prefix = match b.state() {
            battery::State::Charging => "+",
            _ => "",
        };
        let body = format!("{charge_prefix}BAT {pct:.0}%");
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
            remote: false,
        }
    }

    #[tokio::test]
    async fn cpu_load_emits_a_number_with_percent() {
        let mut w = CpuLoadWidget {
            style: ResolvedStyle::default(),
            interval: None,
            icon: SmolStr::new(""),
        };
        let out = w.evaluate(&ctx_empty()).await;
        assert!(!out.segments.is_empty());
        let txt = out.segments[0].text.as_str();
        // Should be a float like "1.23%", so strip the trailing '%' and parse.
        let numeric = txt.strip_suffix('%').unwrap_or(txt);
        assert!(
            numeric.parse::<f64>().is_ok(),
            "expected numeric load with % suffix, got {txt}"
        );
        assert!(txt.ends_with('%'), "expected % suffix, got {txt}");
    }

    #[tokio::test]
    async fn memory_widget_emits_percentage() {
        let mut w = MemoryWidget::new(ResolvedStyle::default(), None, SmolStr::new(""));
        let out = w.evaluate(&ctx_empty()).await;
        assert!(out.segments[0].text.ends_with('%'));
    }

    #[tokio::test]
    async fn battery_widget_does_not_panic() {
        // Result depends on the host; we just verify it returns without panicking.
        let mut w = BatteryWidget {
            style: ResolvedStyle::default(),
            interval: None,
            icon: SmolStr::new(""),
        };
        let _ = w.evaluate(&ctx_empty()).await;
    }
}
