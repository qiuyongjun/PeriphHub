use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Margin, Painter, Rect, RichText,
    Sense, Stroke, StrokeKind, Theme, ThemePreference, Ui, Vec2,
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
    pub card_top: Color32,
    pub card_hover: Color32,
    pub inset: Color32,
    pub track: Color32,
    pub text: Color32,
    pub dim: Color32,
    pub subtle: Color32,
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

/// 冷调中性色让深色界面更有层次，浅色辅助文字保持足够对比度；冰蓝只用于可操作处。
pub const DARK: Palette = Palette {
    dark: true,
    bg: Color32::from_rgb(12, 14, 18),
    card: Color32::from_rgb(20, 23, 29),
    card_top: Color32::from_rgb(20, 23, 29),
    card_hover: Color32::from_rgb(26, 30, 38),
    inset: Color32::from_rgb(16, 18, 23),
    track: Color32::from_rgb(48, 53, 63),
    text: Color32::from_rgb(241, 244, 248),
    dim: Color32::from_rgb(158, 166, 179),
    subtle: Color32::from_rgb(137, 146, 160),
    accent: Color32::from_rgb(129, 167, 255),
    accent_hover: Color32::from_rgb(174, 199, 255),
    on_accent: Color32::from_rgb(11, 18, 32),
    border: Color32::from_rgb(45, 50, 60),
    chip_bg: Color32::from_rgb(27, 31, 39),
    titlebar: Color32::from_rgb(15, 17, 22),
    ok: Color32::from_rgb(119, 190, 155),
    warn: Color32::from_rgb(218, 181, 108),
    danger: Color32::from_rgb(220, 137, 145),
    info: Color32::from_rgb(154, 173, 214),
    close_hover: Color32::from_rgb(180, 70, 80),
};

pub const LIGHT: Palette = Palette {
    dark: false,
    bg: Color32::from_rgb(246, 248, 251),
    card: Color32::from_rgb(255, 255, 255),
    card_top: Color32::from_rgb(255, 255, 255),
    card_hover: Color32::from_rgb(248, 250, 253),
    inset: Color32::from_rgb(242, 245, 249),
    track: Color32::from_rgb(218, 224, 232),
    text: Color32::from_rgb(25, 32, 44),
    dim: Color32::from_rgb(93, 104, 120),
    subtle: Color32::from_rgb(95, 105, 120),
    accent: Color32::from_rgb(55, 102, 235),
    accent_hover: Color32::from_rgb(39, 82, 201),
    on_accent: Color32::from_rgb(255, 255, 255),
    border: Color32::from_rgb(226, 231, 238),
    chip_bg: Color32::from_rgb(242, 245, 249),
    titlebar: Color32::from_rgb(250, 252, 254),
    ok: Color32::from_rgb(31, 118, 82),
    warn: Color32::from_rgb(134, 95, 23),
    danger: Color32::from_rgb(176, 65, 77),
    info: Color32::from_rgb(85, 105, 153),
    close_hover: Color32::from_rgb(214, 75, 87),
};

/// Same colour with a different alpha, for washes and tinted surfaces.
pub fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// 返回当前主题的语义色板。
pub fn palette(mode: ThemeMode) -> &'static Palette {
    match mode {
        ThemeMode::Dark => &DARK,
        ThemeMode::Light => &LIGHT,
    }
}

/// 界面字号阶梯。只取整数：egui 按亚像素排版，11.5 / 13.5 这类半像素值会让
/// 汉字笔画落在半格上，边缘发灰、同一行里粗细不匀。七档够用，且每一档都有
/// 明确职责——新增字号前先问它属于哪一档，别再插 12.5。
pub mod text {
    /// 页面主标题
    pub const TITLE: f32 = 24.0;
    /// 页内标题、统计数字
    pub const HEADING: f32 = 20.0;
    /// 设备名、卡片主标题
    pub const SUBTITLE: f32 = 16.0;
    /// 卡片内的指标数值（DPI / 回报率 / 电量）：比标题小一档，比正文大，
    /// 固定值——不按列宽自适应，否则并排三个数字会是三种字号。
    pub const METRIC: f32 = 18.0;
    /// 正文、控件标签
    pub const BODY: f32 = 13.0;
    /// 次要说明、辅助句子
    pub const META: f32 = 12.0;
    /// 单位、微标签、全大写英文
    pub const MICRO: f32 = 11.0;
}

