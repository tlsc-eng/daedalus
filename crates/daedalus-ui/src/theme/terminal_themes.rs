use super::{Appearance, TerminalColors, c, scheme};

/// Which palette terminal panes draw with; each follows the app's light or dark appearance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalTheme {
    #[default]
    Tlsc,
    Catppuccin,
}

impl TerminalTheme {
    pub const ALL: [TerminalTheme; 2] = [Self::Tlsc, Self::Catppuccin];

    pub fn id(self) -> &'static str {
        match self {
            Self::Tlsc => "tlsc",
            Self::Catppuccin => "catppuccin",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.id() == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Tlsc => "tlsc",
            Self::Catppuccin => "Catppuccin",
        }
    }

    pub fn palette(self, appearance: Appearance) -> TerminalColors {
        match (self, appearance) {
            (Self::Tlsc, a) => scheme(a).1,
            (Self::Catppuccin, Appearance::Dark) => catppuccin_mocha(),
            (Self::Catppuccin, Appearance::Light) => catppuccin_latte(),
        }
    }
}

/// Ghostty's palette files give no faint colour, so the dim one here is mixed toward the background.
fn imported(t: [u32; 7], ansi: [u32; 16]) -> TerminalColors {
    let [fg, dim, bg, cursor, cursor_text, selection, selection_fg] = t.map(c);
    TerminalColors {
        foreground: fg,
        bright_foreground: fg,
        dim_foreground: dim,
        background: bg,
        cursor,
        cursor_text,
        selection,
        selection_foreground: Some(selection_fg),
        ansi: ansi.map(c),
    }
}

// Ghostty "Catppuccin Mocha" (iTerm2-Color-Schemes, MIT).
fn catppuccin_mocha() -> TerminalColors {
    imported(
        [
            0xcdd6f4, 0x82879f, 0x1e1e2e, 0xf5e0dc, 0x1e1e2e, 0xf5e0dc, 0x1e1e2e,
        ],
        [
            0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
            0x585b70, 0xf7aec2, 0xc2ecbf, 0xfcd682, 0xaeccfc, 0xf398da, 0xb1eae1, 0xa6adc8,
        ],
    )
}

// Ghostty "Catppuccin Latte" (iTerm2-Color-Schemes, MIT).
fn catppuccin_latte() -> TerminalColors {
    imported(
        [
            0x4c4f69, 0x686b81, 0xeff1f5, 0xdc8a78, 0xeff1f5, 0xdc8a78, 0xeff1f5,
        ],
        [
            0xbcc0cc, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0x5c5f77,
            0xacb0be, 0xe7103f, 0x46b02f, 0xe49931, 0x3878f6, 0xef95d7, 0x19a1a8, 0x6c6f85,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::contrast;

    const BOTH: [Appearance; 2] = [Appearance::Dark, Appearance::Light];

    #[test]
    fn ids_round_trip() {
        for t in TerminalTheme::ALL {
            assert_eq!(TerminalTheme::from_id(t.id()), Some(t));
        }
        let mut ids: Vec<_> = TerminalTheme::ALL.iter().map(|t| t.id()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), TerminalTheme::ALL.len());
        assert_eq!(TerminalTheme::from_id("nope"), None);
    }

    #[test]
    fn tlsc_is_the_default_and_matches_the_app_palette() {
        assert_eq!(TerminalTheme::default(), TerminalTheme::Tlsc);
        for a in BOTH {
            assert!(TerminalTheme::Tlsc.palette(a) == scheme(a).1);
        }
    }

    #[test]
    fn every_theme_keeps_default_text_readable() {
        for t in TerminalTheme::ALL {
            for a in BOTH {
                let p = t.palette(a);
                for (name, fg) in [
                    ("foreground", p.foreground),
                    ("bright", p.bright_foreground),
                    ("dim", p.dim_foreground),
                ] {
                    let ratio = contrast(fg, p.background);
                    assert!(ratio >= 4.5, "{} {a:?} {name}: {ratio}", t.id());
                }
            }
        }
    }

    #[test]
    fn selected_and_cursor_text_stay_visible() {
        for t in TerminalTheme::ALL {
            for a in BOTH {
                let p = t.palette(a);
                let sel = contrast(p.selection_foreground.unwrap_or(p.foreground), p.selection);
                assert!(sel >= 2.0, "{} {a:?} selection: {sel}", t.id());
                let cur = contrast(p.cursor_text, p.cursor);
                assert!(cur >= 2.0, "{} {a:?} cursor: {cur}", t.id());
            }
        }
    }

    #[test]
    fn light_and_dark_variants_follow_the_appearance() {
        let dark = TerminalTheme::Catppuccin
            .palette(Appearance::Dark)
            .background;
        let light = TerminalTheme::Catppuccin
            .palette(Appearance::Light)
            .background;
        assert_ne!(dark, light);
        assert!(light.l > dark.l);
    }
}
