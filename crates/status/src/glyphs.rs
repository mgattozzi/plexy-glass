use plexy_glass_config::GlyphTier;

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
    pub const NERD: GlyphSet = GlyphSet {
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
    pub const UNICODE: GlyphSet = GlyphSet {
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
    pub const ASCII: GlyphSet = GlyphSet {
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

    pub fn for_tier(tier: GlyphTier) -> &'static GlyphSet {
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
    #[test]
    fn for_tier_maps_each_variant() {
        assert!(GlyphSet::for_tier(GlyphTier::Nerd).powerline);
        assert!(!GlyphSet::for_tier(GlyphTier::Unicode).powerline);
        assert_eq!(GlyphSet::for_tier(GlyphTier::Ascii).git_branch, "git:");
    }
}
