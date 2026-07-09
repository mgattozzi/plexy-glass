use std::future::Future;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plexy_glass_config::{PaletteConfig, StatusConfig, WidgetSpec};
use smol_str::SmolStr;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time;

use crate::widget::{Segment, StyledText, Widget};
use crate::widgets::{
    AttachedClientsWidget, BatteryWidget, CpuLoadWidget, CwdWidget, GitBranchWidget,
    HostnameWidget, MemoryWidget, PrefixIndicatorWidget, SeparatorWidget, SessionWidget,
    ShellWidget, SshWidget, TextWidget, TimeWidget, WindowListWidget,
};
use crate::{GlyphSet, resolve_style};

#[derive(Debug, Clone)]
pub struct WindowSummary {
    pub name: String,
    /// Sticky monitor flags (tmux's `#`/`!`): set when a background window had
    /// activity / a bell. The current window's flags are always cleared upstream,
    /// so a marker never shows on it.
    pub activity: bool,
    pub bell: bool,
    /// Sticky command-completion flag: `Some(true)` → `✓` (exit 0 / codeless),
    /// `Some(false)` → `✗` (nonzero exit), `None` → no flag. Cleared upstream
    /// when the window becomes current, like activity/bell.
    pub done: Option<bool>,
    /// Sticky silence flag (`~`): set when a monitored background window
    /// produced no output for its silence threshold. Cleared on view.
    pub silence: bool,
}

