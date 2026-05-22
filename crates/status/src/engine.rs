use crate::widget::{StyledText, Widget};
use crate::widgets::{
    AttachedClientsWidget, BatteryWidget, CpuLoadWidget, CwdWidget, GitBranchWidget,
    HostnameWidget, MemoryWidget, PrefixIndicatorWidget, SeparatorWidget, SessionWidget,
    ShellWidget, TextWidget, TimeWidget, WindowListWidget,
};
use crate::resolve_style;
use plexy_glass_config::{PaletteConfig, StatusConfig, WidgetSpec};
use smol_str::SmolStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct WindowSummary {
    pub name: String,
    pub active: bool,
}

pub struct EvalContext<'a> {
    pub session_name: &'a str,
    pub windows: &'a [WindowSummary],
    pub active_window: usize,
    pub attached_clients: u8,
    pub prefix_active: bool,
    pub active_pane_cwd: Option<&'a str>,
    pub copy_mode_active: bool,
}

#[allow(dead_code)] // Zone enum reserved for future per-zone APIs; not used yet.
pub enum Zone {
    Left,
    Middle,
    Right,
}

struct WidgetSlot {
    widget: Box<dyn Widget>,
    next_due: Option<Instant>,
    cached: StyledText,
}

pub struct StatusEngine {
    inner: Arc<EngineInner>,
}

pub struct EngineInner {
    left: Mutex<Vec<WidgetSlot>>,
    middle: Mutex<Vec<WidgetSlot>>,
    right: Mutex<Vec<WidgetSlot>>,
    refresh: Duration,
}

impl StatusEngine {
    pub fn new(cfg: &StatusConfig, palette: &PaletteConfig) -> Self {
        let left = cfg.left.iter().map(|s| build_slot(s, palette)).collect();
        let middle = cfg.middle.iter().map(|s| build_slot(s, palette)).collect();
        let right = cfg.right.iter().map(|s| build_slot(s, palette)).collect();
        Self {
            inner: Arc::new(EngineInner {
                left: Mutex::new(left),
                middle: Mutex::new(middle),
                right: Mutex::new(right),
                refresh: cfg.refresh,
            }),
        }
    }

    pub fn inner(&self) -> Arc<EngineInner> {
        Arc::clone(&self.inner)
    }
}

fn build_slot(spec: &WidgetSpec, palette: &PaletteConfig) -> WidgetSlot {
    let widget: Box<dyn Widget> = match spec {
        WidgetSpec::Session { style, padding } => Box::new(SessionWidget {
            style: resolve_style(style, palette),
            pad_left: padding.left,
            pad_right: padding.right,
        }),
        WidgetSpec::WindowList { active_style, inactive_style } => Box::new(WindowListWidget {
            active_style: resolve_style(active_style, palette),
            inactive_style: resolve_style(inactive_style, palette),
        }),
        WidgetSpec::PrefixIndicator { style, content } => Box::new(PrefixIndicatorWidget {
            style: resolve_style(style, palette),
            content: SmolStr::new(content),
        }),
        WidgetSpec::AttachedClients { style, min_count } => Box::new(AttachedClientsWidget {
            style: resolve_style(style, palette),
            min_count: *min_count,
        }),
        WidgetSpec::Time { format, interval, style } => Box::new(TimeWidget {
            format: format.clone(),
            interval: *interval,
            style: resolve_style(style, palette),
        }),
        WidgetSpec::Hostname { style, interval } => {
            Box::new(HostnameWidget::new(resolve_style(style, palette), *interval))
        }
        WidgetSpec::Cwd { style, max_components } => Box::new(CwdWidget {
            style: resolve_style(style, palette),
            max_components: *max_components,
        }),
        WidgetSpec::GitBranch { style, interval } => {
            Box::new(GitBranchWidget::new(resolve_style(style, palette), *interval))
        }
        WidgetSpec::Battery { style, interval } => Box::new(BatteryWidget {
            style: resolve_style(style, palette),
            interval: *interval,
        }),
        WidgetSpec::CpuLoad { style, interval } => Box::new(CpuLoadWidget {
            style: resolve_style(style, palette),
            interval: *interval,
        }),
        WidgetSpec::Memory { style, interval } => {
            Box::new(MemoryWidget::new(resolve_style(style, palette), *interval))
        }
        WidgetSpec::Text { value, style } => Box::new(TextWidget {
            text: SmolStr::new(value),
            style: resolve_style(style, palette),
        }),
        WidgetSpec::Separator { char, style } => Box::new(SeparatorWidget {
            ch: *char,
            style: resolve_style(style, palette),
        }),
        WidgetSpec::Shell { command, args, interval, timeout, style } => Box::new(ShellWidget {
            command: command.clone(),
            args: args.clone(),
            interval: *interval,
            timeout: *timeout,
            style: resolve_style(style, palette),
        }),
    };
    WidgetSlot {
        widget,
        next_due: None,
        cached: StyledText::empty(),
    }
}

