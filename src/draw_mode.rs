/// Annotation Mode — a dedicated mode for drawing annotations on the
/// magnified view.
///
/// Activated by selecting a tool from the pie menu (Space). In this mode
/// mouse movement does not pan the view — only MMB drag pans, so the
/// user can draw without the screen shifting. LMB draws with the current
/// tool; RMB exits annotation mode.

use crate::osd::{self, OsdSprite};
use crate::render::RgbaBuffer;

// ═══════════════════════════════════════════════════════════════════
// Drawing tools
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawTool {
    Select,
    Freehand,
    Erase,
    Line,
    Box,
    Arrow,
    Circle,
    Number,
}

impl DrawTool {
    pub fn label(self) -> &'static str {
        match self {
            DrawTool::Select => "Sel",
            DrawTool::Freehand => "Pen",
            DrawTool::Erase => "Ers",
            DrawTool::Line => "Line",
            DrawTool::Box => "Box",
            DrawTool::Arrow => "Arr",
            DrawTool::Circle => "Cir",
            DrawTool::Number => "#",
        }
    }
    pub fn label_long(self) -> &'static str {
        match self {
            DrawTool::Select => "Select",
            DrawTool::Freehand => "Draw",
            DrawTool::Erase => "Erase",
            DrawTool::Line => "Line",
            DrawTool::Box => "Rectangle",
            DrawTool::Arrow => "Arrow",
            DrawTool::Circle => "Circle",
            DrawTool::Number => "Number",
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Selection style
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionStyle {
    Nearest,
    Overlap,
}

// ═══════════════════════════════════════════════════════════════════
// Annotations
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub enum Annotation {
    Freehand { points: Vec<(f64, f64)>, color: [u8; 3], width: f32 },
    Line { start: (f64, f64), end: (f64, f64), color: [u8; 3], width: f32 },
    Box { top_left: (f64, f64), bottom_right: (f64, f64), color: [u8; 3], width: f32 },
    Arrow { start: (f64, f64), end: (f64, f64), color: [u8; 3], width: f32 },
    Circle { center: (f64, f64), radius: f64, color: [u8; 3], width: f32 },
    Number { pos: (f64, f64), value: i32, color: [u8; 3] },
}

impl Annotation {
    pub fn color(&self) -> [u8; 3] {
        match self {
            Self::Freehand { color, .. } | Self::Line { color, .. }
            | Self::Box { color, .. } | Self::Arrow { color, .. }
            | Self::Circle { color, .. } | Self::Number { color, .. } => *color,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DrawingInProgress {
    pub tool: DrawTool,
    pub color: [u8; 3],
    pub start: (f64, f64),
    pub current: (f64, f64),
    pub points: Vec<(f64, f64)>,
}

impl DrawingInProgress {
    pub fn new(tool: DrawTool, color: [u8; 3], start: (f64, f64)) -> Self {
        Self { tool, color, start, current: start, points: vec![start] }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Palette
// ═══════════════════════════════════════════════════════════════════

pub const PALETTE: &[[u8; 3]] = &[
    [255, 0, 0], [255, 140, 0], [255, 255, 0], [0, 200, 0],
    [0, 150, 255], [140, 0, 255], [255, 255, 255], [0, 0, 0],
];

// ═══════════════════════════════════════════════════════════════════
// Rendering primitives (all use STRAIGHT alpha)
// ═══════════════════════════════════════════════════════════════════

fn capture_to_screen(
    cap: (f64, f64), view_center: (f64, f64), zoom: f64, vp_w: i32, vp_h: i32,
) -> (f64, f64) {
    ((cap.0 - view_center.0) * zoom + vp_w as f64 / 2.0,
     (cap.1 - view_center.1) * zoom + vp_h as f64 / 2.0)
}

/// Blend a straight-alpha source pixel onto a straight-alpha destination.
fn blend_pixel(dst: &mut [u8], src_color: [u8; 3], src_alpha: u8) {
    if src_alpha == 0 { return; }
    let a = src_alpha as u16;
    let inv = 255 - a;
    dst[0] = ((src_color[0] as u16 * a + dst[0] as u16 * inv) / 255) as u8;
    dst[1] = ((src_color[1] as u16 * a + dst[1] as u16 * inv) / 255) as u8;
    dst[2] = ((src_color[2] as u16 * a + dst[2] as u16 * inv) / 255) as u8;
    dst[3] = ((255u16 * a + dst[3] as u16 * inv) / 255) as u8;
}

fn draw_thick_line(buf: &mut [u8], w: i32, h: i32, p0: (f64, f64), p1: (f64, f64), thickness: f64, color: [u8; 3]) {
    let dx = p1.0 - p0.0;
    let dy = p1.1 - p0.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 { return; }
    let steps = (len * 2.0).ceil() as i32;
    let half_t = thickness / 2.0;
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let x = p0.0 + dx * t;
        let y = p0.1 + dy * t;
        let x0 = (x - half_t).floor() as i32;
        let x1 = (x + half_t).ceil() as i32;
        let y0 = (y - half_t).floor() as i32;
        let y1 = (y + half_t).ceil() as i32;
        for py in y0.max(0)..y1.min(h) {
            for px in x0.max(0)..x1.min(w) {
                let dist = ((px as f64 - x).abs()).max((py as f64 - y).abs());
                if dist <= half_t + 0.5 {
                    let alpha = ((1.0 - (dist / (half_t + 0.5)).min(1.0)) * 255.0) as u8;
                    blend_pixel(&mut buf[(py as usize * w as usize + px as usize) * 4..], color, alpha);
                }
            }
        }
    }
}

fn draw_thick_circle(buf: &mut [u8], w: i32, h: i32, center: (f64, f64), radius: f64, thickness: f64, color: [u8; 3]) {
    if radius < 0.5 { return; }
    let half_t = thickness / 2.0;
    let steps = (std::f64::consts::TAU * radius * 2.0).ceil() as i32;
    for i in 0..steps {
        let angle = std::f64::consts::TAU * i as f64 / steps as f64;
        let x = center.0 + radius * angle.cos();
        let y = center.1 + radius * angle.sin();
        for py in (y - half_t - 1.0).floor() as i32..=(y + half_t + 1.0).ceil() as i32 {
            for px in (x - half_t - 1.0).floor() as i32..=(x + half_t + 1.0).ceil() as i32 {
                if px < 0 || py < 0 || px >= w || py >= h { continue; }
                let dist = ((px as f64 - x).powi(2) + (py as f64 - y).powi(2)).sqrt();
                if dist <= half_t + 1.0 {
                    let alpha = ((1.0 - (dist / (half_t + 1.0)).min(1.0)) * 255.0) as u8;
                    blend_pixel(&mut buf[(py as usize * w as usize + px as usize) * 4..], color, alpha);
                }
            }
        }
    }
}

fn draw_text_scaled(buf: &mut [u8], bw: i32, bh: i32, x: i32, y: i32, text: &str, color: [u8; 3], s: i32) {
    let s = s.max(1);
    for (ci, ch) in text.chars().enumerate() {
        let Some(rows) = osd::glyph(ch) else { continue; };
        let gx = x + ci as i32 * 6 * s;
        for (gy, row) in rows.iter().enumerate() {
            for bit in 0..5 {
                if row & (1 << (4 - bit)) != 0 {
                    for dy in 0..s {
                        for dx in 0..s {
                            let px = gx + bit * s + dx;
                            let py = y + gy as i32 * s + dy;
                            if px >= 0 && px < bw && py >= 0 && py < bh {
                                let idx = (py as usize * bw as usize + px as usize) * 4;
                                buf[idx..idx+4].copy_from_slice(&[color[0], color[1], color[2], 255]);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Render a single annotation into a buffer.
fn render_one_annotation(
    buf: &mut RgbaBuffer, vp_w: i32, vp_h: i32,
    view_center: (f64, f64), zoom: f64, ann: &Annotation,
) {
    let c = ann.color();
    match ann {
        Annotation::Freehand { points, width, .. } => {
            for w in points.windows(2) {
                let p0 = capture_to_screen(w[0], view_center, zoom, vp_w, vp_h);
                let p1 = capture_to_screen(w[1], view_center, zoom, vp_w, vp_h);
                draw_thick_line(&mut buf.data, vp_w, vp_h, p0, p1, *width as f64, c);
            }
        }
        Annotation::Line { start, end, width, .. } => {
            let p0 = capture_to_screen(*start, view_center, zoom, vp_w, vp_h);
            let p1 = capture_to_screen(*end, view_center, zoom, vp_w, vp_h);
            draw_thick_line(&mut buf.data, vp_w, vp_h, p0, p1, *width as f64, c);
        }
        Annotation::Box { top_left, bottom_right, width, .. } => {
            let tl = capture_to_screen(*top_left, view_center, zoom, vp_w, vp_h);
            let br = capture_to_screen(*bottom_right, view_center, zoom, vp_w, vp_h);
            let (x0,y0) = (tl.0.round() as i32, tl.1.round() as i32);
            let (x1,y1) = (br.0.round() as i32, br.1.round() as i32);
            draw_thick_line(&mut buf.data, vp_w, vp_h, (x0 as f64, y0 as f64), (x1 as f64, y0 as f64), *width as f64, c);
            draw_thick_line(&mut buf.data, vp_w, vp_h, (x1 as f64, y0 as f64), (x1 as f64, y1 as f64), *width as f64, c);
            draw_thick_line(&mut buf.data, vp_w, vp_h, (x1 as f64, y1 as f64), (x0 as f64, y1 as f64), *width as f64, c);
            draw_thick_line(&mut buf.data, vp_w, vp_h, (x0 as f64, y1 as f64), (x0 as f64, y0 as f64), *width as f64, c);
        }
        Annotation::Arrow { start, end, width, .. } => {
            let p0 = capture_to_screen(*start, view_center, zoom, vp_w, vp_h);
            let p1 = capture_to_screen(*end, view_center, zoom, vp_w, vp_h);
            draw_thick_line(&mut buf.data, vp_w, vp_h, p0, p1, *width as f64, c);
            let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
            let len = (dx*dx + dy*dy).sqrt();
            if len > 1.0 {
                let (ux, uy) = (dx/len, dy/len);
                let (hl, hw) = (12.0, 6.0);
                draw_thick_line(&mut buf.data, vp_w, vp_h, p1, (p1.0-ux*hl+uy*hw, p1.1-uy*hl-ux*hw), *width as f64, c);
                draw_thick_line(&mut buf.data, vp_w, vp_h, p1, (p1.0-ux*hl-uy*hw, p1.1-uy*hl+ux*hw), *width as f64, c);
            }
        }
        Annotation::Circle { center, radius, width, .. } => {
            let sc = capture_to_screen(*center, view_center, zoom, vp_w, vp_h);
            draw_thick_circle(&mut buf.data, vp_w, vp_h, sc, radius * zoom, *width as f64, c);
        }
        Annotation::Number { pos, value, .. } => {
            let sp = capture_to_screen(*pos, view_center, zoom, vp_w, vp_h);
            draw_text_scaled(&mut buf.data, vp_w, vp_h, sp.0.round() as i32, sp.1.round() as i32, &format!("{value}"), c, 1);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Annotation UI — Pie Menu
//
// Icons are rendered via resvg. The critical detail: resvg/tiny_skia
// returns PREMULTIPLIED alpha from Pixmap::take(). Our blend_pixel()
// and the GPU both expect STRAIGHT alpha. So we un-premultiply every
// icon buffer immediately after rendering.
// ═══════════════════════════════════════════════════════════════════

/// An action triggered by clicking a pie menu button.
#[derive(Debug, Clone, Copy)]
pub enum PieAction {
    Tool(DrawTool),
    Color([u8; 3]),
}

struct PieButton {
    cx: f64, cy: f64,
    hw: f64, hh: f64,
    cr: f64,
    bg: [u8; 3],
    label: Option<&'static str>,
    icon: Option<&'static str>,
    action: PieAction,
}

struct PieLayer {
    name: &'static str,
    buttons: Vec<PieButton>,
}

// ── Layout constants ──

// Color ring: a thick annulus (donut) with wedge-shaped color segments.
const COLOR_INNER_R: f64 = 70.0;
const COLOR_OUTER_R: f64 = 130.0;

// Tool ring: individual rounded-rect buttons on a circle.
const TOOL_R: f64 = 220.0; // distance from center to tool button center
const TOOL_SIZE: f64 = 100.0;
const TOOL_HW: f64 = TOOL_SIZE / 2.0;
const TOOL_HH: f64 = TOOL_SIZE / 2.0;
const TOOL_CR: f64 = 16.0;
const TOOL_GAP: f64 = 24.0;

const SPIEL_MARGIN: f64 = 60.0;
// Total sprite radius = outermost element edge + margin.
const TOTAL_R: f64 = TOOL_R + TOOL_HW + SPIEL_MARGIN; // 330

const COLOR_BUTTONS: &[([u8; 3], &str)] = &[
    ([255, 0, 0], "Red"), ([255, 140, 0], "Org"), ([255, 255, 0], "Yel"),
    ([0, 200, 0], "Grn"), ([0, 150, 255], "Blu"), ([140, 0, 255], "Pur"),
    ([255, 255, 255], "Wht"), ([0, 0, 0], "Blk"),
];

const TOOL_BUTTONS: &[(DrawTool, &str)] = &[
    (DrawTool::Select, "mouse-pointer"),
    (DrawTool::Freehand, "pencil"),
    (DrawTool::Erase, "eraser"),
    (DrawTool::Line, "minus"),
    (DrawTool::Box, "square"),
    (DrawTool::Arrow, "arrow-up-right"),
    (DrawTool::Circle, "circle"),
    (DrawTool::Number, "hash"),
];

const BTN_BG: [u8; 3] = [40, 40, 45];

// ── SVG icon loading ──

struct IconCache {
    icons: std::collections::HashMap<&'static str, RgbaBuffer>,
}

impl IconCache {
    fn new() -> Self {
        let mut icons = std::collections::HashMap::new();
        let white = [255u8, 255, 255];
        let entries: &[(&str, &[u8])] = &[
            ("mouse-pointer", include_bytes!("../assets/ui/button/mouse-pointer.svg")),
            ("pencil", include_bytes!("../assets/ui/button/pencil.svg")),
            ("eraser", include_bytes!("../assets/ui/button/eraser.svg")),
            ("minus", include_bytes!("../assets/ui/button/minus.svg")),
            ("square", include_bytes!("../assets/ui/button/square.svg")),
            ("arrow-up-right", include_bytes!("../assets/ui/button/arrow-up-right.svg")),
            ("circle", include_bytes!("../assets/ui/button/circle.svg")),
            ("hash", include_bytes!("../assets/ui/button/hash.svg")),
        ];
        for &(name, bytes) in entries {
            if let Some(buf) = load_svg_icon(bytes, 4.0, white) {
                icons.insert(name, buf);
            }
        }
        Self { icons }
    }
}

/// Render an SVG icon to an RGBA buffer at the given scale,
/// with all non-transparent pixels forced to the specified color.
///
/// Lucide SVGs use `stroke="currentColor"` which defaults to black.
/// We render as-is, then replace every opaque pixel's RGB with the
/// desired color while preserving alpha (handles premultiplied→straight
/// conversion from tiny_skia).
fn load_svg_icon(svg_bytes: &[u8], scale: f32, color: [u8; 3]) -> Option<RgbaBuffer> {
    let opts = resvg::usvg::Options::default();
    let rtree = resvg::usvg::Tree::from_data(svg_bytes, &opts).ok()?;
    let size = rtree.size();
    let w = (size.width() * scale) as u32;
    let h = (size.height() * scale) as u32;
    if w == 0 || h == 0 { return None; }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h)?;
    resvg::render(
        &rtree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // Pixmap::take() returns premultiplied alpha. Convert to straight alpha.
    let mut data = pixmap.take();
    for pixel in data.chunks_exact_mut(4) {
        let a = pixel[3] as u16;
        if a > 0 && a < 255 {
            pixel[0] = (pixel[0] as u16 * 255 / a) as u8;
            pixel[1] = (pixel[1] as u16 * 255 / a) as u8;
            pixel[2] = (pixel[2] as u16 * 255 / a) as u8;
        }
        // Replace RGB with desired color, keep alpha
        if a > 0 {
            pixel[0] = color[0];
            pixel[1] = color[1];
            pixel[2] = color[2];
        }
    }
    Some(RgbaBuffer { width: w as i32, height: h as i32, data })
}

// ── Layout engine ──

fn compute_layout(current_tool: DrawTool, _current_color: [u8; 3]) -> (Vec<PieLayer>, f64) {
    let mut layers = Vec::new();

    // Color layer: logical buttons for indexing (drawn as annulus, not individually).
    let mut color_buttons = Vec::new();
    for &(color, _) in COLOR_BUTTONS.iter() {
        color_buttons.push(PieButton {
            cx: 0.0, cy: 0.0, hw: 0.0, hh: 0.0, cr: 0.0,
            bg: color, label: None, icon: None,
            action: PieAction::Color(color),
        });
    }
    layers.push(PieLayer { name: "colors", buttons: color_buttons });

    // Tool layer: buttons placed on outer ring.
    let mut tool_buttons = Vec::new();
    for &(tool, icon_name) in TOOL_BUTTONS.iter() {
        let active = tool == current_tool;
        let bg = if active { [60, 80, 130] } else { BTN_BG };
        tool_buttons.push(PieButton {
            cx: 0.0, cy: 0.0, hw: TOOL_HW, hh: TOOL_HH, cr: TOOL_CR,
            bg, label: Some(tool.label()), icon: Some(icon_name),
            action: PieAction::Tool(tool),
        });
    }
    let center = TOTAL_R;
    let n = TOOL_BUTTONS.len() as f64;
    for (i, btn) in tool_buttons.iter_mut().enumerate() {
        let a = std::f64::consts::TAU * i as f64 / n - std::f64::consts::FRAC_PI_2;
        btn.cx = center + TOOL_R * a.cos();
        btn.cy = center + TOOL_R * a.sin();
    }
    layers.push(PieLayer { name: "tools", buttons: tool_buttons });

    (layers, TOTAL_R)
}

// ── Rounded-rect drawing ──

fn point_in_rounded_rect(lx: i32, ly: i32, rw: i32, rh: i32, cr: i32) -> bool {
    if lx < 0 || ly < 0 || lx >= rw || ly >= rh { return false; }
    let corners = [(cr, cr), (rw-1-cr, cr), (cr, rh-1-cr), (rw-1-cr, rh-1-cr)];
    for &(cx, cy) in &corners {
        if (lx - cx).pow(2) + (ly - cy).pow(2) <= cr * cr { return true; }
    }
    lx >= cr && lx <= rw-1-cr || ly >= cr && ly <= rh-1-cr
}

fn fill_rounded_rect(buf: &mut [u8], bw: i32, bh: i32, x0: i32, y0: i32, rw: i32, rh: i32, cr: i32, color: [u8; 4]) {
    for y in y0.max(0)..(y0+rh).min(bh) {
        for x in x0.max(0)..(x0+rw).min(bw) {
            if point_in_rounded_rect(x - x0, y - y0, rw, rh, cr) {
                let idx = (y as usize * bw as usize + x as usize) * 4;
                blend_pixel(&mut buf[idx..idx+4], [color[0], color[1], color[2]], color[3]);
            }
        }
    }
}

fn stroke_rounded_rect(buf: &mut [u8], bw: i32, bh: i32, x0: i32, y0: i32, rw: i32, rh: i32, cr: i32, thickness: i32, color: [u8; 4]) {
    let half_t = thickness as f64 / 2.0;
    let hw = rw as f64 / 2.0;
    let hh = rh as f64 / 2.0;
    let cr_f = cr as f64;
    let center_x = x0 as f64 + hw;
    let center_y = y0 as f64 + hh;
    let y_start = (y0 - thickness).max(0);
    let y_end = (y0 + rh + thickness).min(bh);
    let x_start = (x0 - thickness).max(0);
    let x_end = (x0 + rw + thickness).min(bw);
    for y in y_start..y_end {
        for x in x_start..x_end {
            // Signed distance to rounded rect boundary.
            // Negative inside, positive outside, 0 on boundary.
            let px = (x as f64 + 0.5) - center_x;
            let py = (y as f64 + 0.5) - center_y;
            let dx = px.abs() - (hw - cr_f);
            let dy = py.abs() - (hh - cr_f);
            let outside = dx.max(0.0).hypot(dy.max(0.0)) - cr_f;
            let inside = dx.min(dy).min(0.0);
            let sdf = outside + inside;
            let abs_d = sdf.abs();
            if abs_d <= half_t + 0.5 {
                let a = ((1.0 - (abs_d / (half_t + 0.5)).min(1.0)) * color[3] as f64) as u8;
                blend_pixel(&mut buf[(y as usize * bw as usize + x as usize) * 4..], [color[0], color[1], color[2]], a);
            }
        }
    }
}

// ── Drawing helpers ──

fn brighten(c: [u8; 3], amount: u8) -> [u8; 3] {
    [c[0].saturating_add(amount), c[1].saturating_add(amount), c[2].saturating_add(amount)]
}

/// Determine which color sector a pixel belongs to, or None if outside the ring.
fn annulus_sector(px: i32, py: i32, center: f64) -> Option<usize> {
    let dx = px as f64 + 0.5 - center;
    let dy = py as f64 + 0.5 - center;
    let r = dx.hypot(dy);
    if r < COLOR_INNER_R || r > COLOR_OUTER_R {
        return None;
    }
    let mut angle = dy.atan2(dx);
    if angle < 0.0 { angle += std::f64::consts::TAU; }
    let n = COLOR_BUTTONS.len() as f64;
    let sector = ((angle / std::f64::consts::TAU) * n) as usize;
    Some(sector.min(COLOR_BUTTONS.len() - 1))
}

/// Draw the color ring: a thick annulus with wedge-shaped color segments.
/// Hovered segment gets brightened + white border outline.
fn draw_color_annulus(buf: &mut [u8], bw: i32, bh: i32, center: f64, hovered_color: Option<[u8; 3]>) {
    let hovered_sector = hovered_color.and_then(|hc| {
        COLOR_BUTTONS.iter().position(|(c, _)| *c == hc)
    });
    let n = COLOR_BUTTONS.len() as f64;
    let y_min = (center - COLOR_OUTER_R - 1.0).floor() as i32;
    let y_max = (center + COLOR_OUTER_R + 1.0).ceil() as i32;
    let x_min = (center - COLOR_OUTER_R - 1.0).floor() as i32;
    let x_max = (center + COLOR_OUTER_R + 1.0).ceil() as i32;

    // Pass 1: fill colors.
    for py in y_min.max(0)..y_max.min(bh) {
        for px in x_min.max(0)..x_max.min(bw) {
            if let Some(sector) = annulus_sector(px, py, center) {
                let mut color = COLOR_BUTTONS[sector].0;
                if hovered_sector == Some(sector) {
                    color = brighten(color, 70);
                }
                let idx = (py as usize * bw as usize + px as usize) * 4;
                buf[idx] = color[0];
                buf[idx+1] = color[1];
                buf[idx+2] = color[2];
                buf[idx+3] = 255;
            }
        }
    }

    // Pass 2: white border on hovered segment edges.
    if let Some(hs) = hovered_sector {
        for py in y_min.max(0)..y_max.min(bh) {
            for px in x_min.max(0)..x_max.min(bw) {
                let here = annulus_sector(px, py, center);
                if here != Some(hs) { continue; }
                // Check 4 neighbors — if any is different sector or outside ring, draw white.
                let is_edge = [(0,1),(0,-1),(1,0),(-1,0)].iter().any(|&(ox, oy)| {
                    annulus_sector(px + ox, py + oy, center) != Some(hs)
                });
                if is_edge {
                    let idx = (py as usize * bw as usize + px as usize) * 4;
                    buf[idx] = 255;
                    buf[idx+1] = 255;
                    buf[idx+2] = 255;
                    buf[idx+3] = 255;
                }
            }
        }
    }
}

/// Draw a single tool button (rounded-rect with icon).
fn draw_tool_button(buf: &mut [u8], bw: i32, bh: i32, btn: &PieButton, is_hovered: bool, icons: &IconCache) {
    let x0 = (btn.cx - btn.hw).round() as i32;
    let y0 = (btn.cy - btn.hh).round() as i32;
    let rw = (btn.hw * 2.0).round() as i32;
    let rh = (btn.hh * 2.0).round() as i32;
    let cr = btn.cr.round() as i32;

    // Background — much brighter when hovered.
    let bg = if is_hovered { brighten(btn.bg, 80) } else { btn.bg };
    let bg_a = if is_hovered { 255 } else { 220 };
    fill_rounded_rect(buf, bw, bh, x0, y0, rw, rh, cr, [bg[0], bg[1], bg[2], bg_a]);

    // White border when hovered.
    if is_hovered {
        stroke_rounded_rect(buf, bw, bh, x0, y0, rw, rh, cr, 3, [255, 255, 255, 255]);
    }

    // Icon centered inside.
    if let Some(icon_name) = btn.icon {
        if let Some(icon_buf) = icons.icons.get(icon_name) {
            let pad = 12;
            let avail = rw.min(rh) - pad * 2;
            let longest = icon_buf.width.max(icon_buf.height);
            let scale_f = if longest > avail { avail as f64 / longest as f64 } else { 1.0 };
            let draw_w = (icon_buf.width as f64 * scale_f) as i32;
            let draw_h = (icon_buf.height as f64 * scale_f) as i32;
            let ix = x0 + (rw - draw_w) / 2;
            let iy = y0 + (rh - draw_h) / 2;
            for dy in 0..draw_h {
                let sy = (dy as f64 / scale_f) as i32;
                if sy < 0 || sy >= icon_buf.height { continue; }
                for dx in 0..draw_w {
                    let sx = (dx as f64 / scale_f) as i32;
                    if sx < 0 || sx >= icon_buf.width { continue; }
                    let px = ix + dx;
                    let py = iy + dy;
                    if px >= 0 && px < bw && py >= 0 && py < bh {
                        let si = (sy as usize * icon_buf.width as usize + sx as usize) * 4;
                        let di = (py as usize * bw as usize + px as usize) * 4;
                        let sa = icon_buf.data[si + 3];
                        if sa > 0 {
                            blend_pixel(&mut buf[di..di+4], [icon_buf.data[si], icon_buf.data[si+1], icon_buf.data[si+2]], sa);
                        }
                    }
                }
            }
        }
    }
}

// ── Hit testing ──

/// Convert cursor offset (from sprite center) to polar coordinates.
fn cursor_polar(dx: f64, dy: f64) -> (f64, f64) {
    let r = dx.hypot(dy);
    let mut angle = dy.atan2(dx);
    if angle < 0.0 { angle += std::f64::consts::TAU; }
    (r, angle)
}

/// Minimum cursor distance (buffer px) before any button is selected.
const PIE_DEAD_ZONE: f64 = 30.0;

/// Direction vectors for each color button (matches annulus drawing order).
fn color_dirs() -> [(f64, f64); 8] {
    let mut dirs = [(0.0f64, 0.0f64); 8];
    for i in 0..8 {
        let a = std::f64::consts::TAU * i as f64 / 8.0;
        dirs[i] = (a.cos(), a.sin());
    }
    dirs
}

/// Direction vectors for each tool button (starts at -PI/2, 12 o'clock).
fn tool_dirs() -> [(f64, f64); 8] {
    let mut dirs = [(0.0f64, 0.0f64); 8];
    for i in 0..8 {
        let a = std::f64::consts::TAU * i as f64 / 8.0 - std::f64::consts::FRAC_PI_2;
        dirs[i] = (a.cos(), a.sin());
    }
    dirs
}

/// Find closest button by dot product (Blender-style direction comparison).
fn closest_by_direction(cursor_dir: (f64, f64), dirs: &[(f64, f64)]) -> usize {
    let (cx, cy) = cursor_dir;
    let mut best = 0;
    let mut best_dot = f64::NEG_INFINITY;
    for (i, &(bx, by)) in dirs.iter().enumerate() {
        let dot = cx * bx + cy * by;
        if dot > best_dot {
            best_dot = dot;
            best = i;
        }
    }
    best
}

fn normalize_dir(dx: f64, dy: f64) -> (f64, f64) {
    let len = dx.hypot(dy);
    if len < 1e-6 { (0.0, 0.0) } else { (dx / len, dy / len) }
}

/// Nearest hit test: direction-based (Blender-style).
fn find_nearest(_layers: &[PieLayer], _center: f64, dx: f64, dy: f64) -> Option<usize> {
    let s = crate::gpu::RENDER_SCALE as f64;
    let bx = dx * s;
    let by = dy * s;
    let r = bx.hypot(by);
    let n_colors = COLOR_BUTTONS.len();

    if r < PIE_DEAD_ZONE { return None; }
    let dir = normalize_dir(bx, by);

    if r >= COLOR_INNER_R && r <= COLOR_OUTER_R {
        return Some(closest_by_direction(dir, &color_dirs()));
    }
    Some(n_colors + closest_by_direction(dir, &tool_dirs()))
}

/// Overlap hit test: precise hit first, then direction-based fallback.
fn hit_test_overlap(layers: &[PieLayer], center: f64, dx: f64, dy: f64) -> Option<usize> {
    let s = crate::gpu::RENDER_SCALE as f64;
    let bx = dx * s;
    let by = dy * s;
    let r = bx.hypot(by);
    let n_colors = COLOR_BUTTONS.len();

    if r < PIE_DEAD_ZONE { return None; }
    let dir = normalize_dir(bx, by);

    // A. Precise hit: inside color ring.
    if r >= COLOR_INNER_R && r <= COLOR_OUTER_R {
        return Some(closest_by_direction(dir, &color_dirs()));
    }

    // A. Precise hit: inside a tool button's bounding rect.
    let buf_bx = center + bx;
    let buf_by = center + by;
    for (i, btn) in layers[1].buttons.iter().enumerate() {
        let lx = (buf_bx - (btn.cx - btn.hw)).round() as i32;
        let ly = (buf_by - (btn.cy - btn.hh)).round() as i32;
        if point_in_rounded_rect(lx, ly, (btn.hw * 2.0) as i32, (btn.hh * 2.0) as i32, btn.cr as i32) {
            return Some(n_colors + i);
        }
    }

    // B. Fallback: closest tool by direction.
    Some(n_colors + closest_by_direction(dir, &tool_dirs()))
}

// ── Render pie menu sprite ──

/// Get the label text for a flat button index across all layers.
fn flat_label(layers: &[PieLayer], idx: usize) -> Option<&'static str> {
    let mut flat = 0;
    for layer in layers {
        for btn in &layer.buttons {
            if flat == idx {
                return match btn.action {
                    PieAction::Tool(t) => Some(t.label()),
                    PieAction::Color(c) => {
                        COLOR_BUTTONS.iter().find(|(col, _)| *col == c).map(|(_, n)| *n)
                    }
                };
            }
            flat += 1;
        }
    }
    None
}

fn flat_label_long(layers: &[PieLayer], idx: usize) -> Option<&'static str> {
    fn color_name(c: [u8; 3]) -> &'static str {
        match c {
            [255, 0, 0] => "Red", [255, 140, 0] => "Orange",
            [255, 255, 0] => "Yellow", [0, 200, 0] => "Green",
            [0, 150, 255] => "Blue", [140, 0, 255] => "Purple",
            [255, 255, 255] => "White", [0, 0, 0] => "Black",
            _ => "Color",
        }
    }
    let mut flat = 0;
    for layer in layers {
        for btn in &layer.buttons {
            if flat == idx {
                return match btn.action {
                    PieAction::Tool(t) => Some(t.label_long()),
                    PieAction::Color(c) => Some(color_name(c)),
                };
            }
            flat += 1;
        }
    }
    None
}

fn render_pie_menu(
    current_tool: DrawTool, _current_color: [u8; 3],
    hovered: Option<usize>, tooltip: Option<&str>,
    icons: &IconCache,
) -> OsdSprite {
    let (layers, total_r) = compute_layout(current_tool, _current_color);
    let diameter = (total_r * 2.0).ceil() as i32;
    let mut buf = RgbaBuffer::new(diameter, diameter);
    let center = total_r;

    let hovered_color = hovered.and_then(|idx| {
        if idx < COLOR_BUTTONS.len() { Some(COLOR_BUTTONS[idx].0) } else { None }
    });

    // 1. Draw color annulus.
    draw_color_annulus(&mut buf.data, diameter, diameter, center, hovered_color);

    // 2. Draw tool buttons.
    let n_colors = COLOR_BUTTONS.len();
    for (i, btn) in layers[1].buttons.iter().enumerate() {
        let is_hovered = hovered == Some(n_colors + i);
        draw_tool_button(&mut buf.data, diameter, diameter, btn, is_hovered, icons);
    }

    // 3. Tooltip: dark pill + white text near the hovered button.
    if let Some(label) = tooltip {
        let s = 4;
        let lw = label.len() as i32 * 6 * s;
        let lh = 8 * s;
        let pad_x = 12;
        let pad_y = 8;

        // Find the hovered button's center position.
        let btn_center = hovered.and_then(|idx| {
            if idx < n_colors {
                None // colors don't get tooltips
            } else {
                layers[1].buttons.get(idx - n_colors).map(|b| (b.cx, b.cy))
            }
        });

        if let Some((bx, by)) = btn_center {
            // Position tooltip above the button.
            let tx = bx as i32 - lw / 2;
            let ty = by as i32 - TOOL_HH as i32 - lh - pad_y * 2 - 8;
            let bg_x0 = tx - pad_x;
            let bg_y0 = ty - pad_y;
            let bg_w = lw + pad_x * 2;
            let bg_h = lh + pad_y * 2;
            fill_rounded_rect(
                &mut buf.data, diameter, diameter,
                bg_x0, bg_y0, bg_w, bg_h, 10,
                [30, 30, 35, 220],
            );
            draw_text_scaled(
                &mut buf.data, diameter, diameter,
                tx, ty, label, [255, 255, 255], s,
            );
        }
    }

    OsdSprite { buffer: buf, outline: None, x: 0, y: 0, width: diameter, height: diameter }
}

// ═══════════════════════════════════════════════════════════════════
// Erase helpers
// ═══════════════════════════════════════════════════════════════════

/// Minimum distance from a point to a line segment (in capture px).
fn point_to_segment_dist(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;
    let t = if len_sq < 1e-12 {
        0.0
    } else {
        ((px - ax) * dx + (py - ay) * dy) / len_sq
    };
    let t = t.clamp(0.0, 1.0);
    let proj_x = ax + t * dx;
    let proj_y = ay + t * dy;
    ((px - proj_x).powi(2) + (py - proj_y).powi(2)).sqrt()
}

/// Check if a single erase point is within `radius` of an annotation.
fn point_near_annotation(px: f64, py: f64, ann: &Annotation, radius: f64) -> bool {
    match ann {
        Annotation::Freehand { points, .. } => {
            points.windows(2).any(|w| point_to_segment_dist(px, py, w[0].0, w[0].1, w[1].0, w[1].1) < radius)
        }
        Annotation::Line { start, end, .. } => {
            point_to_segment_dist(px, py, start.0, start.1, end.0, end.1) < radius
        }
        Annotation::Box { top_left, bottom_right, .. } => {
            let (x0, y0) = top_left;
            let (x1, y1) = bottom_right;
            point_to_segment_dist(px, py, *x0, *y0, *x1, *y0) < radius
                || point_to_segment_dist(px, py, *x1, *y0, *x1, *y1) < radius
                || point_to_segment_dist(px, py, *x1, *y1, *x0, *y1) < radius
                || point_to_segment_dist(px, py, *x0, *y1, *x0, *y0) < radius
        }
        Annotation::Arrow { start, end, .. } => {
            point_to_segment_dist(px, py, start.0, start.1, end.0, end.1) < radius
        }
        Annotation::Circle { center, radius: r, .. } => {
            let dist = ((px - center.0).powi(2) + (py - center.1).powi(2)).sqrt();
            (dist - r).abs() < radius
        }
        Annotation::Number { pos, .. } => {
            ((px - pos.0).powi(2) + (py - pos.1).powi(2)).sqrt() < radius
        }
    }
}

/// Check if any erase stroke point is near an annotation.
fn annotation_intersects_stroke(ann: &Annotation, stroke: &[(f64, f64)], radius: f64) -> bool {
    stroke.iter().any(|&(px, py)| point_near_annotation(px, py, ann, radius))
}

// ═══════════════════════════════════════════════════════════════════
// State
// ═══════════════════════════════════════════════════════════════════

pub struct DrawModeState {
    pub tool: DrawTool,
    pub color: [u8; 3],
    pub annotations: Vec<Annotation>,
    pub redo_stack: Vec<Annotation>,
    pub next_number: i32,
    pub drawing: Option<DrawingInProgress>,
    pub drawing_held: bool,
    pub space_held: bool,
    pub pie_hover: Option<usize>,
    pub viewport_size: (i32, i32),
    pub free_cursor_offset: (f64, f64),
    pub cached_pie_sprite: Option<OsdSprite>,
    cached_tooltip: bool,
    pub selection_style: SelectionStyle,
    icons: IconCache,
    /// When the current hover started (for tooltip delay).
    hover_start: Option<std::time::Instant>,
    /// The hover index when the timer started.
    hover_timer_idx: Option<usize>,
    /// Whether an annotation tool is active (selected from pie menu).
    /// LMB drawing is only allowed when this is true.
    pub annotation_active: bool,

    /// The (view_center, zoom, vp_w, vp_h) the cached overlay was rendered at.
    overlay_view_key: Option<((f64, f64), f64, i32, i32)>,
    /// Index into annotations vec — how many were rendered in the cache.
    overlay_annotation_count: usize,
    /// Number of points rendered in the current drawing's overlay cache.
    overlay_drawing_points: usize,
    /// Monotonically increasing version counter, bumped on every annotation
    /// change (commit, undo, redo, erase). The engine uses this to detect
    /// when the overlay needs re-rendering vs. GPU UV offset shift.
    overlay_version: u64,
}

impl DrawModeState {
    pub fn new() -> Self {
        Self {
            tool: DrawTool::Freehand, color: [255, 0, 0],
            annotations: Vec::new(), redo_stack: Vec::new(), next_number: 1,
            drawing: None, drawing_held: false, space_held: false,
            pie_hover: None, viewport_size: (0, 0),
            free_cursor_offset: (0.0, 0.0),            cached_pie_sprite: None,
            cached_tooltip: false,
            selection_style: SelectionStyle::Nearest,
            icons: IconCache::new(),
            hover_start: None,
            hover_timer_idx: None,
            annotation_active: false,

            overlay_view_key: None,
            overlay_annotation_count: 0,
            overlay_drawing_points: 0,
            overlay_version: 0,
        }
    }

    pub fn begin_space_hold(&mut self, vp_w: i32, vp_h: i32) {
        self.space_held = true;
        self.pie_hover = None;
        self.free_cursor_offset = (0.0, 0.0);
        self.viewport_size = (vp_w, vp_h);
        self.cached_pie_sprite = None;
    }

    pub fn end_space_hold(&mut self) -> (f64, f64) {
        if let Some(idx) = self.pie_hover {
            let (layers, _) = compute_layout(self.tool, self.color);
            let mut flat = 0;
 'outer: for layer in &layers {
                for btn in &layer.buttons {
                    if flat == idx {
                        match btn.action {
                            PieAction::Tool(t) => self.tool = t,
                            PieAction::Color(c) => self.color = c,
                        }
                        self.annotation_active = true;
                        break 'outer;
                    }
                    flat += 1;
                }
            }
        }
        let offset = self.free_cursor_offset;
        self.space_held = false;
        self.pie_hover = None;
        self.free_cursor_offset = (0.0, 0.0);
        self.cached_pie_sprite = None;
        offset
    }

    pub fn cancel_space_hold(&mut self) {
        self.space_held = false;
        self.pie_hover = None;
        self.free_cursor_offset = (0.0, 0.0);
        self.cached_pie_sprite = None;
        self.annotation_active = false;
    }

    /// Exit annotation mode (e.g. via Escape).
    pub fn deactivate_annotation(&mut self) {
        self.annotation_active = false;
        self.drawing = None;
        self.drawing_held = false;
    }

    pub fn update_pie_hover(&mut self, cursor_offset: (f64, f64)) {
        let (layers, total_r) = compute_layout(self.tool, self.color);
        let new_hover = match self.selection_style {
            SelectionStyle::Nearest => find_nearest(&layers, total_r, cursor_offset.0, cursor_offset.1),
            SelectionStyle::Overlap => hit_test_overlap(&layers, total_r, cursor_offset.0, cursor_offset.1),
        };
        if new_hover != self.pie_hover {
            self.pie_hover = new_hover;
            self.hover_start = Some(std::time::Instant::now());
            self.hover_timer_idx = new_hover;
            self.cached_pie_sprite = None;
        }
    }

    /// Check if the current hover has been active long enough for a tooltip.
    pub fn tooltip_label(&self) -> Option<&'static str> {
        let idx = self.pie_hover?;
        let start = self.hover_start?;
        if self.hover_timer_idx != self.pie_hover {
            return None; // hover changed, timer reset
        }
        if start.elapsed() < std::time::Duration::from_secs(1) {
            return None; // not yet
        }
        let (layers, _) = compute_layout(self.tool, self.color);
        flat_label_long(&layers, idx)
    }

    /// Time until the tooltip should appear, if applicable.
    pub fn tooltip_redraw_after(&self) -> Option<std::time::Duration> {
        let idx = self.pie_hover?;
        let start = self.hover_start?;
        if self.hover_timer_idx != self.pie_hover {
            return None;
        }
        let elapsed = start.elapsed();
        let delay = std::time::Duration::from_secs(1);
        if elapsed >= delay {
            Some(std::time::Duration::ZERO)
        } else {
            Some(delay - elapsed)
        }
    }

    pub fn pie_menu_sprite(&mut self) -> Option<&OsdSprite> {
        let wants_tooltip = self.tooltip_label().is_some();
        // Invalidate if tooltip state changed.
        if self.cached_tooltip != wants_tooltip {
            self.cached_pie_sprite = None;
        }
        if self.cached_pie_sprite.is_none() {
            let tooltip = self.tooltip_label();
            self.cached_pie_sprite = Some(render_pie_menu(
                self.tool, self.color, self.pie_hover, tooltip, &self.icons,
            ));
            self.cached_tooltip = wants_tooltip;
        }
        self.cached_pie_sprite.as_ref()
    }

    /// Render annotations overlay into an external buffer.
    /// - Annotations are rendered once into the canvas.
    /// - Freehand drawing appends segments incrementally.
    /// - Non-freehand drawing clears only the dirty region and redraws.
    /// The overlay is at logical (viewport) resolution — the GPU upscales
    /// it via LINEAR filtering. This avoids the RENDER_SCALE mismatch and
    /// reduces buffer size by 4×.
    pub fn annotations_overlay(
        &mut self,
        target: &mut Option<RgbaBuffer>,
        vp_w: i32, vp_h: i32,
        view_center: (f64, f64), zoom: f64,
    ) {
        let key = (view_center, zoom, vp_w, vp_h);
        let n_ann = self.annotations.len();
        let is_freehand = self.drawing.as_ref().map_or(false, |d| d.tool == DrawTool::Freehand);
        let anns_changed = self.overlay_view_key != Some(key)
            || self.overlay_annotation_count != n_ann;

        // Allocate or resize the canvas if needed.
        let canvas_ok = target.as_ref().map_or(false, |b| b.width == vp_w && b.height == vp_h);
        if !canvas_ok {
            *target = Some(RgbaBuffer::new(vp_w, vp_h));
        }

        if anns_changed {
            // Re-render all annotations into the persistent canvas.
            if let Some(buf) = target {
                buf.data.fill(0);
                for ann in &self.annotations {
                    render_one_annotation(buf, vp_w, vp_h, view_center, zoom, ann);
                }
            }
            self.overlay_view_key = Some(key);
            self.overlay_annotation_count = n_ann;
            self.overlay_drawing_points = 0;
        }

        // Drawing in progress.
        if let Some(ref d) = self.drawing {
            if let Some(buf) = target {
                if is_freehand {
                    // Incremental: append only new segments.
                    let n_existing = self.overlay_drawing_points;
                    let n_total = d.points.len();
                    if n_total > n_existing && n_existing > 0 {
                        for i in (n_existing - 1)..(n_total - 1) {
                            let p0 = capture_to_screen(d.points[i], view_center, zoom, vp_w, vp_h);
                            let p1 = capture_to_screen(d.points[i + 1], view_center, zoom, vp_w, vp_h);
                            draw_thick_line(&mut buf.data, vp_w, vp_h, p0, p1, 2.0, d.color);
                        }
                    } else if n_total > 0 && n_existing == 0 {
                        for w in d.points.windows(2) {
                            let p0 = capture_to_screen(w[0], view_center, zoom, vp_w, vp_h);
                            let p1 = capture_to_screen(w[1], view_center, zoom, vp_w, vp_h);
                            draw_thick_line(&mut buf.data, vp_w, vp_h, p0, p1, 2.0, d.color);
                        }
                    }
                    self.overlay_drawing_points = n_total;
                } else {
                    // Non-freehand: clear dirty region and redraw shape.
                    let (x0, y0, x1, y1) = drawing_bounds(d, view_center, zoom, vp_w, vp_h);
                    clear_region(&mut buf.data, vp_w, vp_h, x0, y0, x1, y1);
                    render_drawing_preview(buf, vp_w, vp_h, view_center, zoom, d);
                }
            }
        }
    }

    /// Invalidate the overlay cache (call when drawing is committed or view changes).
    pub fn invalidate_overlay(&mut self) {
        self.overlay_view_key = None;
        self.overlay_annotation_count = 0;
        self.overlay_drawing_points = 0;
    }

    /// Return the current overlay version counter. Bumped on every annotation
    /// change (commit, undo, redo, erase). The engine uses this to detect
    /// when the overlay needs re-rendering vs. GPU UV offset shift.
    pub fn overlay_version(&self) -> u64 {
        self.overlay_version
    }

    pub fn commit_drawing(&mut self) {
        if let Some(d) = self.drawing.take() {
            let (c, w) = (self.color, 2.0);
            let ann = match d.tool {
                DrawTool::Freehand => Annotation::Freehand { points: d.points, color: c, width: w },
                DrawTool::Erase => {
                    // Erase: remove annotations that intersect the erase stroke.
                    let stroke: Vec<_> = d.points.iter().map(|p| *p).collect();
                    let erase_radius = 10.0; // capture pixels
                    let before = self.annotations.len();
                    self.annotations.retain(|ann| {
                        !annotation_intersects_stroke(ann, &stroke, erase_radius)
                    });
                    if self.annotations.len() != before {
                        self.overlay_version = self.overlay_version.wrapping_add(1);
                    }
                    return;
                }
                DrawTool::Line => Annotation::Line { start: d.start, end: d.current, color: c, width: w },
                DrawTool::Box => Annotation::Box { top_left: (d.start.0.min(d.current.0), d.start.1.min(d.current.1)), bottom_right: (d.start.0.max(d.current.0), d.start.1.max(d.current.1)), color: c, width: w },
                DrawTool::Arrow => Annotation::Arrow { start: d.start, end: d.current, color: c, width: w },
                DrawTool::Circle => { let (dx,dy)=(d.current.0-d.start.0,d.current.1-d.start.1); Annotation::Circle { center: d.start, radius: (dx*dx+dy*dy).sqrt(), color: c, width: w } }
                DrawTool::Number => { let v=self.next_number; self.next_number+=1; Annotation::Number { pos: d.start, value: v, color: c } }
                DrawTool::Select => return,
            };
            self.redo_stack.clear();
            self.annotations.push(ann);
            self.overlay_version = self.overlay_version.wrapping_add(1);
        }
        self.invalidate_overlay();
    }

    pub fn undo(&mut self) {
        if let Some(ann) = self.annotations.pop() {
            if let Annotation::Number { value, .. } = ann { if value < self.next_number { self.next_number = value; } }
            self.redo_stack.push(ann);
            self.overlay_version = self.overlay_version.wrapping_add(1);
            self.invalidate_overlay();
        }
    }

    pub fn redo(&mut self) {
        if let Some(ann) = self.redo_stack.pop() {
            if let Annotation::Number { value, .. } = ann { if value >= self.next_number { self.next_number = value + 1; } }
            self.annotations.push(ann);
            self.overlay_version = self.overlay_version.wrapping_add(1);
            self.invalidate_overlay();
        }
    }
}

/// Clear a rectangular region in a buffer to transparent.
fn clear_region(data: &mut [u8], vp_w: i32, vp_h: i32, x0: i32, y0: i32, x1: i32, y1: i32) {
    let x0 = x0.max(0) as usize;
    let y0 = y0.max(0) as usize;
    let x1 = (x1 as usize).min(vp_w as usize);
    let y1 = (y1 as usize).min(vp_h as usize);
    for y in y0..y1 {
        let start = (y * vp_w as usize + x0) * 4;
        let end = (y * vp_w as usize + x1) * 4;
        data[start..end].fill(0);
    }
}

/// Compute bounding box of a drawing in screen coordinates.
fn drawing_bounds(d: &DrawingInProgress, view_center: (f64, f64), zoom: f64, vp_w: i32, vp_h: i32) -> (i32, i32, i32, i32) {
    let pad = 20; // extra pixels for line thickness
    match d.tool {
        DrawTool::Freehand => (0, 0, 0, 0), // handled incrementally
        DrawTool::Line | DrawTool::Arrow => {
            let p0 = capture_to_screen(d.start, view_center, zoom, vp_w, vp_h);
            let p1 = capture_to_screen(d.current, view_center, zoom, vp_w, vp_h);
            let x0 = (p0.0.min(p1.0) as i32) - pad;
            let y0 = (p0.1.min(p1.1) as i32) - pad;
            let x1 = (p0.0.max(p1.0) as i32) + pad;
            let y1 = (p0.1.max(p1.1) as i32) + pad;
            (x0, y0, x1, y1)
        }
        DrawTool::Box => {
            let tl = capture_to_screen(d.start, view_center, zoom, vp_w, vp_h);
            let br = capture_to_screen(d.current, view_center, zoom, vp_w, vp_h);
            let x0 = (tl.0.min(br.0) as i32) - pad;
            let y0 = (tl.1.min(br.1) as i32) - pad;
            let x1 = (tl.0.max(br.0) as i32) + pad;
            let y1 = (tl.1.max(br.1) as i32) + pad;
            (x0, y0, x1, y1)
        }
        DrawTool::Circle => {
            let sc = capture_to_screen(d.start, view_center, zoom, vp_w, vp_h);
            let (dx, dy) = (d.current.0 - d.start.0, d.current.1 - d.start.1);
            let r = ((dx * dx + dy * dy).sqrt() * zoom) as i32 + pad;
            (sc.0 as i32 - r, sc.1 as i32 - r, sc.0 as i32 + r, sc.1 as i32 + r)
        }
        DrawTool::Number => {
            let sp = capture_to_screen(d.start, view_center, zoom, vp_w, vp_h);
            let s = 20;
            (sp.0 as i32 - s, sp.1 as i32 - s, sp.0 as i32 + s, sp.1 as i32 + s)
        }
        _ => (0, 0, 0, 0),
    }
}

/// Render a non-freehand drawing preview (Line, Box, Arrow, Circle, Number).
fn render_drawing_preview(
    buf: &mut RgbaBuffer, vp_w: i32, vp_h: i32,
    view_center: (f64, f64), zoom: f64, d: &DrawingInProgress,
) {
    match d.tool {
        DrawTool::Line => {
            let p0 = capture_to_screen(d.start, view_center, zoom, vp_w, vp_h);
            let p1 = capture_to_screen(d.current, view_center, zoom, vp_w, vp_h);
            draw_thick_line(&mut buf.data, vp_w, vp_h, p0, p1, 2.0, d.color);
        }
        DrawTool::Box => {
            let tl = capture_to_screen(d.start, view_center, zoom, vp_w, vp_h);
            let br = capture_to_screen(d.current, view_center, zoom, vp_w, vp_h);
            let (x0,y0) = (tl.0.round() as i32, tl.1.round() as i32);
            let (x1,y1) = (br.0.round() as i32, br.1.round() as i32);
            draw_thick_line(&mut buf.data, vp_w, vp_h, (x0 as f64, y0 as f64), (x1 as f64, y0 as f64), 2.0, d.color);
            draw_thick_line(&mut buf.data, vp_w, vp_h, (x1 as f64, y0 as f64), (x1 as f64, y1 as f64), 2.0, d.color);
            draw_thick_line(&mut buf.data, vp_w, vp_h, (x1 as f64, y1 as f64), (x0 as f64, y1 as f64), 2.0, d.color);
            draw_thick_line(&mut buf.data, vp_w, vp_h, (x0 as f64, y1 as f64), (x0 as f64, y0 as f64), 2.0, d.color);
        }
        DrawTool::Arrow => {
            let p0 = capture_to_screen(d.start, view_center, zoom, vp_w, vp_h);
            let p1 = capture_to_screen(d.current, view_center, zoom, vp_w, vp_h);
            draw_thick_line(&mut buf.data, vp_w, vp_h, p0, p1, 2.0, d.color);
            let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
            let len = (dx*dx + dy*dy).sqrt();
            if len > 1.0 {
                let (ux, uy) = (dx/len, dy/len);
                let (hl, hw) = (12.0, 6.0);
                draw_thick_line(&mut buf.data, vp_w, vp_h, p1, (p1.0-ux*hl+uy*hw, p1.1-uy*hl-ux*hw), 2.0, d.color);
                draw_thick_line(&mut buf.data, vp_w, vp_h, p1, (p1.0-ux*hl-uy*hw, p1.1-uy*hl+ux*hw), 2.0, d.color);
            }
        }
        DrawTool::Circle => {
            let sc = capture_to_screen(d.start, view_center, zoom, vp_w, vp_h);
            let (dx,dy) = (d.current.0-d.start.0, d.current.1-d.start.1);
            draw_thick_circle(&mut buf.data, vp_w, vp_h, sc, ((dx*dx+dy*dy).sqrt())*zoom, 2.0, d.color);
        }
        DrawTool::Number => {
            let sp = capture_to_screen(d.start, view_center, zoom, vp_w, vp_h);
            draw_text_scaled(&mut buf.data, vp_w, vp_h, sp.0.round() as i32, sp.1.round() as i32, "#?", d.color, 1);
        }
        _ => {}
    }
}