pub struct EvalContext<'a> {
    pub session_name: &'a str,
    pub windows: &'a [WindowSummary],
    pub active_window: usize,
    pub attached_clients: u8,
    pub prefix_active: bool,
    pub active_pane_cwd: Option<&'a str>,
    pub copy_mode_active: bool,
    pub sync_active: bool,
    pub zoom_active: bool,
    /// Index of the window currently being drag-reordered, if any. The window
    /// list renders it with a distinct (reversed) style.
    pub dragging_window: Option<usize>,
    /// The attached client reached the daemon over `-H`/SSH (session-level
    /// aggregate). Drives the `ssh` marker widget.
    pub remote: bool,
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
    pub fn new(cfg: &StatusConfig, palette: &PaletteConfig, glyphs: &GlyphSet) -> Self {
        let left = cfg
            .left
            .iter()
            .map(|s| build_slot(s, palette, glyphs))
            .collect();
        let middle = cfg
            .middle
            .iter()
            .map(|s| build_slot(s, palette, glyphs))
            .collect();
        let right = cfg
            .right
            .iter()
            .map(|s| build_slot(s, palette, glyphs))
            .collect();
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

fn build_slot(spec: &WidgetSpec, palette: &PaletteConfig, glyphs: &GlyphSet) -> WidgetSlot {
    let widget: Box<dyn Widget> = match spec {
        WidgetSpec::Session { style, padding } => Box::new(SessionWidget {
            style: resolve_style(style, palette),
            pad_left: padding.left,
            pad_right: padding.right,
            icon: SmolStr::new(glyphs.session),
        }),
        WidgetSpec::WindowList {
            active_style,
            inactive_style,
        } => Box::new(WindowListWidget {
            active_style: resolve_style(active_style, palette),
            inactive_style: resolve_style(inactive_style, palette),
        }),
        WidgetSpec::PrefixIndicator { style, content } => Box::new(PrefixIndicatorWidget {
            style: resolve_style(style, palette),
            content: SmolStr::new(content),
            icon: SmolStr::new(glyphs.prefix),
        }),
        WidgetSpec::Ssh { style, content } => Box::new(SshWidget {
            style: resolve_style(style, palette),
            content: SmolStr::new(content),
        }),
        WidgetSpec::AttachedClients { style, min_count } => Box::new(AttachedClientsWidget {
            style: resolve_style(style, palette),
            min_count: *min_count,
            icon: SmolStr::new(glyphs.clients),
        }),
        WidgetSpec::Time {
            format,
            interval,
            style,
            utc,
        } => Box::new(TimeWidget {
            format: format.clone(),
            interval: *interval,
            style: resolve_style(style, palette),
            icon: SmolStr::new(glyphs.clock),
            utc: *utc,
        }),
        WidgetSpec::Hostname { style, interval } => Box::new(HostnameWidget::new(
            resolve_style(style, palette),
            *interval,
            SmolStr::new(glyphs.host),
        )),
        WidgetSpec::Cwd {
            style,
            max_components,
        } => Box::new(CwdWidget {
            style: resolve_style(style, palette),
            max_components: *max_components,
            icon: SmolStr::new(glyphs.cwd),
        }),
        WidgetSpec::GitBranch { style, interval } => Box::new(GitBranchWidget::new(
            resolve_style(style, palette),
            *interval,
            SmolStr::new(glyphs.git_branch),
        )),
        WidgetSpec::Battery { style, interval } => Box::new(BatteryWidget {
            style: resolve_style(style, palette),
            interval: *interval,
            icon: SmolStr::new(glyphs.battery),
        }),
        WidgetSpec::CpuLoad { style, interval } => Box::new(CpuLoadWidget {
            style: resolve_style(style, palette),
            interval: *interval,
            icon: SmolStr::new(glyphs.cpu),
        }),
        WidgetSpec::Memory { style, interval } => Box::new(MemoryWidget::new(
            resolve_style(style, palette),
            *interval,
            SmolStr::new(glyphs.mem),
        )),
        WidgetSpec::Text { value, style } => Box::new(TextWidget {
            text: SmolStr::new(value),
            style: resolve_style(style, palette),
        }),
        WidgetSpec::Separator { char, style } => Box::new(SeparatorWidget {
            ch: *char,
            style: resolve_style(style, palette),
        }),
        WidgetSpec::Shell {
            command,
            args,
            interval,
            timeout,
            style,
        } => Box::new(ShellWidget {
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
    pub sync_active: bool,
    pub zoom_active: bool,
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
            sync_active: self.sync_active,
            zoom_active: self.zoom_active,
            dragging_window: None,
            remote: false,
        }
    }
}

impl StatusEngine {
    /// Spawn a background tick task that refreshes interval-driven widgets.
    ///
    /// The task signals `notify.notify_one()` after each refresh batch so
    /// the render coordinator wakes up. The returned `JoinHandle` should
    /// be aborted by the owner on session shutdown.
    pub fn spawn_tick_task<F, Fut>(&self, notify: Arc<Notify>, snapshot_ctx: F) -> JoinHandle<()>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = SnapshotCtx> + Send,
    {
        let inner = Arc::clone(&self.inner);
        let snapshot_ctx = Arc::new(snapshot_ctx);
        tokio::spawn(async move {
            loop {
                // Awaited (not a blocking call): the snapshot closure may take
                // async locks, and this task runs on a runtime worker thread
                // where blocking would panic.
                let owned = (snapshot_ctx)().await;
                let ctx = owned.as_eval_context();
                let next_deadline = inner.refresh_due_intervals(&ctx).await;
                notify.notify_one();
                match next_deadline {
                    Some(deadline) => {
                        time::sleep_until(time::Instant::from_std(deadline)).await;
                    }
                    None => {
                        // No interval-driven widgets at all, so we sleep on the default refresh.
                        time::sleep(inner.refresh()).await;
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

    pub const fn refresh(&self) -> Duration {
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
        SegmentSnapshot {
            left,
            middle,
            right,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SegmentSnapshot {
    pub left: Vec<Vec<Segment>>,
    pub middle: Vec<Vec<Segment>>,
    pub right: Vec<Vec<Segment>>,
}

impl SegmentSnapshot {
    /// Iterate every segment across all three zones in paint order.
    pub fn iter_segments(&self) -> impl Iterator<Item = &Segment> {
        self.left
            .iter()
            .chain(self.middle.iter())
            .chain(self.right.iter())
            .flat_map(|widget_segments| widget_segments.iter())
    }

    /// Build a flat list of clickable col-range → action regions.
    ///
    /// Assumes segments are painted contiguously starting at column 0. The render
    /// coordinator translates the ranges to viewport-absolute columns when it paints
    /// each zone (zones don't always start at col 0), so until per-zone hit tables
    /// are wired, callers can use this as a rough cut.
    pub fn click_hits(&self) -> Vec<StatusHit> {
        let mut out = Vec::new();
        let mut col: u16 = 0;
        for seg in self.iter_segments() {
            // Display width keeps click ranges aligned with how the compositor
            // paints wide graphemes (CJK window names, emoji, …).
            let width = plexy_glass_emulator::display_width(&seg.text);
            if let Some(action) = seg.click_action {
                out.push(StatusHit {
                    // saturating_add to match the accumulation below, so both column
                    // computations share one overflow policy.
                    col_range: col..col.saturating_add(width),
                    action,
                });
            }
            col = col.saturating_add(width);
        }
        out
    }
}

/// What command a click on a status-bar segment should fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickAction {
    SelectWindow(usize),
    ToggleSyncPanes,
    ExitCopyMode,
    Detach,
    NoOp,
}

/// One clickable region in the rendered status bar: column range + action.
///
/// Computed by the render coordinator from a `SegmentSnapshot` and pushed to
/// `WindowManager::set_status_hits` so click dispatch can binary-search by
/// column. Note that column ranges are zone-relative; the render coordinator
/// translates to viewport-absolute columns when it paints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusHit {
    pub col_range: Range<u16>,
    pub action: ClickAction,
}

#[cfg(test)]
mod tests {
    use plexy_glass_config::built_in_default;

    use super::*;
    use crate::GlyphSet;

    #[tokio::test]
    async fn engine_builds_from_default_config() {
        let cfg = built_in_default();
        let engine = StatusEngine::new(&cfg.status, &cfg.palette, &GlyphSet::UNICODE);
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
        // Force a fast interval on an interval-driven widget so the tick fires
        // often (the default right cluster has CpuLoad, which carries one).
        for w in &mut cfg.status.right {
            if let plexy_glass_config::WidgetSpec::CpuLoad { interval, .. } = w {
                *interval = Some(Duration::from_millis(100));
            }
        }
        let engine = StatusEngine::new(&cfg.status, &cfg.palette, &GlyphSet::UNICODE);
        let notify = Arc::new(Notify::new());
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_inc = Arc::clone(&counter);
        let n2 = Arc::clone(&notify);
        tokio::spawn(async move {
            for _ in 0..10 {
                n2.notified().await;
                counter_inc.fetch_add(1, Ordering::SeqCst);
            }
        });
        let snapshot_ctx = || async {
            SnapshotCtx {
                session_name: "test".into(),
                windows: vec![],
                active_window: 0,
                attached_clients: 1,
                prefix_active: false,
                active_pane_cwd: None,
                copy_mode_active: false,
                sync_active: false,
                zoom_active: false,
            }
        };
        let handle = engine.spawn_tick_task(notify, snapshot_ctx);
        time::sleep(Duration::from_millis(600)).await;
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
        let engine = StatusEngine::new(&cfg.status, &cfg.palette, &GlyphSet::UNICODE);
        let inner = engine.inner();
        let ctx = EvalContext {
            session_name: "demo",
            windows: &[WindowSummary {
                name: "shell0".into(),
                activity: false,
                bell: false,
                done: None,
                silence: false,
            }],
            active_window: 0,
            attached_clients: 1,
            prefix_active: false,
            active_pane_cwd: None,
            copy_mode_active: false,
            sync_active: false,
            zoom_active: false,
            dragging_window: None,
            remote: false,
        };
        inner.refresh_event_driven(&ctx).await;
        let snap = inner.snapshot().await;
        // The session widget (first slot in left) should have a non-empty cache
        // containing "demo".
        assert!(!snap.left[0].is_empty());
        assert!(snap.left[0][0].text.contains("demo"));
    }
}
