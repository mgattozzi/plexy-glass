//! Build live sessions from config-declared templates (Feature B). This module
//! holds the *pure* layout flattening (an N-ary `PaneNode` → the engine's binary
//! split model, reusing the same algorithm as `session::collect_replay_ops`) and
//! per-pane `SpawnSpec` construction. The `Session` wiring is in
//! `session::build_from_template`.

use plexy_glass_config::{PaneNode, PaneTemplate, SplitDirection};
use plexy_glass_mux::SplitDir;
use plexy_glass_protocol::SpawnSpec;

/// A binary layout tree (the engine's split model) with a pane template at each
/// leaf. An N-ary config split folds into a right-leaning chain of binary splits.
pub(crate) enum BinLayout<'a> {
    Leaf(&'a PaneTemplate),
    Split {
        dir: SplitDir,
        first: Box<Self>,
        second: Box<Self>,
    },
}

pub(crate) struct BuildOp {
    pub target_dfs_idx: u32,
    pub new_pane_dfs_idx: u32,
    pub dir: SplitDir,
}

pub(crate) fn to_binary(node: &PaneNode) -> BinLayout<'_> {
    match node {
        PaneNode::Leaf(pt) => BinLayout::Leaf(pt),
        PaneNode::Split { dir, children, .. } => fold_children(map_dir(*dir), children),
    }
}

const fn map_dir(d: SplitDirection) -> SplitDir {
    match d {
        SplitDirection::Vertical => SplitDir::Vertical,
        SplitDirection::Horizontal => SplitDir::Horizontal,
    }
}

// invariant: `children.len() >= 2` (enforced by `decode_split`). Right-leaning fold.
fn fold_children(dir: SplitDir, children: &[PaneNode]) -> BinLayout<'_> {
    let first = Box::new(to_binary(&children[0]));
    let second = if children.len() == 2 {
        Box::new(to_binary(&children[1]))
    } else {
        Box::new(fold_children(dir, &children[1..]))
    };
    BinLayout::Split { dir, first, second }
}

/// Compute the preorder split-ratio list for a window's layout, matching the
/// order `LayoutTree::set_ratios_preorder` consumes (root split, then the first
/// subtree, then the second, the same structure `to_binary` builds).
///
/// At each right-leaning binary split node the first (direct) child's ratio is
/// its OWN declared weight / the sum of ALL the direct children's weights at
/// that level (the head included). A 0-total sibling group falls back to equal
/// weights, but that path is defense only: the decoder rejects `ratio=0`, so a
/// 0 total cannot arise from config.
pub(crate) fn preorder_ratios(node: &PaneNode) -> Vec<f32> {
    let mut out = Vec::new();
    push_preorder_ratios(node, &mut out);
    out
}

fn push_preorder_ratios(node: &PaneNode, out: &mut Vec<f32>) {
    let PaneNode::Split { children, weights, .. } = node else {
        return;
    };
    push_split_chain_ratios(children, weights, out);
}

/// Emit the right-leaning binary split chain over `children`/`weights` in the
/// preorder `set_ratios_preorder` walks: the chain head's ratio, then the head
/// child's own subtree, then recurse on the remaining children as the second
/// subtree. `children.len() == weights.len() >= 2` (decoder invariant).
fn push_split_chain_ratios(children: &[PaneNode], weights: &[u32], out: &mut Vec<f32>) {
    let total: u64 = weights.iter().map(|w| u64::from(*w)).sum();
    // Defense: a 0-total group (only reachable if a PaneNode::Split is
    // constructed bypassing the decoder) splits evenly rather than NaN.
    let ratio = if total == 0 {
        1.0 / weights.len() as f32
    } else {
        u64::from(weights[0]) as f32 / total as f32
    };
    out.push(ratio);
    // First subtree: the head child's own splits.
    push_preorder_ratios(&children[0], out);
    // Second subtree: the rest of the chain (>= 1 child remaining).
    if children.len() == 2 {
        push_preorder_ratios(&children[1], out);
    } else {
        push_split_chain_ratios(&children[1..], &weights[1..], out);
    }
}

/// The DFS-leaf index (left-first, the canonical pane index order) of the
/// `active=#true` leaf in a window's layout, if any. At most one leaf is active
/// (decode-enforced); returns the first marked leaf's index.
pub(crate) fn active_leaf_index(node: &PaneNode) -> Option<usize> {
    let mut idx = 0usize;
    find_active_leaf(node, &mut idx)
}

