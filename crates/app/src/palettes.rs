//! The palettes the interface can be drawn in.
//!
//! The first is the one a new user gets: the greys and the blue-violet
//! that chat apps have made familiar. The second is our own, from before
//! that. The rest are colour schemes people know from their editors and
//! terminals, with each scheme's own colours put to the uses we have for
//! them. Where a scheme
//! has no colour that suits a use, such as text dim enough to step back
//! and still bright enough to read, one is made from those it has.
//!
//! To add one, add it to [`ALL`]. The tests below then hold it to what
//! every palette has to manage: text that can be read on everything it is
//! written on.

use egui::Color32;

use crate::theme::{Colors, Palette};

/// The palette used until another is chosen.
pub fn default() -> &'static Palette {
    &ALL[0]
}

/// The palette saved as `id`, if there is one.
pub fn by_id(id: &str) -> Option<&'static Palette> {
    ALL.iter().find(|palette| palette.id == id)
}

/// A colour as it is written in the schemes' own documentation.
const fn c(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Every palette, in the order they are offered.
pub static ALL: [Palette; 24] = [
    Palette {
        id: "blurple",
        name: "Blurple",
        light: false,
        colors: Colors {
            background: c(0x313338),
            stage: c(0x111214),
            panel: c(0x2B2D31),
            field: c(0x1E1F22),
            control: c(0x3A3C42),
            control_hover: c(0x43464D),
            border: c(0x3F4147),
            hairline: c(0x3A3C42),
            text: c(0xDBDEE1),
            muted: c(0xA3A8B0),
            idle: c(0x80848E),
            accent: c(0x6C78F4),
            on_accent: c(0x111214),
            warn: c(0xF0B232),
            on_warn: c(0x1E1F22),
        },
    },
    Palette {
        id: "flowerchat",
        name: "Flowerchat",
        light: false,
        colors: Colors {
            background: c(0x15171B),
            stage: c(0x0B0C0E),
            panel: c(0x1A1C21),
            field: c(0x1D2025),
            control: c(0x23272E),
            control_hover: c(0x2C3038),
            border: c(0x30353E),
            hairline: c(0x23272E),
            text: c(0xE8EAED),
            muted: c(0xA3AAB5),
            idle: c(0x6B7280),
            accent: c(0x4CC38A),
            on_accent: c(0x0E1512),
            warn: c(0xF2994A),
            on_warn: c(0x1A1206),
        },
    },
    Palette {
        id: "gruvbox-dark",
        name: "Gruvbox Dark",
        light: false,
        colors: Colors {
            background: c(0x282828),
            stage: c(0x1D2021),
            panel: c(0x32302F),
            field: c(0x1D2021),
            control: c(0x3C3836),
            control_hover: c(0x504945),
            border: c(0x504945),
            hairline: c(0x3C3836),
            text: c(0xEBDBB2),
            muted: c(0xA89984),
            idle: c(0x7C6F64),
            accent: c(0xB8BB26),
            on_accent: c(0x1D2021),
            warn: c(0xFE8019),
            on_warn: c(0x1D2021),
        },
    },
    // Gruvbox on its hardest background, with the yellow that a desktop
    // themed in it puts around the window in focus.
    Palette {
        id: "gruvbox-darker",
        name: "Gruvbox Darker",
        light: false,
        colors: Colors {
            background: c(0x1D2021),
            stage: c(0x141617),
            panel: c(0x1D2021),
            field: c(0x141617),
            control: c(0x282828),
            control_hover: c(0x3C3836),
            border: c(0x3C3836),
            hairline: c(0x282828),
            text: c(0xEBDBB2),
            muted: c(0xA89984),
            idle: c(0x7C6F64),
            accent: c(0xFABD2F),
            on_accent: c(0x1D2021),
            warn: c(0xFB4934),
            on_warn: c(0x1D2021),
        },
    },
    Palette {
        id: "gruvbox-light",
        name: "Gruvbox Light",
        light: true,
        colors: Colors {
            background: c(0xFBF1C7),
            stage: c(0xEBDBB2),
            panel: c(0xF2E5BC),
            field: c(0xF9F5D7),
            control: c(0xEBDBB2),
            control_hover: c(0xD5C4A1),
            border: c(0xD5C4A1),
            hairline: c(0xEBDBB2),
            text: c(0x3C3836),
            muted: c(0x665C54),
            idle: c(0xA89984),
            accent: c(0x79740E),
            on_accent: c(0xFBF1C7),
            warn: c(0xAF3A03),
            on_warn: c(0xFBF1C7),
        },
    },
    Palette {
        id: "one-half-dark",
        name: "One Half Dark",
        light: false,
        colors: Colors {
            background: c(0x282C34),
            stage: c(0x1E2127),
            panel: c(0x21252B),
            field: c(0x1E2127),
            control: c(0x313640),
            control_hover: c(0x3E4452),
            border: c(0x3E4452),
            hairline: c(0x313640),
            text: c(0xDCDFE4),
            muted: c(0x9DA5B4),
            idle: c(0x5C6370),
            accent: c(0x61AFEF),
            on_accent: c(0x1E2127),
            warn: c(0xE5C07B),
            on_warn: c(0x1E2127),
        },
    },
    Palette {
        id: "one-half-light",
        name: "One Half Light",
        light: true,
        colors: Colors {
            background: c(0xFAFAFA),
            stage: c(0xE5E5E5),
            panel: c(0xF0F0F0),
            field: c(0xFFFFFF),
            control: c(0xE5E5E5),
            control_hover: c(0xD4D4D4),
            border: c(0xD4D4D4),
            hairline: c(0xE5E5E5),
            text: c(0x383A42),
            muted: c(0x696C77),
            idle: c(0xA0A1A7),
            accent: c(0x0184BC),
            on_accent: c(0xFFFFFF),
            warn: c(0x986801),
            on_warn: c(0xFFFFFF),
        },
    },
    Palette {
        id: "monokai",
        name: "Monokai",
        light: false,
        colors: Colors {
            background: c(0x272822),
            stage: c(0x191A17),
            panel: c(0x1E1F1C),
            field: c(0x30312A),
            control: c(0x3E3D32),
            control_hover: c(0x4E4D42),
            border: c(0x49483E),
            hairline: c(0x3E3D32),
            text: c(0xF8F8F2),
            muted: c(0xA59F85),
            idle: c(0x75715E),
            accent: c(0xA6E22E),
            on_accent: c(0x1E1F1C),
            warn: c(0xFD971F),
            on_warn: c(0x1E1F1C),
        },
    },
    Palette {
        id: "catppuccin-latte",
        name: "Catppuccin Latte",
        light: true,
        colors: Colors {
            background: c(0xEFF1F5),
            stage: c(0xDCE0E8),
            panel: c(0xE6E9EF),
            field: c(0xEFF1F5),
            control: c(0xDCE0E8),
            control_hover: c(0xCCD0DA),
            border: c(0xCCD0DA),
            hairline: c(0xDCE0E8),
            text: c(0x4C4F69),
            muted: c(0x5C5F77),
            idle: c(0x9CA0B0),
            accent: c(0x8839EF),
            on_accent: c(0xEFF1F5),
            warn: c(0xD20F39),
            on_warn: c(0xEFF1F5),
        },
    },
    Palette {
        id: "catppuccin-frappe",
        name: "Catppuccin Frappé",
        light: false,
        colors: Colors {
            background: c(0x303446),
            stage: c(0x232634),
            panel: c(0x292C3C),
            field: c(0x232634),
            control: c(0x414559),
            control_hover: c(0x51576D),
            border: c(0x414559),
            hairline: c(0x414559),
            text: c(0xC6D0F5),
            muted: c(0xA5ADCE),
            idle: c(0x737994),
            accent: c(0xCA9EE6),
            on_accent: c(0x232634),
            warn: c(0xEF9F76),
            on_warn: c(0x232634),
        },
    },
    Palette {
        id: "catppuccin-macchiato",
        name: "Catppuccin Macchiato",
        light: false,
        colors: Colors {
            background: c(0x24273A),
            stage: c(0x181926),
            panel: c(0x1E2030),
            field: c(0x181926),
            control: c(0x363A4F),
            control_hover: c(0x494D64),
            border: c(0x363A4F),
            hairline: c(0x363A4F),
            text: c(0xCAD3F5),
            muted: c(0xA5ADCB),
            idle: c(0x6E738D),
            accent: c(0xC6A0F6),
            on_accent: c(0x181926),
            warn: c(0xF5A97F),
            on_warn: c(0x181926),
        },
    },
    Palette {
        id: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        light: false,
        colors: Colors {
            background: c(0x1E1E2E),
            stage: c(0x11111B),
            panel: c(0x181825),
            field: c(0x11111B),
            control: c(0x313244),
            control_hover: c(0x45475A),
            border: c(0x313244),
            hairline: c(0x313244),
            text: c(0xCDD6F4),
            muted: c(0xA6ADC8),
            idle: c(0x6C7086),
            accent: c(0xCBA6F7),
            on_accent: c(0x11111B),
            warn: c(0xFAB387),
            on_warn: c(0x11111B),
        },
    },
    Palette {
        id: "dracula",
        name: "Dracula",
        light: false,
        colors: Colors {
            background: c(0x282A36),
            stage: c(0x191A21),
            panel: c(0x21222C),
            field: c(0x191A21),
            control: c(0x343746),
            control_hover: c(0x44475A),
            border: c(0x44475A),
            hairline: c(0x343746),
            text: c(0xF8F8F2),
            muted: c(0xA7AFD0),
            idle: c(0x6272A4),
            accent: c(0xBD93F9),
            on_accent: c(0x191A21),
            warn: c(0xFFB86C),
            on_warn: c(0x191A21),
        },
    },
    Palette {
        id: "nord",
        name: "Nord",
        light: false,
        colors: Colors {
            background: c(0x2E3440),
            stage: c(0x20242C),
            panel: c(0x292E39),
            field: c(0x242933),
            control: c(0x3B4252),
            control_hover: c(0x434C5E),
            border: c(0x434C5E),
            hairline: c(0x3B4252),
            text: c(0xE5E9F0),
            muted: c(0xA3ADBF),
            idle: c(0x616E88),
            accent: c(0x88C0D0),
            on_accent: c(0x2E3440),
            warn: c(0xD08770),
            on_warn: c(0x2E3440),
        },
    },
    Palette {
        id: "tokyo-night",
        name: "Tokyo Night",
        light: false,
        colors: Colors {
            background: c(0x1A1B26),
            stage: c(0x0F0F14),
            panel: c(0x16161E),
            field: c(0x14141B),
            control: c(0x292E42),
            control_hover: c(0x3B4261),
            border: c(0x292E42),
            hairline: c(0x292E42),
            text: c(0xC0CAF5),
            muted: c(0x9AA5CE),
            idle: c(0x565F89),
            accent: c(0x7AA2F7),
            on_accent: c(0x16161E),
            warn: c(0xFF9E64),
            on_warn: c(0x16161E),
        },
    },
    Palette {
        id: "solarized-dark",
        name: "Solarized Dark",
        light: false,
        colors: Colors {
            background: c(0x002B36),
            stage: c(0x00212B),
            panel: c(0x073642),
            field: c(0x00212B),
            control: c(0x114B58),
            control_hover: c(0x1B5A68),
            border: c(0x1A4F5B),
            hairline: c(0x0E3F4B),
            text: c(0xEEE8D5),
            muted: c(0x93A1A1),
            idle: c(0x586E75),
            accent: c(0x2AA198),
            on_accent: c(0x002B36),
            warn: c(0xB58900),
            on_warn: c(0x002B36),
        },
    },
    Palette {
        id: "solarized-light",
        name: "Solarized Light",
        light: true,
        colors: Colors {
            background: c(0xFDF6E3),
            stage: c(0xEEE8D5),
            panel: c(0xEEE8D5),
            field: c(0xFFFBEE),
            control: c(0xE4DDC9),
            control_hover: c(0xD9D2BD),
            border: c(0xD9D2BD),
            hairline: c(0xE4DDC9),
            text: c(0x073642),
            muted: c(0x586E75),
            idle: c(0x93A1A1),
            accent: c(0x6C71C4),
            on_accent: c(0xFDF6E3),
            warn: c(0xCB4B16),
            on_warn: c(0xFDF6E3),
        },
    },
    Palette {
        id: "rose-pine",
        name: "Rosé Pine",
        light: false,
        colors: Colors {
            background: c(0x191724),
            stage: c(0x12101A),
            panel: c(0x1F1D2E),
            field: c(0x13111C),
            control: c(0x26233A),
            control_hover: c(0x403D52),
            border: c(0x26233A),
            hairline: c(0x26233A),
            text: c(0xE0DEF4),
            muted: c(0x908CAA),
            idle: c(0x6E6A86),
            accent: c(0xEBBCBA),
            on_accent: c(0x191724),
            warn: c(0xF6C177),
            on_warn: c(0x191724),
        },
    },
    Palette {
        id: "rose-pine-dawn",
        name: "Rosé Pine Dawn",
        light: true,
        colors: Colors {
            background: c(0xFAF4ED),
            stage: c(0xF2E9E1),
            panel: c(0xF4EDE8),
            field: c(0xFFFAF3),
            control: c(0xDFDAD9),
            control_hover: c(0xD3CFD1),
            border: c(0xDFDAD9),
            hairline: c(0xDFDAD9),
            text: c(0x575279),
            muted: c(0x6E6A86),
            idle: c(0x9893A5),
            accent: c(0x286983),
            on_accent: c(0xFAF4ED),
            warn: c(0xB4637A),
            on_warn: c(0xFFFAF3),
        },
    },
    Palette {
        id: "everforest",
        name: "Everforest",
        light: false,
        colors: Colors {
            background: c(0x2D353B),
            stage: c(0x1E2326),
            panel: c(0x272E33),
            field: c(0x232A2E),
            control: c(0x3D484D),
            control_hover: c(0x475258),
            border: c(0x3D484D),
            hairline: c(0x343F44),
            text: c(0xD3C6AA),
            muted: c(0x9DA9A0),
            idle: c(0x7A8478),
            accent: c(0xA7C080),
            on_accent: c(0x232A2E),
            warn: c(0xE69875),
            on_warn: c(0x232A2E),
        },
    },
    Palette {
        id: "kanagawa",
        name: "Kanagawa",
        light: false,
        colors: Colors {
            background: c(0x1F1F28),
            stage: c(0x16161D),
            panel: c(0x181820),
            field: c(0x16161D),
            control: c(0x2A2A37),
            control_hover: c(0x363646),
            border: c(0x2A2A37),
            hairline: c(0x2A2A37),
            text: c(0xDCD7BA),
            muted: c(0xA6A69C),
            idle: c(0x727169),
            accent: c(0x7E9CD8),
            on_accent: c(0x16161D),
            warn: c(0xFFA066),
            on_warn: c(0x16161D),
        },
    },
    Palette {
        id: "synthwave-84",
        name: "Synthwave '84",
        light: false,
        colors: Colors {
            background: c(0x262335),
            stage: c(0x1A1626),
            panel: c(0x241B2F),
            field: c(0x1D1A2B),
            control: c(0x34294F),
            control_hover: c(0x463465),
            border: c(0x34294F),
            hairline: c(0x34294F),
            text: c(0xF5F2FF),
            muted: c(0x9699C9),
            idle: c(0x6A6398),
            accent: c(0xFF7EDB),
            on_accent: c(0x241B2F),
            warn: c(0xFEDE5D),
            on_warn: c(0x241B2F),
        },
    },
    // A terminal from before screens had colours, with amber, the other
    // colour such screens came in, for what should not be overlooked.
    Palette {
        id: "green-phosphor",
        name: "Green Phosphor",
        light: false,
        colors: Colors {
            background: c(0x020A04),
            stage: c(0x000000),
            panel: c(0x04110A),
            field: c(0x000500),
            control: c(0x0A2613),
            control_hover: c(0x10391D),
            border: c(0x0F3518),
            hairline: c(0x0A2613),
            text: c(0x33FF66),
            muted: c(0x1FB84A),
            idle: c(0x147A32),
            accent: c(0x9DFFB5),
            on_accent: c(0x020A04),
            warn: c(0xFFB000),
            on_warn: c(0x020A04),
        },
    },
    // After the colour scheme of that name in Windows 3.1, which nobody
    // is known to have used on purpose. White on red was hard to read then
    // too, so the text is black here.
    Palette {
        id: "hot-dog-stand",
        name: "Hot Dog Stand",
        light: true,
        colors: Colors {
            background: c(0xFF0000),
            stage: c(0xFFFFFF),
            panel: c(0xFFFF00),
            field: c(0xFFFFFF),
            control: c(0xC0C0C0),
            control_hover: c(0xDFDFDF),
            border: c(0x000000),
            hairline: c(0x000000),
            text: c(0x000000),
            muted: c(0x3A0000),
            idle: c(0x7A0000),
            accent: c(0x000000),
            on_accent: c(0xFFFF00),
            warn: c(0x0000AA),
            on_warn: c(0xFFFFFF),
        },
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// How far apart two colours are in brightness, as the Web Content
    /// Accessibility Guidelines count it: from 1, the same, to 21, black
    /// on white. Body text is asked to reach 4.5 and large shapes 3.
    fn contrast(a: Color32, b: Color32) -> f64 {
        fn luminance(color: Color32) -> f64 {
            let linear = |channel: u8| {
                let value = f64::from(channel) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
        }
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn palettes_are_told_apart_by_id_and_by_name() {
        for (index, palette) in ALL.iter().enumerate() {
            for other in &ALL[index + 1..] {
                assert_ne!(palette.id, other.id);
                assert_ne!(palette.name, other.name);
            }
            assert_eq!(by_id(palette.id), Some(palette));
        }
        assert_eq!(by_id("no such palette"), None);
        assert_eq!(default().id, "blurple");
    }

    #[test]
    fn every_palette_can_be_read() {
        let mut failures = Vec::new();
        for palette in &ALL {
            let colors = &palette.colors;
            let mut check = |what: &str, front: Color32, back: Color32, least: f64| {
                let found = contrast(front, back);
                if found < least {
                    failures.push(format!(
                        "{}: {what} has a contrast of {found:.2}, below {least}",
                        palette.name
                    ));
                }
            };
            for (surface, back) in [
                ("the background", colors.background),
                ("a panel", colors.panel),
                ("a text field", colors.field),
                ("a button", colors.control),
                ("a button under the pointer", colors.control_hover),
            ] {
                check(&format!("text on {surface}"), colors.text, back, 4.5);
            }
            // Dimmer on purpose, and still for reading.
            for (surface, back) in [
                ("the background", colors.background),
                ("a panel", colors.panel),
                ("the stage", colors.stage),
            ] {
                check(&format!("muted text on {surface}"), colors.muted, back, 4.0);
            }
            // Meters, indicators and a few words in a heavier weight.
            for (surface, back) in [
                ("the background", colors.background),
                ("a panel", colors.panel),
            ] {
                check(
                    &format!("the accent on {surface}"),
                    colors.accent,
                    back,
                    3.0,
                );
                check(&format!("a warning on {surface}"), colors.warn, back, 3.0);
            }
            check("text on the accent", colors.on_accent, colors.accent, 4.0);
            check("text on a warning", colors.on_warn, colors.warn, 4.0);
            let dark_text_suits_it = contrast(colors.background, Color32::BLACK)
                > contrast(colors.background, Color32::WHITE);
            if palette.light != dark_text_suits_it {
                failures.push(format!("{}: is it light or dark?", palette.name));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
