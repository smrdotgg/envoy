//! Envoy's look: a rune-kit `Kit` built from the theme and accent settings, plus a few
//! colours of our own (selection tint, unread dot, avatar hues).

use rune::{Color, oklch};
use rune_kit::{Kit, PaletteFocus, PaletteSeed};

use envoy_core::settings::Settings;

#[derive(Clone)]
pub struct Theme {
    pub k: Kit,
    pub dark: bool,
    pub accent: Color,
    /// Selected row fill.
    pub selected: Color,
    /// Unread marker and star.
    pub unread: Color,
    pub star: Color,
    /// The sidebar / window background behind the framed content.
    pub page: Color,
    /// The framed content card.
    pub card: Color,
    pub compact: bool,
}

fn accent_hue(name: &str) -> (f32, f32) {
    match name {
        "violet" => (295.0, 0.16),
        "teal" => (195.0, 0.11),
        "green" => (150.0, 0.14),
        "orange" => (50.0, 0.16),
        "pink" => (350.0, 0.16),
        "graphite" => (260.0, 0.015),
        _ => (258.0, 0.17),
    }
}

pub fn build(settings: &Settings, system_dark: bool) -> Theme {
    let dark = match settings.str("appearance.theme").as_str() {
        "light" => false,
        "dark" => true,
        _ => system_dark,
    };
    let (hue, chroma) = accent_hue(&settings.str("appearance.accent"));
    let accent = if dark { oklch(0.72, chroma * 0.9, hue) } else { oklch(0.56, chroma, hue) };
    let (card, page, fg) = if dark {
        (oklch(0.205, 0.006, 265.0), oklch(0.165, 0.006, 265.0), oklch(0.93, 0.004, 265.0))
    } else {
        (Color::WHITE, oklch(0.968, 0.008, 255.0), oklch(0.24, 0.01, 265.0))
    };
    let mut k = Kit::from_palette_focus(PaletteSeed { bg: card, fg, accent }, PaletteFocus::Accent);
    k.c.page = page;
    k.c.bg_deep = page;
    k.radius.sm = 5.0;
    k.radius.md = 8.0;
    k.radius.lg = 12.0;
    let selected = if dark { oklch(0.30, chroma * 0.35, hue) } else { oklch(0.93, chroma * 0.25, hue) };
    Theme {
        k,
        dark,
        accent,
        selected,
        unread: accent,
        star: if dark { oklch(0.82, 0.15, 85.0) } else { oklch(0.76, 0.16, 75.0) },
        page,
        card,
        compact: settings.str("appearance.density") == "compact",
    }
}

/// A stable colour for a sender, from their address.
pub fn avatar_color(seed: &str, dark: bool) -> Color {
    let mut h: u32 = 2166136261;
    for b in seed.to_lowercase().bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    let hue = (h % 360) as f32;
    if dark { oklch(0.62, 0.12, hue) } else { oklch(0.64, 0.13, hue) }
}
