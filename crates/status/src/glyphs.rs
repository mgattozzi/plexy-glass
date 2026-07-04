use plexy_glass_config::GlyphTier;

use crate::{ResolvedStyle, Segment};

/// Which edge of the status bar a zone is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cluster {
    Left,
    Right,
}

/// Flatten a zone's per-widget segment groups into one segment vector.
///
/// On the nerd tier, insert a powerline separator between adjacent non-empty
/// groups (and a cap at the cluster's outer edge) coloured to transition between
/// the neighbouring backgrounds. On the other tiers this is a plain flatten.
pub fn powerline_zone(
    widgets: Vec<Vec<Segment>>,
    cluster: Cluster,
    glyphs: &GlyphSet,
) -> Vec<Segment> {
    // One space of internal padding on each side of every widget group, carrying
    // the group's edge background, so content doesn't crowd the powerline arrows
    // (or abut its neighbours on the flat tiers).
    fn pad_group(g: Vec<Segment>) -> Vec<Segment> {
        let space = |bg| Segment {
            text: " ".into(),
            style: ResolvedStyle {
                bg,
                ..Default::default()
            },
            click_action: None,
        };
        let lead = g.first().and_then(|s| s.style.bg);
        let trail = g.last().and_then(|s| s.style.bg);
        let mut out = Vec::with_capacity(g.len() + 2);
        out.push(space(lead));
        out.extend(g);
        out.push(space(trail));
        out
    }
    let groups: Vec<Vec<Segment>> = widgets
        .into_iter()
        .filter(|g| g.iter().any(|s| !s.text.is_empty()))
        .map(pad_group)
        .collect();
    if !glyphs.powerline {
        return groups.into_iter().flatten().collect();
    }
    let sep = match cluster {
        Cluster::Left => glyphs.sep_right,
        Cluster::Right => glyphs.sep_left,
    };
    let bg_of = |g: &[Segment], first: bool| -> Option<crate::Rgb> {
        if first { g.first() } else { g.last() }.and_then(|s| s.style.bg)
    };
    fn arrow(text: &str, fg: Option<crate::Rgb>, bg: Option<crate::Rgb>) -> Segment {
        Segment {
            text: text.into(),
            style: ResolvedStyle {
                fg,
                bg,
                ..Default::default()
            },
            click_action: None,
        }
    }
    let mut out = Vec::new();
    match cluster {
        Cluster::Left => {
            for (i, g) in groups.iter().enumerate() {
                if i > 0 {
                    let prev = bg_of(&groups[i - 1], false);
                    let cur = bg_of(g, true);
                    if prev != cur {
                        // same bg: the separator would be invisible, so skip it.
                        out.push(arrow(sep, prev, cur));
                    }
                }
                out.extend(g.iter().cloned());
            }
            if let Some(last) = groups.last() {
                let lb = bg_of(last, false);
                if lb.is_some() {
                    out.push(arrow(sep, lb, None)); // cap into bar bg
                }
            }
        }
        Cluster::Right => {
            for (i, g) in groups.iter().enumerate() {
                if i == 0 {
                    let cur = bg_of(g, true);
                    if cur.is_some() {
                        out.push(arrow(sep, cur, None)); // leading cap from bar bg
                    }
                } else {
                    let prev = bg_of(&groups[i - 1], false);
                    let cur = bg_of(g, true);
                    if prev != cur {
                        // same bg: the separator would be invisible, so skip it.
                        out.push(arrow(sep, cur, prev));
                    }
                }
                out.extend(g.iter().cloned());
            }
        }
    }
    out
}

/// Resolved glyph table for one tier.
///
/// The single source of truth for every icon and separator the status surface
/// paints. Note that pane borders use plain box-drawing and do NOT consult this.
#[derive(Debug, Clone, Copy)]
pub struct GlyphSet {
    /// True only on the nerd tier, and it gates powerline separator insertion.
    pub powerline: bool,
    /// Left-cluster segment separator (points right, e.g. U+E0B0).
    pub sep_right: &'static str,
    /// Right-cluster segment separator (points left, e.g. U+E0B2).
    pub sep_left: &'static str,
    pub session: &'static str,
    pub prefix: &'static str,
    pub git_branch: &'static str,
    pub cwd: &'static str,
    pub clock: &'static str,
    pub cpu: &'static str,
    pub mem: &'static str,
    pub battery: &'static str,
    pub host: &'static str,
    pub clients: &'static str,
}

