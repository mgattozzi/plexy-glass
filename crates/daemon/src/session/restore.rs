//! Session restore and declared-template construction helpers.

use super::Session;
use crate::{error::DaemonError, window_manager::WindowManager};
use plexy_glass_protocol::{PtySize, SpawnSpec};
use std::sync::Arc;

impl Session {
    /// Build a Session from a saved on-disk state. The base shell is the
    /// same as `new`; we then replay structural changes (splits, extra
    /// windows, names, sync_input, focus) to reach the saved layout.
    /// Each restored pane spawns the caller-supplied `base_spec` with cwd
    /// set from the saved state. Split ratios are restored from the saved
    /// state.
    pub async fn restore_from(
        saved: crate::persist::SessionStateV1,
        base_spec: SpawnSpec,
        size: PtySize,
        config: Arc<plexy_glass_config::Config>,
    ) -> Result<Arc<Self>, DaemonError> {
        let first_window = saved.windows.first().ok_or_else(|| {
            DaemonError::Io(std::io::Error::other("restored session has zero windows"))
        })?;
        let first_pane_saved = first_window.panes.first().ok_or_else(|| {
            DaemonError::Io(std::io::Error::other("restored window has zero panes"))
        })?;
        let mut first_spec = base_spec.clone();
        first_spec.cwd = restore_cwd(first_pane_saved.cwd.as_deref());

        let session = Self::new(saved.name.clone(), first_spec, size, Arc::clone(&config))?;
        {
            let mut wm = session.window_manager.lock().await;
            // Re-anchor the session base cwd so interactive new windows
            // (`Ctrl+a c` anchors to session_cwd) keep working after restore.
            // SessionStateV1 has no session-level cwd field; window 0's saved
            // home base is the persisted proxy (for a declared session it
            // equals the session cwd when window 0 has no own cwd, and for an
            // interactively created session both are None, preserving the
            // pre-detach daemon-cwd behavior).
            wm.set_session_cwd(first_window.home_cwd.clone());
            // Window 0 already exists from Session::new with its first pane, so
            // restore its name + remaining panes via replay.
            wm.set_window_name(0, first_window.name.clone());
            replay_window_layout(&mut wm, 0, first_window, &base_spec)?;
            for (wi, w) in saved.windows.iter().enumerate().skip(1) {
                let first_pane = w.panes.first().ok_or_else(|| {
                    DaemonError::Io(std::io::Error::other(format!(
                        "restored window {wi} has zero panes"
                    )))
                })?;
                let mut spec_for_first = base_spec.clone();
                spec_for_first.cwd = restore_cwd(first_pane.cwd.as_deref());
                wm.new_window_with_spec(spec_for_first, w.name.clone())?;
                replay_window_layout(&mut wm, wi, w, &base_spec)?;
            }
            // Restore per-window flags + active-pane focus.
            for (i, saved_w) in saved.windows.iter().enumerate() {
                if let Some(win) = wm.windows_mut().get_mut(i) {
                    win.sync_input = saved_w.sync_input;
                    win.home_cwd = saved_w.home_cwd.clone();
                    let leaves = win.layout().dfs_leaves();
                    // Restore user-assigned pane names by DFS index (the same
                    // order panes were serialized in).
                    for (li, pid) in leaves.iter().enumerate() {
                        if let Some(ps) = saved_w.panes.get(li)
                            && let Some(p) = win.pane(*pid)
                        {
                            p.set_name(ps.name.clone());
                        }
                    }
                    if let Some(pid) = leaves.get(saved_w.active_pane as usize) {
                        win.focus(*pid);
                    }
                }
            }
            let active = saved
                .active_window
                .min(wm.windows().len().saturating_sub(1));
            wm.set_active_window(active);
        }
        // Round-trip: re-save the restored shape (also catches any drift
        // between the saved file and what we actually built).
        session.mark_dirty();
        Ok(session)
    }

