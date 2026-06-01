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
        first: Box<BinLayout<'a>>,
        second: Box<BinLayout<'a>>,
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
        PaneNode::Split { dir, children } => fold_children(map_dir(*dir), children),
    }
}

fn map_dir(d: SplitDirection) -> SplitDir {
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

/// The `SpawnSpec` for a declared pane: `command` runs via the default shell
/// `-c`; no command = an interactive default shell. cwd = pane cwd, else the
/// session cwd, with a leading `~` expanded. `env` empty = inherit daemon env.
pub(crate) fn pane_spec(pt: &PaneTemplate, session_cwd: &Option<String>) -> SpawnSpec {
    let home = std::env::var("HOME").ok();
    make_spec(&default_shell(), pt, session_cwd, home.as_deref())
}

fn make_spec(shell: &str, pt: &PaneTemplate, session_cwd: &Option<String>, home: Option<&str>) -> SpawnSpec {
    let args = match &pt.command {
        Some(cmd) => vec!["-c".to_string(), cmd.clone()],
        None => vec![],
    };
    let cwd = pt
        .cwd
        .as_deref()
        .or(session_cwd.as_deref())
        .map(|c| expand_tilde(c, home));
    SpawnSpec { program: shell.to_string(), args, env: vec![], cwd }
}

pub(crate) fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

fn expand_tilde(path: &str, home: Option<&str>) -> String {
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
        PaneNode::Leaf(PaneTemplate { command: cmd.map(str::to_string), cwd: None, name: None })
    }

    #[test]
    fn make_spec_command_runs_via_shell_dash_c() {
        let pt = PaneTemplate { command: Some("npm run dev".into()), cwd: None, name: None };
        let s = make_spec("/bin/zsh", &pt, &None, None);
        assert_eq!(s.program, "/bin/zsh");
        assert_eq!(s.args, vec!["-c".to_string(), "npm run dev".to_string()]);
        assert!(s.env.is_empty());
        assert_eq!(s.cwd, None);
    }

    #[test]
    fn make_spec_no_command_is_interactive_shell() {
        let pt = PaneTemplate { command: None, cwd: None, name: None };
        let s = make_spec("/bin/sh", &pt, &None, None);
        assert_eq!(s.program, "/bin/sh");
        assert!(s.args.is_empty());
    }

    #[test]
    fn make_spec_cwd_precedence_and_tilde() {
        let session_cwd = Some("~/proj".to_string());
        let pt_override = PaneTemplate { command: None, cwd: Some("~/proj/sub".into()), name: None };
        let s = make_spec("/bin/sh", &pt_override, &session_cwd, Some("/home/u"));
        assert_eq!(s.cwd.as_deref(), Some("/home/u/proj/sub"));
        let pt_inherit = PaneTemplate { command: None, cwd: None, name: None };
        let s2 = make_spec("/bin/sh", &pt_inherit, &session_cwd, Some("/home/u"));
        assert_eq!(s2.cwd.as_deref(), Some("/home/u/proj"));
    }

    #[test]
    fn expand_tilde_cases() {
        assert_eq!(expand_tilde("~", Some("/home/u")), "/home/u");
        assert_eq!(expand_tilde("~/a/b", Some("/home/u")), "/home/u/a/b");
        assert_eq!(expand_tilde("/abs", Some("/home/u")), "/abs");
        assert_eq!(expand_tilde("~/a", None), "~/a"); // no HOME: unchanged
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
                },
            ],
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
}