fn find_active_leaf(node: &PaneNode, idx: &mut usize) -> Option<usize> {
    match node {
        PaneNode::Leaf(p) => {
            let here = *idx;
            *idx += 1;
            if p.active { Some(here) } else { None }
        }
        PaneNode::Split { children, .. } => {
            for child in children {
                if let Some(found) = find_active_leaf(child, idx) {
                    return Some(found);
                }
            }
            None
        }
    }
}

/// Pane templates in left-first DFS order (the canonical pane index order).
pub(crate) fn bin_leaves<'a>(node: &'a BinLayout<'a>) -> Vec<&'a PaneTemplate> {
    let mut out = Vec::new();
    collect_leaves(node, &mut out);
    out
}

fn collect_leaves<'a>(node: &'a BinLayout<'a>, out: &mut Vec<&'a PaneTemplate>) {
    match node {
        BinLayout::Leaf(pt) => out.push(pt),
        BinLayout::Split { first, second, .. } => {
            collect_leaves(first, out);
            collect_leaves(second, out);
        }
    }
}

/// Split ops in the order to replay them (pre-order; identical accounting to
/// `session::collect_replay_ops`).
pub(crate) fn collect_ops(node: &BinLayout) -> Vec<BuildOp> {
    let mut out = Vec::new();
    collect_ops_rec(node, 0, &mut out);
    out
}

fn collect_ops_rec(node: &BinLayout, base_dfs: u32, out: &mut Vec<BuildOp>) {
    if let BinLayout::Split { dir, first, second } = node {
        let target = leftmost_dfs(first, base_dfs);
        let first_size = count_leaves(first);
        out.push(BuildOp {
            target_dfs_idx: target,
            new_pane_dfs_idx: base_dfs + first_size,
            dir: *dir,
        });
        collect_ops_rec(first, base_dfs, out);
        collect_ops_rec(second, base_dfs + first_size, out);
    }
}

fn leftmost_dfs(node: &BinLayout, base: u32) -> u32 {
    match node {
        BinLayout::Leaf(_) => base,
        BinLayout::Split { first, .. } => leftmost_dfs(first, base),
    }
}

fn count_leaves(node: &BinLayout) -> u32 {
    match node {
        BinLayout::Leaf(_) => 1,
        BinLayout::Split { first, second, .. } => count_leaves(first) + count_leaves(second),
    }
}

/// A window's home base: the cwd new panes/splits in the window inherit.
/// `window` cwd wins over `session` cwd; a leading `~` is expanded. `None`
/// means "no anchor" (the daemon's cwd).
pub(crate) fn resolve_home_cwd(
    window_cwd: Option<&str>,
    session_cwd: Option<&str>,
    home: Option<&str>,
) -> Option<String> {
    window_cwd.or(session_cwd).map(|c| expand_tilde(c, home))
}

/// `resolve_home_cwd` reading `HOME` from the environment.
pub(crate) fn home_base(window_cwd: Option<&str>, session_cwd: Option<&str>) -> Option<String> {
    let home = std::env::var("HOME").ok();
    resolve_home_cwd(window_cwd, session_cwd, home.as_deref())
}

/// The `SpawnSpec` for a declared pane: `command` runs via the default shell
/// `-c`; no command = an interactive default shell. cwd = the pane's own cwd
/// (tilde-expanded), else the window `home_cwd` (already expanded). `env` is the
/// effective overlay (session ∪ window ∪ pane, already merged) set ON TOP of
/// the inherited daemon environment by the spawn path.
pub(crate) fn pane_spec(pt: &PaneTemplate, home_cwd: Option<&str>, env: Vec<(String, String)>) -> SpawnSpec {
    let home = std::env::var("HOME").ok();
    make_spec(&default_shell(), pt, home_cwd, home.as_deref(), env)
}

fn make_spec(
    shell: &str,
    pt: &PaneTemplate,
    home_cwd: Option<&str>,
    home: Option<&str>,
    env: Vec<(String, String)>,
) -> SpawnSpec {
    let args = match &pt.command {
        Some(cmd) => vec!["-c".to_string(), cmd.clone()],
        None => vec![],
    };
    let cwd = match pt.cwd.as_deref() {
        Some(c) => Some(expand_tilde(c, home)),
        None => home_cwd.map(str::to_string),
    };
    SpawnSpec { program: shell.to_string(), args, env, cwd }
}