/// 控件尺寸与圆角。按钮、标签、步进器、开关共用同一套刻度：之前控件圆角在
/// 6/7/8/9 之间各写各的，同一行里"加号按钮"和旁边的档位标签圆角不一样，
/// 放大看就是一排毛边。
pub mod ctl {
    /// 标准控件圆角（标签、按钮、图标按钮）
    pub const RADIUS: u8 = 8;
    /// 小控件圆角（步进按钮、行内小标签）
    pub const RADIUS_SM: u8 = 7;
    /// 标准控件高度
    pub const H: f32 = 32.0;
    /// 次要小控件高度
    pub const H_SM: f32 = 26.0;
    /// 主按钮高度
    pub const H_BTN: f32 = 36.0;
}

/// 系统字体分离正文与展示字重；缺少某个字面时沿用原有字体回退链。
///
/// 中文正文取 `msyh.ttc` 的**第二个字面**（Microsoft YaHei UI）：它和雅黑
/// 文档版同源，但字面更窄、字距更紧，是 Windows 为界面文本单独调过的一版。
/// 文档版在 12–13px 上偏松散，同一行里中英文的节奏对不上，这是"看着不精致"
/// 最直接的来源。TTC 的字面序号由 `FontData::from_blob` 的第二个参数指定。
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let dir = std::env::var_os("WINDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("C:/Windows"))
        .join("Fonts");

    // (字节, TTC 内字面序号)：雅黑取界面版，黑体只有一个字面。
    let cjk_face = std::fs::read(dir.join("msyh.ttc"))
        .map(|b| (b, 1u32))
        .or_else(|_| std::fs::read(dir.join("simhei.ttf")).map(|b| (b, 0u32)));
    if let Ok((bytes, index)) = cjk_face {
        fonts.font_data.insert(
            "cjk".to_owned(),
            egui::FontData::from_blob(std::sync::Arc::new(bytes), index).into(),
        );
        if let Some(f) = fonts.families.get_mut(&FontFamily::Proportional) {
            f.insert(0, "cjk".to_owned());
        }
        if let Some(f) = fonts.families.get_mut(&FontFamily::Monospace) {
            f.push("cjk".to_owned());
        }
    }
    if let Ok(bytes) = std::fs::read(dir.join("segoeui.ttf")) {
        fonts
            .font_data
            .insert("latin".to_owned(), egui::FontData::from_owned(bytes).into());
        if let Some(f) = fonts.families.get_mut(&FontFamily::Proportional) {
            f.insert(0, "latin".to_owned());
        }
    }
    // 展示字重：拉丁用 Segoe UI Semibold，中文用等线加粗——等线 Bold 比
    // 雅黑 Bold 轻一档，和西文半粗的视觉重量更接近，标题不会显得发黑。
    let mut display = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    for (name, key) in [
        ("dengb.ttf", "display-cjk"),
        ("seguisb.ttf", "display-latin"),
    ] {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            fonts
                .font_data
                .insert(key.to_owned(), egui::FontData::from_owned(bytes).into());
            display.insert(0, key.to_owned());
        }
    }
    fonts
        .families
        .insert(FontFamily::Name("display".into()), display);
    ctx.set_fonts(fonts);
}

/// 展示字体使用实际半粗字面，egui 的 strong 仅改变颜色。
pub fn heading_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("display".into()))
}

/// 标题和关键数字共用字重规则。
pub fn heading(text: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(text).font(heading_font(size)).color(color)
}

/// 正文：控件标签、可读句子。语义入口，避免调用方散落裸字号。
pub fn body(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text).size(text::BODY).color(color)
}

/// 次要说明：跟随在标题下方或卡片脚注里的解释性文字。
pub fn meta(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text).size(text::META).color(color)
}