    /// Build a Session fresh from a config-declared template (Feature B).
    /// Unlike `restore_from`, this never reads disk, the template is the
    /// source of truth. Each pane runs its declared `command` via the default
    /// shell (or an interactive shell when no command), with cwd resolved from
    /// the pane/session template. Split ratios are 50/50 (templates have no
    /// ratios).
    pub async fn build_from_template(
        template: &plexy_glass_config::SessionTemplate,
        size: PtySize,
        config: Arc<plexy_glass_config::Config>,
    ) -> Result<Arc<Self>, DaemonError> {
        let first_window = template.windows.first().ok_or_else(|| {
            DaemonError::Io(std::io::Error::other("declared session has zero windows"))
        })?;
        let bin0 = crate::declared::to_binary(&first_window.layout);
        let leaves0 = crate::declared::bin_leaves(&bin0);
        let win0_home =
            crate::declared::home_base(first_window.cwd.as_deref(), template.cwd.as_deref());
        // invariant: a PaneNode always has >= 1 leaf, so leaves0[0] exists.
        let first_spec = crate::declared::pane_spec(leaves0[0], win0_home.as_deref());

        let session = Self::new(template.name.clone(), first_spec, size, Arc::clone(&config))?;
        {
            let mut wm = session.window_manager.lock().await;
            wm.set_session_cwd(crate::declared::home_base(None, template.cwd.as_deref()));
            wm.set_window_name(0, first_window.name.clone());
            wm.set_window_home_cwd(0, win0_home.clone());
            build_window_from_bin(&mut wm, 0, &bin0, &leaves0, win0_home.as_deref())?;
            for (wi, w) in template.windows.iter().enumerate().skip(1) {
                let bin = crate::declared::to_binary(&w.layout);
                let leaves = crate::declared::bin_leaves(&bin);
                let home = crate::declared::home_base(w.cwd.as_deref(), template.cwd.as_deref());
                let first = crate::declared::pane_spec(leaves[0], home.as_deref());
                wm.new_window_with_spec(first, w.name.clone())?;
                wm.set_window_home_cwd(wi, home.clone());
                build_window_from_bin(&mut wm, wi, &bin, &leaves, home.as_deref())?;
            }
            wm.set_active_window(0);
        }
        // Persist the built shape (harmless; declared names are never restored).
        session.mark_dirty();
        Ok(session)
    }
}

/// Build `window_idx`'s panes from a binary layout, using each leaf's declared
/// `SpawnSpec`. The window's first pane already exists; we replay split ops in
/// pre-order (same accounting as `collect_replay_ops`), then apply pane names.
pub(super) fn build_window_from_bin(
    wm: &mut WindowManager,
    window_idx: usize,
    bin: &crate::declared::BinLayout,
    leaves: &[&plexy_glass_config::PaneTemplate],
    home_cwd: Option<&str>,
) -> Result<(), DaemonError> {
    for op in crate::declared::collect_ops(bin) {
        // invariant: new_pane_dfs_idx < leaves.len() (collect_ops indexes the
        // same DFS order bin_leaves produced).
        let spec = crate::declared::pane_spec(leaves[op.new_pane_dfs_idx as usize], home_cwd);
        wm.split_window_at_dfs(window_idx, op.target_dfs_idx, op.dir, spec)?;
    }
    if let Some(win) = wm.windows_mut().get_mut(window_idx) {
        let pane_ids = win.layout().dfs_leaves();
        for (i, pid) in pane_ids.iter().enumerate() {
            if let Some(pt) = leaves.get(i)
                && let Some(p) = win.pane(*pid)
            {
                p.set_name(pt.name.clone());
            }
        }
    }
    Ok(())
}

/// Convert a saved pane cwd into a spawnable filesystem path. New persist
/// files store plain paths (which pass through unchanged), but legacy files
/// carry raw OSC-7 `file://host/path` URLs; portable-pty silently falls back
/// to `$HOME` for a cwd that isn't a directory, so the URL must be stripped
/// here. Malformed values map to `None` (daemon-cwd fallback).
pub(super) fn restore_cwd(saved: Option<&str>) -> Option<String> {
    saved.and_then(crate::popup::osc7_to_path)
}

