//! Declared-template session construction (Feature B).
//!
//! The daemon no longer restores sessions from disk; a fresh daemon builds
//! config-declared sessions here and otherwise creates sessions on demand.

use std::io;
use std::sync::Arc;

use plexy_glass_protocol::PtySize;

use super::Session;
use crate::declared;
use crate::error::DaemonError;
use crate::window_manager::WindowManager;

impl Session {
    /// Build a `Session` fresh from a config-declared template (Feature B).
    ///
    /// The template is the source of truth (nothing is read from disk). Each
    /// pane runs its declared `command` via the default shell (or an interactive
    /// shell when no command), with cwd resolved from the pane/session template
    /// and its effective `env` overlay (session ∪ window ∪ pane). Split ratios
    /// honor the declared `ratio=` weights, and the declared active window/pane
    /// (else window 0 / DFS-leftmost) is focused.
    pub async fn build_from_template(
        template: &plexy_glass_config::SessionTemplate,
        size: PtySize,
        config: Arc<plexy_glass_config::Config>,
    ) -> Result<Arc<Self>, DaemonError> {
        let first_window = template.windows.first().ok_or_else(|| {
            DaemonError::Io(io::Error::other("declared session has zero windows"))
        })?;
        let bin0 = declared::to_binary(&first_window.layout);
        let leaves0 = declared::bin_leaves(&bin0);
        let win0_home = declared::home_base(first_window.cwd.as_deref(), template.cwd.as_deref());
        // invariant: a PaneNode always has >= 1 leaf, so leaves0[0] exists.
        let first_env = declared::merge_env(&template.env, &first_window.env, &leaves0[0].env);
        let first_spec = declared::pane_spec(leaves0[0], win0_home.as_deref(), first_env);

        let session = Self::new(template.name.clone(), first_spec, size, Arc::clone(&config))?;
        let build: Result<(), DaemonError> = async {
            let mut wm = session.window_manager.lock().await;
            wm.set_session_cwd(declared::home_base(None, template.cwd.as_deref()));
            wm.set_window_name(0, first_window.name.clone());
            wm.set_window_home_cwd(0, win0_home.clone());
            build_window_from_bin(
                &mut wm,
                0,
                &bin0,
                &leaves0,
                win0_home.as_deref(),
                &template.env,
                first_window,
            )?;
            for (wi, w) in template.windows.iter().enumerate().skip(1) {
                let bin = declared::to_binary(&w.layout);
                let leaves = declared::bin_leaves(&bin);
                let home = declared::home_base(w.cwd.as_deref(), template.cwd.as_deref());
                let env = declared::merge_env(&template.env, &w.env, &leaves[0].env);
                let first = declared::pane_spec(leaves[0], home.as_deref(), env);
                wm.new_window_with_spec(first, w.name.clone())?;
                wm.set_window_home_cwd(wi, home.clone());
                build_window_from_bin(
                    &mut wm,
                    wi,
                    &bin,
                    &leaves,
                    home.as_deref(),
                    &template.env,
                    w,
                )?;
            }
            // Focus the declared active window (else window 0). The per-window
            // active pane was set inside build_window_from_bin.
            let active_window = template.windows.iter().position(|w| w.active).unwrap_or(0);
            wm.set_active_window(active_window);
            Ok(())
        }
        .await;
        if let Err(e) = build {
            // Tear down a partially-built session: the first pane (and maybe more)
            // already exist and would leak their children + reader threads.
            session.begin_close();
            session.terminate_panes().await;
            return Err(e);
        }
        Ok(session)
    }
}

/// Build `window_idx`'s panes from a binary layout.
///
/// Each leaf spawns via its declared `SpawnSpec`. The window's first pane
/// already exists; we replay split ops in pre-order, then apply the declared
/// split ratios, pane names, and active pane.
fn build_window_from_bin(
    wm: &mut WindowManager,
    window_idx: usize,
    bin: &declared::BinLayout,
    leaves: &[&plexy_glass_config::PaneTemplate],
    home_cwd: Option<&str>,
    session_env: &[(String, String)],
    window: &plexy_glass_config::WindowTemplate,
) -> Result<(), DaemonError> {
    for op in declared::collect_ops(bin) {
        // invariant: new_pane_dfs_idx < leaves.len() (collect_ops indexes the
        // same DFS order bin_leaves produced).
        let pt = leaves[op.new_pane_dfs_idx as usize];
        let env = declared::merge_env(session_env, &window.env, &pt.env);
        let spec = declared::pane_spec(pt, home_cwd, env);
        wm.split_window_at_dfs(window_idx, op.target_dfs_idx, op.dir, spec)?;
    }
    // Apply the declared split ratios (from `ratio=` weights). The build above
    // rebuilt the shape at default 0.5 ratios; the preorder ratio list maps
    // 1:1 onto the live tree's splits. Then resize panes to the corrected rects.
    let ratios = declared::preorder_ratios(&window.layout);
    if !ratios.is_empty() {
        let viewport = wm.viewport();
        if let Some(win) = wm.windows_mut().get_mut(window_idx) {
            win.layout_mut().set_ratios_preorder(&ratios);
            win.resize(viewport)?;
        }
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
        // Focus the declared active pane (else the DFS-leftmost default).
        if let Some(active_idx) = declared::active_leaf_index(&window.layout)
            && let Some(pid) = pane_ids.get(active_idx)
        {
            win.focus(*pid);
        }
    }
    Ok(())
}