impl GlyphSet {
    pub const NERD: Self = Self {
        powerline: true,
        sep_right: "\u{e0b0}",
        sep_left: "\u{e0b2}",
        session: "\u{ebc8}",
        prefix: "\u{f4a1}",
        git_branch: "\u{e0a0}",
        cwd: "\u{f07b}",
        clock: "\u{f017}",
        cpu: "\u{f2db}",
        mem: "\u{efc5}",
        battery: "\u{f240}",
        host: "\u{f108}",
        clients: "\u{f0c0}",
    };
    pub const UNICODE: Self = Self {
        powerline: false,
        sep_right: "",
        sep_left: "",
        session: "\u{25c6}",
        prefix: "\u{25b2}",
        git_branch: "\u{2387}",
        cwd: "\u{25b8}",
        clock: "\u{25f7}",
        cpu: "\u{03bb}",
        mem: "\u{2263}",
        battery: "\u{25ae}",
        host: "@",
        clients: "^",
    };
    pub const ASCII: Self = Self {
        powerline: false,
        sep_right: "",
        sep_left: "",
        session: "*",
        prefix: "^",
        git_branch: "git:",
        cwd: ">",
        clock: "",
        cpu: "cpu:",
        mem: "mem:",
        battery: "bat:",
        host: "@",
        clients: "cl:",
    };

    pub const fn for_tier(tier: GlyphTier) -> &'static Self {
        match tier {
            GlyphTier::Unicode => &Self::UNICODE,
            GlyphTier::Nerd => &Self::NERD,
            GlyphTier::Ascii => &Self::ASCII,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ResolvedStyle, Rgb, Segment};

    fn seg(text: &str, bg: Option<Rgb>) -> Segment {
        Segment {
            text: text.into(),
            style: ResolvedStyle {
                bg,
                ..Default::default()
            },
            click_action: None,
        }
    }
    fn rgb(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }

    #[test]
    fn for_tier_maps_each_variant() {
        assert!(GlyphSet::for_tier(GlyphTier::Nerd).powerline);
        assert!(!GlyphSet::for_tier(GlyphTier::Unicode).powerline);
        assert_eq!(GlyphSet::for_tier(GlyphTier::Ascii).git_branch, "git:");
    }

    #[test]
    fn powerline_off_just_flattens() {
        let zone = vec![vec![seg("a", None)], vec![seg("b", None)]];
        let out = powerline_zone(zone, Cluster::Left, &GlyphSet::UNICODE);
        let joined: String = out.iter().map(|s| s.text.as_str()).collect();
        // No powerline arrows on a flat tier; each group keeps a space of
        // padding on each side.
        assert_eq!(joined, " a  b ");
        assert!(!out.iter().any(|s| s.text == GlyphSet::NERD.sep_right));
    }

    #[test]
    fn powerline_left_inserts_arrow_between_differing_bgs() {
        let a = rgb(10, 10, 10);
        let b = rgb(20, 20, 20);
        let zone = vec![vec![seg("a", Some(a))], vec![seg("b", Some(b))]];
        let out = powerline_zone(zone, Cluster::Left, &GlyphSet::NERD);
        assert!(out.iter().any(|s| s.text == "a"));
        assert!(out.iter().any(|s| s.text == "b"));
        // Two right-arrows in order: the inter-group transition (fg=a bg=b) and
        // the trailing cap into the bar bg (fg=b bg=None).
        let seps: Vec<_> = out
            .iter()
            .filter(|s| s.text == GlyphSet::NERD.sep_right)
            .collect();
        assert_eq!(seps.len(), 2);
        assert_eq!(seps[0].style.fg, Some(a));
        assert_eq!(seps[0].style.bg, Some(b));
        assert_eq!(seps[1].style.fg, Some(b));
        assert_eq!(seps[1].style.bg, None);
    }

    #[test]
    fn powerline_right_inserts_arrow_between_differing_bgs() {
        let a = rgb(10, 10, 10);
        let b = rgb(20, 20, 20);
        let zone = vec![vec![seg("a", Some(a))], vec![seg("b", Some(b))]];
        let out = powerline_zone(zone, Cluster::Right, &GlyphSet::NERD);
        assert!(out.iter().any(|s| s.text == "a"));
        assert!(out.iter().any(|s| s.text == "b"));
        // Two left-arrows in order: the leading cap from the bar bg (fg=a
        // bg=None) and the inter-group transition (fg=cur b, bg=prev a).
        let seps: Vec<_> = out
            .iter()
            .filter(|s| s.text == GlyphSet::NERD.sep_left)
            .collect();
        assert_eq!(seps.len(), 2);
        assert_eq!(seps[0].style.fg, Some(a));
        assert_eq!(seps[0].style.bg, None);
        assert_eq!(seps[1].style.fg, Some(b));
        assert_eq!(seps[1].style.bg, Some(a));
    }

    #[test]
    fn powerline_pads_each_group_with_edge_colored_spaces() {
        let a = rgb(10, 10, 10);
        let zone = vec![vec![seg("x", Some(a))]];
        let out = powerline_zone(zone, Cluster::Left, &GlyphSet::NERD);
        // Leading cell is a space carrying the group bg; "x" is flanked by spaces.
        assert_eq!(out[0].text, " ");
        assert_eq!(out[0].style.bg, Some(a));
        let xi = out
            .iter()
            .position(|s| s.text == "x")
            .expect("content present");
        assert_eq!(out[xi - 1].text, " ");
        assert_eq!(out[xi + 1].text, " ");
    }
}