/// Replay a saved layout for `window_idx`. The window's first pane is
/// already present; we walk the saved layout depth-first, splitting the
/// existing structure at each Split node to spawn the next pane.
pub(super) fn replay_window_layout(
    wm: &mut WindowManager,
    window_idx: usize,
    saved: &crate::persist::WindowStateV1,
    base_spec: &SpawnSpec,
) -> Result<(), DaemonError> {
    let mut ops: Vec<ReplayOp> = Vec::new();
    collect_replay_ops(&saved.layout, 0, &mut ops);
    for op in ops {
        let mut spec = base_spec.clone();
        spec.cwd = restore_cwd(
            saved
                .panes
                .get(op.new_pane_dfs_idx as usize)
                .and_then(|p| p.cwd.as_deref()),
        );
        wm.split_window_at_dfs(window_idx, op.target_dfs_idx, op.dir, spec)?;
    }
    // The replay rebuilt the exact saved shape at default 0.5 ratios, so the
    // saved preorder ratio list maps 1:1 onto the live tree's splits.
    // Re-apply them, then resize panes to their corrected rects.
    let mut ratios = Vec::new();
    preorder_ratios(&saved.layout, &mut ratios);
    if !ratios.is_empty() {
        let viewport = wm.viewport();
        if let Some(win) = wm.windows_mut().get_mut(window_idx) {
            win.layout_mut().set_ratios_preorder(&ratios);
            win.resize(viewport)?;
        }
    }
    Ok(())
}

struct ReplayOp {
    /// DFS index of the existing pane being split.
    target_dfs_idx: u32,
    /// DFS index that the newly-spawned pane will occupy AFTER the split.
    new_pane_dfs_idx: u32,
    dir: plexy_glass_mux::SplitDir,
}

/// Collect split ratios in preorder (root, first subtree, second), the same
/// order `set_ratios_preorder` consumes.
fn preorder_ratios(node: &crate::persist::LayoutStateV1, out: &mut Vec<f32>) {
    if let crate::persist::LayoutStateV1::Split { ratio, first, second, .. } = node {
        out.push(*ratio);
        preorder_ratios(first, out);
        preorder_ratios(second, out);
    }
}

fn collect_replay_ops(
    node: &crate::persist::LayoutStateV1,
    base_dfs: u32,
    out: &mut Vec<ReplayOp>,
) {
    use crate::persist::{LayoutDirV1, LayoutStateV1};
    match node {
        LayoutStateV1::Leaf(_) => {}
        LayoutStateV1::Split { dir, first, second, .. } => {
            let target = leftmost_leaf_dfs(first, base_dfs);
            let first_size = count_leaves(first);
            let new_pane = base_dfs + first_size;
            out.push(ReplayOp {
                target_dfs_idx: target,
                new_pane_dfs_idx: new_pane,
                dir: match dir {
                    LayoutDirV1::Vertical => plexy_glass_mux::SplitDir::Vertical,
                    LayoutDirV1::Horizontal => plexy_glass_mux::SplitDir::Horizontal,
                },
            });
            collect_replay_ops(first, base_dfs, out);
            collect_replay_ops(second, base_dfs + first_size, out);
        }
    }
}

fn leftmost_leaf_dfs(node: &crate::persist::LayoutStateV1, base: u32) -> u32 {
    match node {
        crate::persist::LayoutStateV1::Leaf(_) => base,
        crate::persist::LayoutStateV1::Split { first, .. } => leftmost_leaf_dfs(first, base),
    }
}

fn count_leaves(node: &crate::persist::LayoutStateV1) -> u32 {
    match node {
        crate::persist::LayoutStateV1::Leaf(_) => 1,
        crate::persist::LayoutStateV1::Split { first, second, .. } => {
            count_leaves(first) + count_leaves(second)
        }
    }
}