/// Owned snapshot of session state suitable for evaluating widgets.
/// (We can't borrow across awaits, so the caller produces an owned
/// snapshot each tick.)
pub struct SnapshotCtx {
    pub session_name: String,
    pub windows: Vec<WindowSummary>,
    pub active_window: usize,
    pub attached_clients: u8,
    pub prefix_active: bool,
    pub active_pane_cwd: Option<String>,
    pub copy_mode_active: bool,
}

impl SnapshotCtx {
    pub fn as_eval_context(&self) -> EvalContext<'_> {
        EvalContext {
            session_name: &self.session_name,
            windows: &self.windows,
            active_window: self.active_window,
            attached_clients: self.attached_clients,
            prefix_active: self.prefix_active,
            active_pane_cwd: self.active_pane_cwd.as_deref(),
            copy_mode_active: self.copy_mode_active,
        }
    }
}

impl StatusEngine {
    /// Spawn a background tick task that refreshes interval-driven widgets.
    ///
    /// The task signals `notify.notify_one()` after each refresh batch so
    /// the render coordinator wakes up. The returned `JoinHandle` should
    /// be aborted by the owner on session shutdown.
    pub fn spawn_tick_task(
        &self,
        notify: std::sync::Arc<tokio::sync::Notify>,
        snapshot_ctx: impl Fn() -> SnapshotCtx + Send + Sync + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let inner = Arc::clone(&self.inner);
        let snapshot_ctx = std::sync::Arc::new(snapshot_ctx);
        tokio::spawn(async move {
            loop {
                let owned = (snapshot_ctx)();
                let ctx = owned.as_eval_context();
                let next_deadline = inner.refresh_due_intervals(&ctx).await;
                notify.notify_one();
                match next_deadline {
                    Some(deadline) => {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                    }
                    None => {
                        // No interval-driven widgets at all, so we sleep on the default refresh.
                        tokio::time::sleep(inner.refresh()).await;
                    }
                }
            }
        })
    }
}

impl EngineInner {
    /// Refresh ALL event-driven widgets in all three zones from `ctx`.
    pub async fn refresh_event_driven(&self, ctx: &EvalContext<'_>) {
        for zone in [&self.left, &self.middle, &self.right] {
            let mut slots = zone.lock().await;
            for slot in slots.iter_mut() {
                if slot.widget.interval().is_none() {
                    slot.cached = slot.widget.evaluate(ctx).await;
                }
            }
        }
    }