/// 微标签：单位、档位序号、全大写英文。只用于短标记，不承载句子。
pub fn micro(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text).size(text::MICRO).color(color)
}

/// 同步全局控件、排版与主题，避免原生控件与自绘表面割裂。
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
        s.spacing.item_spacing = Vec2::new(10.0, 8.0);
        s.spacing.button_padding = Vec2::new(12.0, 7.0);
        s.spacing.interact_size.y = 32.0;
        s.spacing.menu_margin = Margin::same(8);
        s.spacing.scroll = egui::style::ScrollStyle::thin();
        s.animation_time = 0.14;
        s.text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(text::BODY));
        s.text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(text::BODY));
        s.text_styles
            .insert(egui::TextStyle::Small, FontId::proportional(text::MICRO));
        s.text_styles
            .insert(egui::TextStyle::Heading, heading_font(text::TITLE));
    });
}

/// 卡片以细描边为主、柔和投影为辅，悬浮层级清晰但不显得厚重。
pub fn card_shadow(p: &Palette) -> egui::Shadow {
    egui::Shadow {
        offset: [0, 3],
        blur: 10,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 30 } else { 12 }),
    }
}

/// 统一输入、选中、悬停和弹层的视觉反馈。
fn visuals(p: &Palette) -> egui::Visuals {
    let mut v = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.panel_fill = p.bg;
    v.window_fill = p.bg;
    v.faint_bg_color = p.chip_bg;
    v.extreme_bg_color = p.inset;
    v.override_text_color = Some(p.text);
    v.selection.bg_fill = alpha(p.accent, if p.dark { 64 } else { 40 });
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.widgets.inactive.bg_fill = p.chip_bg;
    v.widgets.inactive.weak_bg_fill = p.chip_bg;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.corner_radius = CornerRadius::same(8);
    v.widgets.hovered.bg_fill = p.card_hover;
    v.widgets.hovered.weak_bg_fill = p.card_hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, alpha(p.accent, 130));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.hovered.corner_radius = CornerRadius::same(8);
    v.widgets.active.bg_fill = p.track;
    v.widgets.active.weak_bg_fill = p.track;
    v.widgets.active.bg_stroke = Stroke::new(1.0, alpha(p.accent, 190));
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

/// 保持电量阈值一致，并区分有线供电与未知读数。
pub fn battery_level(battery: Option<u8>, wired: bool) -> BatteryLevel {
    match battery {
        Some(b) if b >= 60 => BatteryLevel::Good,
        Some(b) if b >= 20 => BatteryLevel::Low,
        Some(_) => BatteryLevel::Critical,
        None if wired => BatteryLevel::Wired,
        None => BatteryLevel::Unknown,
    }
}

/// 按电量状态选择语义色；"有线"不是读数，用次级文本色，避免像链接。
pub fn battery_color(level: BatteryLevel, p: &Palette) -> Color32 {
    match level {
        BatteryLevel::Good => p.ok,
        BatteryLevel::Low => p.warn,
        BatteryLevel::Critical => p.danger,
        BatteryLevel::Wired => p.dim,
        BatteryLevel::Unknown => p.dim,
    }
}

/// 电量文本。驱动已经挡掉了 >100 的无效读数，这里再兜一次：条形图按 100%
/// 封顶，文字要是照原样打出来就会看到"127%"配一条满格，两边对不上。
pub fn battery_text(battery: Option<u8>, wired: bool) -> String {
    match battery {
        Some(b) => format!("{}%", b.min(100)),
        None if wired => "有线".to_string(),
        None => "--".to_string(),
    }
}

/// Brand mark drawn with egui primitives: ink tile with a "P" monogram.
pub fn brand_badge(ui: &mut Ui, size: f32, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint_brand(ui.painter(), rect, p);
}

/// 主窗口、浮窗和空态共用标识比例与细微高光。
pub fn paint_brand(painter: &Painter, rect: Rect, p: &Palette) {
    let size = rect.width();
    let radius = CornerRadius::same((size * 0.28) as u8);
    painter.rect_filled(rect, radius, p.accent);
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, alpha(Color32::WHITE, 44)),
        StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        "P",
        heading_font(size * 0.62),
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
