use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Sense, Stroke, Theme, ThemePreference, Ui, Vec2,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub card: Color32,
    pub card_hover: Color32,
    pub track: Color32,
    pub text: Color32,
    pub dim: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub on_accent: Color32,
    pub border: Color32,
    pub chip_bg: Color32,
    pub titlebar: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub danger: Color32,
    pub info: Color32,
    pub close_hover: Color32,
}

/// Monochrome palettes: near-black / near-white surfaces with the accent
/// being the opposite pole of the theme (white on black, black on white).
/// Semantic colours stay, but desaturated, so they sit quietly in the grey
/// field instead of fighting it.
pub const DARK: Palette = Palette {
    dark: true,
    bg: Color32::from_rgb(10, 10, 10),
    card: Color32::from_rgb(20, 20, 20),
    card_hover: Color32::from_rgb(28, 28, 28),
    track: Color32::from_rgb(38, 38, 38),
    text: Color32::from_rgb(250, 250, 250),
    dim: Color32::from_rgb(163, 163, 163),
    accent: Color32::from_rgb(245, 245, 245),
    accent_hover: Color32::from_rgb(255, 255, 255),
    on_accent: Color32::from_rgb(12, 12, 12),
    border: Color32::from_rgb(46, 46, 46),
    chip_bg: Color32::from_rgb(26, 26, 26),
    titlebar: Color32::from_rgb(10, 10, 10),
    ok: Color32::from_rgb(74, 199, 132),
    warn: Color32::from_rgb(228, 172, 82),
    danger: Color32::from_rgb(232, 102, 102),
    info: Color32::from_rgb(128, 166, 236),
    close_hover: Color32::from_rgb(196, 64, 64),
};

pub const LIGHT: Palette = Palette {
    dark: false,
    bg: Color32::from_rgb(250, 250, 250),
    card: Color32::from_rgb(255, 255, 255),
    card_hover: Color32::from_rgb(245, 245, 245),
    track: Color32::from_rgb(229, 229, 229),
    text: Color32::from_rgb(23, 23, 23),
    dim: Color32::from_rgb(115, 115, 115),
    accent: Color32::from_rgb(23, 23, 23),
    accent_hover: Color32::from_rgb(0, 0, 0),
    on_accent: Color32::from_rgb(250, 250, 250),
    border: Color32::from_rgb(229, 229, 229),
    chip_bg: Color32::from_rgb(245, 245, 245),
    titlebar: Color32::from_rgb(250, 250, 250),
    ok: Color32::from_rgb(22, 138, 84),
    warn: Color32::from_rgb(182, 112, 10),
    danger: Color32::from_rgb(203, 52, 52),
    info: Color32::from_rgb(47, 105, 205),
    close_hover: Color32::from_rgb(229, 72, 77),
};

/// Same colour with a different alpha, for washes and tinted surfaces.
pub fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub fn palette(mode: ThemeMode) -> &'static Palette {
    match mode {
        ThemeMode::Dark => &DARK,
        ThemeMode::Light => &LIGHT,
    }
}

pub fn apply(ctx: &egui::Context, mode: ThemeMode) {
    let p = palette(mode);
    ctx.set_theme(match mode {
        ThemeMode::Dark => ThemePreference::Dark,
        ThemeMode::Light => ThemePreference::Light,
    });
    let v = visuals(p);
    ctx.set_visuals_of(Theme::Dark, v.clone());
    ctx.set_visuals_of(Theme::Light, v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 8.0);
        s.spacing.button_padding = Vec2::new(12.0, 7.0);
        s.spacing.interact_size.y = 30.0;
        // Thin scroll bars that stay visible and expand on hover.
        s.spacing.scroll = egui::style::ScrollStyle::thin();
    });
}

/// Soft drop shadow for cards; only meaningful in light mode where the
/// background lacks the natural separation dark mode gets from borders.
pub fn card_shadow(p: &Palette) -> egui::Shadow {
    if p.dark {
        egui::Shadow::NONE
    } else {
        egui::Shadow {
            offset: [0, 1],
            blur: 8,
            spread: 0,
            color: Color32::from_black_alpha(16),
        }
    }
}

fn visuals(p: &Palette) -> egui::Visuals {
    let mut v = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.panel_fill = p.bg;
    v.window_fill = p.bg;
    v.faint_bg_color = p.chip_bg;
    v.extreme_bg_color = p.track;
    v.override_text_color = Some(p.text);
    v.selection.bg_fill = p.accent.gamma_multiply(0.3);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.widgets.inactive.bg_fill = p.chip_bg;
    v.widgets.inactive.weak_bg_fill = p.chip_bg;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.corner_radius = CornerRadius::same(8);
    v.widgets.hovered.bg_fill = p.card_hover;
    v.widgets.hovered.weak_bg_fill = p.card_hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.dim.gamma_multiply(0.5));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.hovered.corner_radius = CornerRadius::same(8);
    v.widgets.active.bg_fill = p.track;
    v.widgets.active.weak_bg_fill = p.track;
    v.widgets.active.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.active.corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_corner_radius = CornerRadius::same(12);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.popup_shadow = egui::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 80 } else { 30 }),
    };
    v
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatteryLevel {
    Good,
    Low,
    Critical,
    Wired,
    Unknown,
}

