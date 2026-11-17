use super::{Appearance, TerminalColors, c, scheme};

/// Which palette terminal panes draw with; each follows the app's light or dark appearance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalTheme {
    #[default]
    Tlsc,
    Catppuccin,
    TokyoNight,
    Gruvbox,
    Dracula,
    Nord,
}

impl TerminalTheme {
    /// Every theme, in the order a picker lists them.
    pub const ALL: [TerminalTheme; 6] = [
        Self::Tlsc,
        Self::Catppuccin,
        Self::TokyoNight,
        Self::Gruvbox,
        Self::Dracula,
        Self::Nord,
    ];

    /// Stable identifier used in settings.
    pub fn id(self) -> &'static str {
        match self {
            Self::Tlsc => "tlsc",
            Self::Catppuccin => "catppuccin",
            Self::TokyoNight => "tokyo-night",
            Self::Gruvbox => "gruvbox",
            Self::Dracula => "dracula",
            Self::Nord => "nord",
        }
    }

    /// The theme whose [`id`](Self::id) is `id`.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.id() == id)
    }

    /// Display name for pickers.
    pub fn name(self) -> &'static str {
        match self {
            Self::Tlsc => "tlsc",
            Self::Catppuccin => "Catppuccin",
            Self::TokyoNight => "Tokyo Night",
            Self::Gruvbox => "Gruvbox",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
        }
    }

    /// The colours this theme uses in `appearance`.
    pub fn palette(self, appearance: Appearance) -> TerminalColors {
        match (self, appearance) {
            (Self::Tlsc, a) => scheme(a).1,
            (Self::Catppuccin, Appearance::Dark) => catppuccin_mocha(),
            (Self::Catppuccin, Appearance::Light) => catppuccin_latte(),
            (Self::TokyoNight, Appearance::Dark) => tokyo_night(),
            (Self::TokyoNight, Appearance::Light) => tokyo_night_day(),
            (Self::Gruvbox, Appearance::Dark) => gruvbox_dark(),
            (Self::Gruvbox, Appearance::Light) => gruvbox_light(),
            (Self::Dracula, _) => dracula(),
            (Self::Nord, _) => nord(),
        }
    }

    /// The appearance the palette was made for; dark-only themes stay dark in light mode.
    pub fn minted_for(self, appearance: Appearance) -> Appearance {
        match self {
            Self::Dracula | Self::Nord => Appearance::Dark,
            _ => appearance,
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

// Ghostty "TokyoNight Night" (iTerm2-Color-Schemes, MIT).
fn tokyo_night() -> TerminalColors {
    imported(
        [
            0xc0caf5, 0x7e84a2, 0x1a1b26, 0xc0caf5, 0x1a1b26, 0x283457, 0xc0caf5,
        ],
        [
            0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
            0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
        ],
    )
}

// Ghostty "TokyoNight Day" (iTerm2-Color-Schemes, MIT).
fn tokyo_night_day() -> TerminalColors {
    imported(
        [
            0x3760bf, 0x3760bf, 0xe1e2e7, 0x3760bf, 0xe1e2e7, 0x99a7df, 0x3760bf,
        ],
        [
            0xe9e9ed, 0xf52a65, 0x587539, 0x8c6c3e, 0x2e7de9, 0x9854f1, 0x007197, 0x6172b0,
            0xa1a6c5, 0xf52a65, 0x587539, 0x8c6c3e, 0x2e7de9, 0x9854f1, 0x007197, 0x3760bf,
        ],
    )
}

// Ghostty "Gruvbox Dark" (iTerm2-Color-Schemes, MIT).
fn gruvbox_dark() -> TerminalColors {
    imported(
        [
            0xebdbb2, 0x999078, 0x282828, 0xebdbb2, 0x282828, 0x665c54, 0xebdbb2,
        ],
        [
            0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
            0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ],
    )
}

// Ghostty "Gruvbox Light" (iTerm2-Color-Schemes, MIT).
fn gruvbox_light() -> TerminalColors {
    imported(
        [
            0x3c3836, 0x716c5f, 0xfbf1c7, 0x3c3836, 0xfbf1c7, 0x3c3836, 0xfbf1c7,
        ],
        [
            0xfbf1c7, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0x7c6f64,
            0x928374, 0x9d0006, 0x79740e, 0xb57614, 0x076678, 0x8f3f71, 0x427b58, 0x3c3836,
        ],
    )
}

// Ghostty "Dracula" (iTerm2-Color-Schemes, MIT).
fn dracula() -> TerminalColors {
    imported(
        [
            0xf8f8f2, 0x9a9b9d, 0x282a36, 0xf8f8f2, 0x282a36, 0x44475a, 0xffffff,
        ],
        [
            0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
            0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
        ],
    )
}

// Ghostty "Nord" (iTerm2-Color-Schemes, MIT).
fn nord() -> TerminalColors {
    imported(
        [
            0xd8dee9, 0x999faa, 0x2e3440, 0xeceff4, 0x282828, 0xeceff4, 0x4c566a,
        ],
        [
            0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
            0x596377, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
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
            assert_eq!(TerminalTheme::Tlsc.palette(a), scheme(a).1);
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
        for t in TerminalTheme::ALL {
            if t.minted_for(Appearance::Light) != Appearance::Light {
                continue;
            }
            let dark = t.palette(Appearance::Dark).background;
            let light = t.palette(Appearance::Light).background;
            assert_ne!(dark, light, "{}", t.id());
            assert!(light.l > dark.l, "{}", t.id());
        }
    }

    #[test]
    fn a_dark_only_theme_stays_dark_in_light_mode() {
        for t in [TerminalTheme::Dracula, TerminalTheme::Nord] {
            assert_eq!(t.palette(Appearance::Light), t.palette(Appearance::Dark));
            assert_eq!(t.minted_for(Appearance::Light), Appearance::Dark);
        }
    }
}