    /// Refresh due interval-driven widgets. Called from the tick task.
    /// Returns the earliest next deadline across all interval widgets,
    /// or `None` if there are no interval-driven widgets in the engine.
    pub async fn refresh_due_intervals(&self, ctx: &EvalContext<'_>) -> Option<Instant> {
        let now = Instant::now();
        let mut next_deadline: Option<Instant> = None;
        for zone in [&self.left, &self.middle, &self.right] {
            let mut slots = zone.lock().await;
            for slot in slots.iter_mut() {
                let Some(interval) = slot.widget.interval() else {
                    continue;
                };
                let due = slot.next_due.unwrap_or(now);
                if due <= now {
                    slot.cached = slot.widget.evaluate(ctx).await;
                    slot.next_due = Some(now + interval);
                }
                let nd = slot.next_due.unwrap_or(now + interval);
                next_deadline = Some(match next_deadline {
                    Some(prev) => prev.min(nd),
                    None => nd,
                });
            }
        }
        next_deadline
    }

    pub fn refresh(&self) -> Duration {
        self.refresh
    }

    /// Read the current cached segments (clones, since we don't want to
    /// hold the locks past return).
    pub async fn snapshot(&self) -> SegmentSnapshot {
        let left = self
            .left
            .lock()
            .await
            .iter()
            .map(|s| s.cached.segments.clone())
            .collect();
        let middle = self
            .middle
            .lock()
            .await
            .iter()
            .map(|s| s.cached.segments.clone())
            .collect();
        let right = self
            .right
            .lock()
            .await
            .iter()
            .map(|s| s.cached.segments.clone())
            .collect();
        SegmentSnapshot { left, middle, right }
    }
}

#[derive(Debug, Clone)]
pub struct SegmentSnapshot {
    pub left: Vec<Vec<crate::widget::Segment>>,
    pub middle: Vec<Vec<crate::widget::Segment>>,
    pub right: Vec<Vec<crate::widget::Segment>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_config::built_in_default;

    #[tokio::test]
    async fn engine_builds_from_default_config() {
        let cfg = built_in_default();
        let engine = StatusEngine::new(&cfg.status, &cfg.palette);
        let inner = engine.inner();
        let snap = inner.snapshot().await;
        assert_eq!(snap.left.len(), cfg.status.left.len());
        assert_eq!(snap.middle.len(), cfg.status.middle.len());
        assert_eq!(snap.right.len(), cfg.status.right.len());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tick_task_notifies_periodically() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut cfg = built_in_default();
        // Force a fast interval on the Time widget so the tick fires often.
        for w in cfg.status.right.iter_mut() {
            if let plexy_glass_config::WidgetSpec::Time { interval, .. } = w {
                *interval = Some(std::time::Duration::from_millis(100));
            }
        }
        let engine = StatusEngine::new(&cfg.status, &cfg.palette);
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let counter_inc = std::sync::Arc::clone(&counter);
        let n2 = std::sync::Arc::clone(&notify);
        tokio::spawn(async move {
            for _ in 0..10 {
                n2.notified().await;
                counter_inc.fetch_add(1, Ordering::SeqCst);
            }
        });
        let snapshot_ctx = || SnapshotCtx {
            session_name: "test".into(),
            windows: vec![],
            active_window: 0,
            attached_clients: 1,
            prefix_active: false,
            active_pane_cwd: None,
            copy_mode_active: false,
        };
        let handle = engine.spawn_tick_task(notify, snapshot_ctx);
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        handle.abort();
        assert!(
            counter.load(Ordering::SeqCst) >= 3,
            "expected at least 3 ticks, got {}",
            counter.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn engine_refreshes_event_driven_widgets() {
        let cfg = built_in_default();
        let engine = StatusEngine::new(&cfg.status, &cfg.palette);
        let inner = engine.inner();
        let ctx = EvalContext {
            session_name: "demo",
            windows: &[WindowSummary { name: "shell0".into(), active: true }],
            active_window: 0,
            attached_clients: 1,
            prefix_active: false,
            active_pane_cwd: None,
            copy_mode_active: false,
        };
        inner.refresh_event_driven(&ctx).await;
        let snap = inner.snapshot().await;
        // The session widget (first slot in left) should have a non-empty cache
        // containing "demo".
        assert!(!snap.left[0].is_empty());
        assert!(snap.left[0][0].text.contains("demo"));
    }
}