/// Merge env overlays in inheritance order (session, then window, then pane),
/// with later levels overriding earlier ones per key, preserving each key's
/// first-seen declared order. The result is the pane's effective env overlay.
pub(crate) fn merge_env(
    session: &[(String, String)],
    window: &[(String, String)],
    pane: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in session.iter().chain(window).chain(pane) {
        if let Some(slot) = out.iter_mut().find(|(ek, _)| ek == k) {
            slot.1.clone_from(v);
        } else {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

pub(crate) fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

/// Expand a leading `~` / `~/…` against `home`. No `~user` form; with no
/// HOME the path is returned verbatim. Shared with the connection layer's
/// `save-buffer` / `load-buffer` path policy.
pub(crate) fn expand_tilde(path: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return path.to_string();
    };
    if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_config::{PaneNode, PaneTemplate, SplitDirection};

    fn leaf(cmd: Option<&str>) -> PaneNode {
        PaneNode::Leaf(PaneTemplate {
            command: cmd.map(str::to_string),
            cwd: None,
            name: None,
            active: false,
            env: vec![],
        })
    }

    #[test]
    fn make_spec_command_runs_via_shell_dash_c() {
        let pt = PaneTemplate { command: Some("npm run dev".into()), cwd: None, name: None, active: false, env: vec![] };
        let s = make_spec("/bin/zsh", &pt, None, None, vec![]);
        assert_eq!(s.program, "/bin/zsh");
        assert_eq!(s.args, vec!["-c".to_string(), "npm run dev".to_string()]);
        assert!(s.env.is_empty());
        assert_eq!(s.cwd, None);
    }

    #[test]
    fn make_spec_no_command_is_interactive_shell() {
        let pt = PaneTemplate { command: None, cwd: None, name: None, active: false, env: vec![] };
        let s = make_spec("/bin/sh", &pt, None, None, vec![]);
        assert_eq!(s.program, "/bin/sh");
        assert!(s.args.is_empty());
    }

    #[test]
    fn make_spec_cwd_precedence_and_tilde() {
        // `home_cwd` is already resolved (`resolve_home_cwd` expanded the tilde upstream).
        let home_cwd = Some("/home/u/proj");
        let pt_override = PaneTemplate { command: None, cwd: Some("~/proj/sub".into()), name: None, active: false, env: vec![] };
        let s = make_spec("/bin/sh", &pt_override, home_cwd, Some("/home/u"), vec![]);
        assert_eq!(s.cwd.as_deref(), Some("/home/u/proj/sub"));
        let pt_inherit = PaneTemplate { command: None, cwd: None, name: None, active: false, env: vec![] };
        let s2 = make_spec("/bin/sh", &pt_inherit, home_cwd, Some("/home/u"), vec![]);
        assert_eq!(s2.cwd.as_deref(), Some("/home/u/proj"));
    }

    fn pt(cwd: Option<&str>) -> PaneTemplate {
        PaneTemplate { command: None, cwd: cwd.map(str::to_string), name: None, active: false, env: vec![] }
    }

    #[test]
    fn resolve_home_cwd_window_wins_then_session() {
        // window cwd wins over session cwd; tilde expands.
        assert_eq!(resolve_home_cwd(Some("~/w"), Some("~/s"), Some("/home/u")), Some("/home/u/w".into()));
        // no window cwd → session cwd.
        assert_eq!(resolve_home_cwd(None, Some("~/s"), Some("/home/u")), Some("/home/u/s".into()));
        // neither → None.
        assert_eq!(resolve_home_cwd(None, None, Some("/home/u")), None);
    }

    #[test]
    fn make_spec_pane_cwd_overrides_home_base() {
        // pane cwd wins over the resolved home base.
        let s = make_spec("/bin/sh", &pt(Some("~/pane")), Some("/home/u/home_base"), Some("/home/u"), vec![]);
        assert_eq!(s.cwd.as_deref(), Some("/home/u/pane"));
        // no pane cwd → home base (already expanded; not re-expanded).
        let s = make_spec("/bin/sh", &pt(None), Some("/home/u/home_base"), Some("/home/u"), vec![]);
        assert_eq!(s.cwd.as_deref(), Some("/home/u/home_base"));
        // no pane cwd, no home base → None.
        let s = make_spec("/bin/sh", &pt(None), None, Some("/home/u"), vec![]);
        assert_eq!(s.cwd, None);
    }

    #[test]
    fn expand_tilde_cases() {
        assert_eq!(expand_tilde("~", Some("/home/u")), "/home/u");
        assert_eq!(expand_tilde("~/a/b", Some("/home/u")), "/home/u/a/b");
        assert_eq!(expand_tilde("/abs", Some("/home/u")), "/abs");
        assert_eq!(expand_tilde("~/a", None), "~/a"); // no HOME: unchanged
        assert_eq!(expand_tilde("~", None), "~"); // no HOME: unchanged
        assert_eq!(expand_tilde("~user/a", Some("/home/u")), "~user/a"); // no ~user form
        assert_eq!(expand_tilde("rel/a", Some("/home/u")), "rel/a"); // relative: verbatim
        assert_eq!(expand_tilde("a~/b", Some("/home/u")), "a~/b"); // ~ only leads
    }

    #[test]
    fn flatten_single_leaf_has_no_ops() {
        let node = leaf(None);
        let bin = to_binary(&node);
        assert_eq!(bin_leaves(&bin).len(), 1);
        assert!(collect_ops(&bin).is_empty());
    }

    #[test]
    fn flatten_flat_three_way_split() {
        // split vertical { A B C } → side-by-side; ops split 0 then 1.
        let node = PaneNode::Split {
            dir: SplitDirection::Vertical,
            children: vec![leaf(Some("a")), leaf(Some("b")), leaf(Some("c"))],
            weights: vec![1, 1, 1],
        };
        let bin = to_binary(&node);
        let leaves = bin_leaves(&bin);
        assert_eq!(
            leaves.iter().map(|p| p.command.as_deref()).collect::<Vec<_>>(),
            vec![Some("a"), Some("b"), Some("c")]
        );
        let ops = collect_ops(&bin);
        assert_eq!(ops.len(), 2);
        assert_eq!((ops[0].target_dfs_idx, ops[0].new_pane_dfs_idx), (0, 1));
        assert_eq!((ops[1].target_dfs_idx, ops[1].new_pane_dfs_idx), (1, 2));
        assert!(ops.iter().all(|o| o.dir == SplitDir::Vertical));
    }

    #[test]
    fn flatten_nested_split_targets_and_dirs() {
        // split vertical { A  (split horizontal { B C }) }
        let node = PaneNode::Split {
            dir: SplitDirection::Vertical,
            children: vec![
                leaf(Some("a")),
                PaneNode::Split {
                    dir: SplitDirection::Horizontal,
                    children: vec![leaf(Some("b")), leaf(Some("c"))],
                    weights: vec![1, 1],
                },
            ],
            weights: vec![1, 1],
        };
        let bin = to_binary(&node);
        assert_eq!(
            bin_leaves(&bin).iter().map(|p| p.command.as_deref()).collect::<Vec<_>>(),
            vec![Some("a"), Some("b"), Some("c")]
        );
        let ops = collect_ops(&bin);
        assert_eq!(ops.len(), 2);
        // top split: split A@0 vertically, new pane at dfs 1 (B's slot)
        assert_eq!(
            (ops[0].target_dfs_idx, ops[0].new_pane_dfs_idx, ops[0].dir),
            (0, 1, SplitDir::Vertical)
        );
        // nested split: split B@1 horizontally, new pane at dfs 2 (C)
        assert_eq!(
            (ops[1].target_dfs_idx, ops[1].new_pane_dfs_idx, ops[1].dir),
            (1, 2, SplitDir::Horizontal)
        );
    }

    // --- v2: preorder ratios ---

    fn split(children: Vec<PaneNode>, weights: Vec<u32>) -> PaneNode {
        PaneNode::Split { dir: SplitDirection::Vertical, children, weights }
    }

    #[test]
    fn single_leaf_has_no_ratios() {
        assert!(preorder_ratios(&leaf(None)).is_empty());
    }

    #[test]
    fn two_way_default_is_half_v1_identical() {
        let node = split(vec![leaf(None), leaf(None)], vec![1, 1]);
        assert_eq!(preorder_ratios(&node), vec![0.5]);
    }

    #[test]
    fn flat_three_way_default_is_third_then_half() {
        // INTENTIONAL change from v1's 50/25/25 right-lean cascade: even
        // 33/33/33 via preorder [1/3, 1/2]. Pin this.
        let node = split(vec![leaf(None), leaf(None), leaf(None)], vec![1, 1, 1]);
        let r = preorder_ratios(&node);
        assert_eq!(r.len(), 2);
        assert!((r[0] - 1.0 / 3.0).abs() < 1e-6, "{r:?}");
        assert!((r[1] - 0.5).abs() < 1e-6, "{r:?}");
    }

    #[test]
    fn two_way_weighted_is_two_thirds() {
        let node = split(vec![leaf(None), leaf(None)], vec![2, 1]);
        let r = preorder_ratios(&node);
        assert_eq!(r.len(), 1);
        assert!((r[0] - 2.0 / 3.0).abs() < 1e-6, "{r:?}");
    }

    #[test]
    fn flat_three_way_weighted_one_two_one() {
        // weights [1,2,1]: outer = 1/(1+2+1) = 1/4, inner = 2/(2+1) = 2/3.
        let node = split(vec![leaf(None), leaf(None), leaf(None)], vec![1, 2, 1]);
        let r = preorder_ratios(&node);
        assert_eq!(r.len(), 2);
        assert!((r[0] - 0.25).abs() < 1e-6, "{r:?}");
        assert!((r[1] - 2.0 / 3.0).abs() < 1e-6, "{r:?}");
    }

    #[test]
    fn nested_split_weight_is_its_own_ratio_not_leaf_count() {
        // outer { pane ratio=2; split ratio=1 { pane; pane } }
        // outer ratio = 2/(2+1) = 2/3 REGARDLESS of the inner leaf count; then
        // the head child (a leaf, no ratios), then the inner split's own [0.5].
        let inner = split(vec![leaf(None), leaf(None)], vec![1, 1]);
        let node = split(vec![leaf(None), inner], vec![2, 1]);
        let r = preorder_ratios(&node);
        assert_eq!(r.len(), 2);
        assert!((r[0] - 2.0 / 3.0).abs() < 1e-6, "{r:?}");
        assert!((r[1] - 0.5).abs() < 1e-6, "{r:?}");
    }

    #[test]
    fn zero_total_group_falls_back_to_equal_weights() {
        // Defense path only (decoder rejects ratio=0): a 0-total group must not
        // produce NaN.
        let node = split(vec![leaf(None), leaf(None)], vec![0, 0]);
        let r = preorder_ratios(&node);
        assert_eq!(r.len(), 1);
        assert!((r[0] - 0.5).abs() < 1e-6, "{r:?}");
        assert!(!r[0].is_nan());
    }

    // --- v2: active-leaf DFS index ---

    fn active_leaf(active: bool) -> PaneNode {
        PaneNode::Leaf(PaneTemplate {
            command: None,
            cwd: None,
            name: None,
            active,
            env: vec![],
        })
    }

    #[test]
    fn active_leaf_index_none_when_unmarked() {
        let node = split(vec![leaf(None), leaf(None)], vec![1, 1]);
        assert_eq!(active_leaf_index(&node), None);
        assert_eq!(active_leaf_index(&leaf(None)), None);
    }

    #[test]
    fn active_leaf_index_picks_marked_in_dfs_order() {
        // outer { L0  (inner { L1*  L2 }) } → marked leaf is DFS index 1.
        let inner = PaneNode::Split {
            dir: SplitDirection::Horizontal,
            children: vec![active_leaf(true), active_leaf(false)],
            weights: vec![1, 1],
        };
        let node = split(vec![active_leaf(false), inner], vec![1, 1]);
        assert_eq!(active_leaf_index(&node), Some(1));
    }

    // --- v2: env merge ---

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn merge_env_pane_overrides_window_overrides_session() {
        let session = kv(&[("A", "s"), ("B", "s"), ("C", "s")]);
        let window = kv(&[("B", "w"), ("D", "w")]);
        let pane = kv(&[("C", "p")]);
        let merged = merge_env(&session, &window, &pane);
        // A from session, B overridden by window, C overridden by pane, D from window.
        let lookup = |k: &str| merged.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.as_str());
        assert_eq!(lookup("A"), Some("s"));
        assert_eq!(lookup("B"), Some("w"));
        assert_eq!(lookup("C"), Some("p"));
        assert_eq!(lookup("D"), Some("w"));
        assert_eq!(merged.len(), 4);
    }

    #[test]
    fn merge_env_preserves_first_seen_order() {
        let session = kv(&[("A", "1")]);
        let window = kv(&[("B", "2")]);
        let pane = kv(&[("A", "3"), ("C", "4")]);
        let merged = merge_env(&session, &window, &pane);
        let keys: Vec<&str> = merged.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["A", "B", "C"], "A keeps its first-seen slot but takes pane's value");
        assert_eq!(merged[0].1, "3");
    }
}
