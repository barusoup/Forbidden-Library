use dream::ui::Color;
use dream::ui::theme::{Metrics, Theme};

pub const SIDEBAR_W: f32 = 76.0;
pub const TOPBAR_H: f32 = 66.0;
pub const TILE_GAP: f32 = 2.0;
pub const TILE_RADIUS: f32 = 0.0;

/// A quiet monochrome palette. Dark is the default Google Photos-like
/// surface: near-black chrome, square photo tiles and one soft grey for
/// pressed and hovered states. Light is the same hierarchy inverted. No
/// gradients anywhere.
pub fn archive_theme(dark: bool) -> Theme {
    let mut theme = if dark {
        Theme::dark_theme()
    } else {
        Theme::light()
    };
    if dark {
        theme.bg = Color::rgb(0x0b, 0x0b, 0x0c);
        theme.g1 = Color::rgb(0x15, 0x15, 0x17);
        theme.g2 = Color::rgb(0x20, 0x21, 0x24);
        theme.g3 = Color::rgb(0x2c, 0x2d, 0x31);
        theme.pressed = Color::rgb(0x37, 0x38, 0x3c);
        theme.pane = theme.g1;
        theme.pane_alpha_bg = theme.g1;
        theme.pane_alpha_g2 = theme.g2;
        theme.fg = Color::rgb(0xf1, 0xf1, 0xf2);
        theme.muted = Color::rgb(0xa2, 0xa2, 0xa7);
        theme.faint = Color::rgb(0x73, 0x73, 0x78);
        theme.border = Color::rgb(0x23, 0x23, 0x26);
        theme.accent = Color::rgb(0xf1, 0xf1, 0xf2);
        theme.accent_dim = Color::rgb(0x86, 0x86, 0x8c);
        theme.accent_text = Color::rgb(0x0b, 0x0b, 0x0c);
        theme.danger = Color::rgb(0xd8, 0x6a, 0x6a);
        theme.ok = Color::rgb(0x9d, 0xc4, 0x9d);
        theme.info = theme.muted;
    } else {
        theme.bg = Color::rgb(0xff, 0xff, 0xff);
        theme.g1 = Color::rgb(0xf7, 0xf8, 0xf9);
        theme.g2 = Color::rgb(0xee, 0xf0, 0xf2);
        theme.g3 = Color::rgb(0xe3, 0xe6, 0xe9);
        theme.pressed = Color::rgb(0xda, 0xdc, 0xe0);
        theme.pane = theme.bg;
        theme.pane_alpha_bg = theme.bg;
        theme.pane_alpha_g2 = theme.g2;
        theme.fg = Color::rgb(0x20, 0x21, 0x24);
        theme.muted = Color::rgb(0x5f, 0x63, 0x68);
        theme.faint = Color::rgb(0x80, 0x86, 0x8b);
        theme.border = Color::rgb(0xe6, 0xe8, 0xeb);
        theme.accent = Color::rgb(0x20, 0x21, 0x24);
        theme.accent_dim = Color::rgb(0x5f, 0x63, 0x68);
        theme.accent_text = Color::rgb(0xff, 0xff, 0xff);
        theme.danger = Color::rgb(0xa8, 0x3a, 0x3a);
        theme.ok = Color::rgb(0x2f, 0x6b, 0x46);
        theme.info = theme.muted;
    }
    theme.chk = [theme.g2, theme.g3];
    let metrics = Metrics {
        radius: 8.0,
        radius_lg: 12.0,
        border: 1.0,
        gap: 8.0,
        pad: 16.0,
        titlebar_h: 36.0,
        toolbar_h: TOPBAR_H,
        rail_w: SIDEBAR_W,
        inspector_w: 320.0,
        button: 36.0,
        text_button_w: 56.0,
        control_h: 38.0,
        row_gap: 8.0,
        ..Metrics::default()
    };
    theme
        .with_metrics(metrics)
        .with_accent(theme.accent, theme.accent_text)
}

/// Fade a colour by a frame alpha (0..1).
pub fn fade(color: Color, alpha: f32) -> Color {
    color.with_alpha((color.a as f32 * alpha.clamp(0.0, 1.0)).round() as u8)
}

pub fn hover_overlay(dark: bool, amount: f32) -> Color {
    if dark {
        Color::rgba(255, 255, 255, (16.0 * amount) as u8)
    } else {
        Color::rgba(0, 0, 0, (10.0 * amount) as u8)
    }
}

/// Ease used for page and overlay entrances.
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

pub fn ease_out_quint(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(5)
}