pub fn battery_level(battery: Option<u8>, wired: bool) -> BatteryLevel {
    match battery {
        Some(b) if b >= 60 => BatteryLevel::Good,
        Some(b) if b >= 20 => BatteryLevel::Low,
        Some(_) => BatteryLevel::Critical,
        None if wired => BatteryLevel::Wired,
        None => BatteryLevel::Unknown,
    }
}

pub fn battery_color(level: BatteryLevel, p: &Palette) -> Color32 {
    match level {
        BatteryLevel::Good => p.ok,
        BatteryLevel::Low => p.warn,
        BatteryLevel::Critical => p.danger,
        BatteryLevel::Wired => p.info,
        BatteryLevel::Unknown => p.dim,
    }
}

pub fn battery_label(level: BatteryLevel) -> &'static str {
    match level {
        BatteryLevel::Good => "电量充足",
        BatteryLevel::Low => "电量偏低",
        BatteryLevel::Critical => "电量低",
        BatteryLevel::Wired => "有线供电",
        BatteryLevel::Unknown => "电量未知",
    }
}

/// Brand mark drawn with egui primitives: ink tile with a "P" monogram.
pub fn brand_badge(ui: &mut Ui, size: f32, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same((size * 0.28) as u8), p.accent);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        "P",
        FontId::proportional(size * 0.62),
        p.on_accent,
    );
}

/// RGBA icon used for the window/tray icon, drawn pixel by pixel.
/// A near-black tile with a white "P"; the faint light rim keeps the tile
/// readable when it sits on a dark taskbar.
pub fn icon_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let ink = [16.0, 16.0, 16.0];
    let glyph = [245.0, 245.0, 245.0];
    let rim = [255.0, 255.0, 255.0];
    let rim_a = 0.16;
    let rim_w = (s * 0.05).max(1.0);
    let radius = s * 0.28;
    let stem = (s * 0.34, s * 0.24, s * 0.42, s * 0.76);
    let bowl_out = (s * 0.34, s * 0.24, s * 0.68, s * 0.52);
    let bowl_in = (s * 0.44, s * 0.34, s * 0.58, s * 0.42);
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    let samples = 3;
    for y in 0..size {
        for x in 0..size {
            let mut acc = [0f32; 4];
            for sy in 0..samples {
                for sx in 0..samples {
                    let px = x as f32 + (sx as f32 + 0.5) / samples as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / samples as f32;
                    let tile = rounded_contains(px, py, 0.0, 0.0, s, s, radius);
                    if !tile {
                        continue;
                    }
                    let inner = rounded_contains(
                        px,
                        py,
                        rim_w,
                        rim_w,
                        s - rim_w,
                        s - rim_w,
                        (radius - rim_w).max(0.0),
                    );
                    let p_glyph = rect_contains(px, py, stem)
                        || (rounded_contains(
                            px,
                            py,
                            bowl_out.0,
                            bowl_out.1,
                            bowl_out.2,
                            bowl_out.3,
                            s * 0.14,
                        ) && !rounded_contains(
                            px,
                            py,
                            bowl_in.0,
                            bowl_in.1,
                            bowl_in.2,
                            bowl_in.3,
                            s * 0.06,
                        ));
                    // Priority: glyph > rim ring > tile body.
                    let (c, a) = if p_glyph {
                        (glyph, 255.0)
                    } else if !inner {
                        (rim, 255.0 * rim_a)
                    } else {
                        (ink, 255.0)
                    };
                    acc[0] += c[0];
                    acc[1] += c[1];
                    acc[2] += c[2];
                    acc[3] += a;
                }
            }
            let total = (samples * samples) as f32;
            let a = acc[3] / total;
            if a <= 0.0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                out.push((acc[0] / total * 255.0 / a).clamp(0.0, 255.0) as u8);
                out.push((acc[1] / total * 255.0 / a).clamp(0.0, 255.0) as u8);
                out.push((acc[2] / total * 255.0 / a).clamp(0.0, 255.0) as u8);
                out.push(a.clamp(0.0, 255.0) as u8);
            }
        }
    }
    out
}

fn rect_contains(x: f32, y: f32, r: (f32, f32, f32, f32)) -> bool {
    x >= r.0 && x <= r.2 && y >= r.1 && y <= r.3
}

fn rounded_contains(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    let radius = radius.min((x1 - x0) * 0.5).min((y1 - y0) * 0.5).max(0.0);
    let cx = x.clamp(x0 + radius, x1 - radius);
    let cy = y.clamp(y0 + radius, y1 - radius);
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= radius * radius
}
