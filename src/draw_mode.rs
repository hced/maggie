/// Annotation tools for the magnified view.
///
/// There is no "annotation mode" — LMB always draws with the last-used
/// tool (default: Freehand). Holding Space shows the Annotation UI;
/// panning pauses and the cursor detaches from center at 1:1 size.
/// Releasing Space activates the highlighted component.

use crate::osd::OsdSprite;
use crate::render::RgbaBuffer;

// ── Drawing tools ──

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
}

// ── Selection style ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionStyle {
    Nearest,
    Overlap,
}

// ── Annotations ──

#[derive(Debug, Clone)]
pub enum Annotation {
    Freehand {
        points: Vec<(f64, f64)>,
        color: [u8; 3],
        width: f32,
    },
    Line {
        start: (f64, f64),
        end: (f64, f64),
        color: [u8; 3],
        width: f32,
    },
    Box {
        top_left: (f64, f64),
        bottom_right: (f64, f64),
        color: [u8; 3],
        width: f32,
    },
    Arrow {
        start: (f64, f64),
        end: (f64, f64),
        color: [u8; 3],
        width: f32,
    },
    Circle {
        center: (f64, f64),
        radius: f64,
        color: [u8; 3],
        width: f32,
    },
    Number {
        pos: (f64, f64),
        value: i32,
        color: [u8; 3],
    },
}

impl Annotation {
    pub fn color(&self) -> [u8; 3] {
        match self {
            Annotation::Freehand { color, .. }
            | Annotation::Line { color, .. }
            | Annotation::Box { color, .. }
            | Annotation::Arrow { color, .. }
            | Annotation::Circle { color, .. }
            | Annotation::Number { color, .. } => *color,
        }
    }
}

// ── Drawing in-progress ──

#[derive(Debug, Clone)]
pub struct DrawingInProgress {
    pub tool: DrawTool,
    pub start: (f64, f64),
    pub current: (f64, f64),
    pub points: Vec<(f64, f64)>,
}

impl DrawingInProgress {
    pub fn new(tool: DrawTool, start: (f64, f64)) -> Self {
        Self {
            tool,
            start,
            current: start,
            points: vec![start],
        }
    }
}

// ── Palette ──

pub const PALETTE: &[[u8; 3]] = &[
    [255, 0, 0],    // Red
    [255, 140, 0],   // Orange
    [255, 255, 0],   // Yellow
    [0, 200, 0],     // Green
    [0, 150, 255],   // Blue
    [140, 0, 255],   // Purple
    [255, 255, 255], // White
    [0, 0, 0],       // Black
];

// ── Rendering primitives ──

fn capture_to_screen(
    cap: (f64, f64),
    view_center: (f64, f64),
    zoom: f64,
    vp_w: i32,
    vp_h: i32,
) -> (f64, f64) {
    let sx = (cap.0 - view_center.0) * zoom + vp_w as f64 / 2.0;
    let sy = (cap.1 - view_center.1) * zoom + vp_h as f64 / 2.0;
    (sx, sy)
}

pub(crate) fn blend_pixel(pixel: &mut [u8], color: [u8; 3], alpha: u8) {
    if alpha == 0 {
        return;
    }
    let a = alpha as u16;
    let inv = 255 - a;
    pixel[0] = ((color[0] as u16 * a + pixel[0] as u16 * inv) / 255) as u8;
    pixel[1] = ((color[1] as u16 * a + pixel[1] as u16 * inv) / 255) as u8;
    pixel[2] = ((color[2] as u16 * a + pixel[2] as u16 * inv) / 255) as u8;
    pixel[3] = ((255u16 * a + pixel[3] as u16 * inv) / 255) as u8;
}

fn draw_thick_line(
    buf: &mut [u8],
    w: i32,
    h: i32,
    p0: (f64, f64),
    p1: (f64, f64),
    thickness: f32,
    color: [u8; 3],
) {
    let dx = p1.0 - p0.0;
    let dy = p1.1 - p0.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        return;
    }
    let steps = (len * 2.0).ceil() as i32;
    let half_t = thickness / 2.0;
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let x = p0.0 + dx * t;
        let y = p0.1 + dy * t;
        let x0 = (x - half_t as f64).floor() as i32;
        let x1 = (x + half_t as f64).ceil() as i32;
        let y0 = (y - half_t as f64).floor() as i32;
        let y1 = (y + half_t as f64).ceil() as i32;
        for py in y0.max(0)..y1.min(h) {
            for px in x0.max(0)..x1.min(w) {
                let idx = (py as usize * w as usize + px as usize) * 4;
                let dist_x = (px as f64 - x).abs();
                let dist_y = (py as f64 - y).abs();
                let dist = dist_x.max(dist_y);
                if dist <= half_t as f64 + 0.5 {
                    let alpha = ((1.0 - (dist / (half_t as f64 + 0.5)).min(1.0))
                        * 255.0) as u8;
                    blend_pixel(&mut buf[idx..idx + 4], color, alpha);
                }
            }
        }
    }
}

fn draw_thick_circle(
    buf: &mut [u8],
    w: i32,
    h: i32,
    center: (f64, f64),
    radius: f64,
    thickness: f32,
    color: [u8; 3],
) {
    if radius < 0.5 {
        return;
    }
    let half_t = thickness / 2.0;
    let steps = (std::f64::consts::TAU * radius * 2.0).ceil() as i32;
    for i in 0..steps {
        let angle = std::f64::consts::TAU * i as f64 / steps as f64;
        let x = center.0 + radius * angle.cos();
        let y = center.1 + radius * angle.sin();
        let x0 = (x - half_t as f64 - 1.0).floor() as i32;
        let x1 = (x + half_t as f64 + 1.0).ceil() as i32;
        let y0 = (y - half_t as f64 - 1.0).floor() as i32;
        let y1 = (y + half_t as f64 + 1.0).ceil() as i32;
        for py in y0.max(0)..y1.min(h) {
            for px in x0.max(0)..x1.min(w) {
                let dist = ((px as f64 - x).powi(2)
                    + (py as f64 - y).powi(2))
                .sqrt();
                if dist <= half_t as f64 + 1.0 {
                    let alpha = ((1.0
                        - (dist / (half_t as f64 + 1.0)).min(1.0))
                        * 255.0) as u8;
                    let idx =
                        (py as usize * w as usize + px as usize) * 4;
                    blend_pixel(&mut buf[idx..idx + 4], color, alpha);
                }
            }
        }
    }
}

fn draw_text_scaled(
    buf: &mut [u8],
    buf_w: i32,
    buf_h: i32,
    x: i32,
    y: i32,
    text: &str,
    color: [u8; 3],
    s: i32,
) {
    let s = s.max(1);
    for (ci, ch) in text.chars().enumerate() {
        let Some(rows) = crate::osd::glyph(ch) else {
            continue;
        };
        let gx = x + ci as i32 * 6 * s;
        for (gy, row) in rows.iter().enumerate() {
            for bit in 0..5 {
                if row & (1 << (4 - bit)) != 0 {
                    for dy in 0..s {
                        for dx in 0..s {
                            let px = gx + bit * s + dx;
                            let py = y + gy as i32 * s + dy;
                            if px >= 0 && px < buf_w && py >= 0 && py < buf_h {
                                let idx = (py as usize * buf_w as usize
                                    + px as usize)
                                    * 4;
                                buf[idx] = color[0];
                                buf[idx + 1] = color[1];
                                buf[idx + 2] = color[2];
                                buf[idx + 3] = 255;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Render all annotations into a viewport-sized overlay buffer.
pub fn render_annotations_overlay(
    annotations: &[Annotation],
    drawing: Option<&DrawingInProgress>,
    viewport_w: i32,
    viewport_h: i32,
    view_center: (f64, f64),
    zoom: f64,
) -> Option<RgbaBuffer> {
    if annotations.is_empty() && drawing.is_none() {
        return None;
    }

    let mut buf = RgbaBuffer::new(viewport_w, viewport_h);

    let draw_annotation = |buf: &mut RgbaBuffer, ann: &Annotation| {
        let c = ann.color();
        match ann {
            Annotation::Freehand { points, width, .. } => {
                if points.len() < 2 {
                    return;
                }
                for window in points.windows(2) {
                    let p0 = capture_to_screen(
                        window[0], view_center, zoom, viewport_w,
                        viewport_h,
                    );
                    let p1 = capture_to_screen(
                        window[1], view_center, zoom, viewport_w,
                        viewport_h,
                    );
                    draw_thick_line(
                        &mut buf.data, viewport_w, viewport_h,
                        p0, p1, *width, c,
                    );
                }
            }
            Annotation::Line { start, end, width, .. } => {
                let p0 = capture_to_screen(
                    *start, view_center, zoom, viewport_w, viewport_h,
                );
                let p1 = capture_to_screen(
                    *end, view_center, zoom, viewport_w, viewport_h,
                );
                draw_thick_line(
                    &mut buf.data, viewport_w, viewport_h,
                    p0, p1, *width, c,
                );
            }
            Annotation::Box {
                top_left,
                bottom_right,
                width,
                ..
            } => {
                let tl = capture_to_screen(
                    *top_left, view_center, zoom, viewport_w,
                    viewport_h,
                );
                let br = capture_to_screen(
                    *bottom_right, view_center, zoom, viewport_w,
                    viewport_h,
                );
                let (x0, y0) = (tl.0.round() as i32, tl.1.round() as i32);
                let (x1, y1) = (br.0.round() as i32, br.1.round() as i32);
                let w = *width;
                draw_thick_line(&mut buf.data, viewport_w, viewport_h, (x0 as f64, y0 as f64), (x1 as f64, y0 as f64), w, c);
                draw_thick_line(&mut buf.data, viewport_w, viewport_h, (x1 as f64, y0 as f64), (x1 as f64, y1 as f64), w, c);
                draw_thick_line(&mut buf.data, viewport_w, viewport_h, (x1 as f64, y1 as f64), (x0 as f64, y1 as f64), w, c);
                draw_thick_line(&mut buf.data, viewport_w, viewport_h, (x0 as f64, y1 as f64), (x0 as f64, y0 as f64), w, c);
            }
            Annotation::Arrow { start, end, width, .. } => {
                let p0 = capture_to_screen(
                    *start, view_center, zoom, viewport_w, viewport_h,
                );
                let p1 = capture_to_screen(
                    *end, view_center, zoom, viewport_w, viewport_h,
                );
                draw_thick_line(
                    &mut buf.data, viewport_w, viewport_h,
                    p0, p1, *width, c,
                );
                let dx = p1.0 - p0.0;
                let dy = p1.1 - p0.1;
                let len = (dx * dx + dy * dy).sqrt();
                if len > 1.0 {
                    let ux = dx / len;
                    let uy = dy / len;
                    let head_len = 12.0;
                    let head_w = 6.0;
                    let h1 = (
                        p1.0 - ux * head_len + uy * head_w,
                        p1.1 - uy * head_len - ux * head_w,
                    );
                    let h2 = (
                        p1.0 - ux * head_len - uy * head_w,
                        p1.1 - uy * head_len + ux * head_w,
                    );
                    draw_thick_line(&mut buf.data, viewport_w, viewport_h, p1, h1, *width, c);
                    draw_thick_line(&mut buf.data, viewport_w, viewport_h, p1, h2, *width, c);
                }
            }
            Annotation::Circle {
                center, radius, width, ..
            } => {
                let sc = capture_to_screen(
                    *center, view_center, zoom, viewport_w, viewport_h,
                );
                let sr = radius * zoom;
                draw_thick_circle(
                    &mut buf.data, viewport_w, viewport_h, sc, sr,
                    *width, c,
                );
            }
            Annotation::Number { pos, value, .. } => {
                let sp = capture_to_screen(
                    *pos, view_center, zoom, viewport_w, viewport_h,
                );
                draw_text_scaled(
                    &mut buf.data,
                    viewport_w,
                    viewport_h,
                    sp.0.round() as i32,
                    sp.1.round() as i32,
                    &format!("{value}"),
                    c,
                    1,
                );
            }
        }
    };

    for ann in annotations {
        draw_annotation(&mut buf, ann);
    }
    if let Some(d) = drawing {
        match d.tool {
            DrawTool::Freehand => {
                if d.points.len() >= 2 {
                    for window in d.points.windows(2) {
                        let p0 = capture_to_screen(window[0], view_center, zoom, viewport_w, viewport_h);
                        let p1 = capture_to_screen(window[1], view_center, zoom, viewport_w, viewport_h);
                        draw_thick_line(&mut buf.data, viewport_w, viewport_h, p0, p1, 2.0, [255, 0, 0]);
                    }
                }
            }
            DrawTool::Erase => {
                if d.points.len() >= 2 {
                    for window in d.points.windows(2) {
                        let p0 = capture_to_screen(window[0], view_center, zoom, viewport_w, viewport_h);
                        let p1 = capture_to_screen(window[1], view_center, zoom, viewport_w, viewport_h);
                        draw_thick_line(&mut buf.data, viewport_w, viewport_h, p0, p1, 10.0, [255, 255, 255]);
                    }
                }
            }
            DrawTool::Line => {
                let p0 = capture_to_screen(d.start, view_center, zoom, viewport_w, viewport_h);
                let p1 = capture_to_screen(d.current, view_center, zoom, viewport_w, viewport_h);
                draw_thick_line(&mut buf.data, viewport_w, viewport_h, p0, p1, 2.0, [255, 0, 0]);
            }
            DrawTool::Box => {
                let tl = capture_to_screen(d.start, view_center, zoom, viewport_w, viewport_h);
                let br = capture_to_screen(d.current, view_center, zoom, viewport_w, viewport_h);
                let (x0, y0) = (tl.0.round() as i32, tl.1.round() as i32);
                let (x1, y1) = (br.0.round() as i32, br.1.round() as i32);
                draw_thick_line(&mut buf.data, viewport_w, viewport_h, (x0 as f64, y0 as f64), (x1 as f64, y0 as f64), 2.0, [255, 0, 0]);
                draw_thick_line(&mut buf.data, viewport_w, viewport_h, (x1 as f64, y0 as f64), (x1 as f64, y1 as f64), 2.0, [255, 0, 0]);
                draw_thick_line(&mut buf.data, viewport_w, viewport_h, (x1 as f64, y1 as f64), (x0 as f64, y1 as f64), 2.0, [255, 0, 0]);
                draw_thick_line(&mut buf.data, viewport_w, viewport_h, (x0 as f64, y1 as f64), (x0 as f64, y0 as f64), 2.0, [255, 0, 0]);
            }
            DrawTool::Arrow => {
                let p0 = capture_to_screen(d.start, view_center, zoom, viewport_w, viewport_h);
                let p1 = capture_to_screen(d.current, view_center, zoom, viewport_w, viewport_h);
                draw_thick_line(&mut buf.data, viewport_w, viewport_h, p0, p1, 2.0, [255, 0, 0]);
                let dx = p1.0 - p0.0;
                let dy = p1.1 - p0.1;
                let len = (dx * dx + dy * dy).sqrt();
                if len > 1.0 {
                    let ux = dx / len;
                    let uy = dy / len;
                    let head_len = 12.0;
                    let head_w = 6.0;
                    let h1 = (p1.0 - ux * head_len + uy * head_w, p1.1 - uy * head_len - ux * head_w);
                    let h2 = (p1.0 - ux * head_len - uy * head_w, p1.1 - uy * head_len + ux * head_w);
                    draw_thick_line(&mut buf.data, viewport_w, viewport_h, p1, h1, 2.0, [255, 0, 0]);
                    draw_thick_line(&mut buf.data, viewport_w, viewport_h, p1, h2, 2.0, [255, 0, 0]);
                }
            }
            DrawTool::Circle => {
                let sc = capture_to_screen(d.start, view_center, zoom, viewport_w, viewport_h);
                let dx = d.current.0 - d.start.0;
                let dy = d.current.1 - d.start.1;
                let sr = ((dx * dx + dy * dy).sqrt()) * zoom;
                draw_thick_circle(&mut buf.data, viewport_w, viewport_h, sc, sr, 2.0, [255, 0, 0]);
            }
            DrawTool::Number => {
                let sp = capture_to_screen(d.start, view_center, zoom, viewport_w, viewport_h);
                draw_text_scaled(&mut buf.data, viewport_w, viewport_h, sp.0.round() as i32, sp.1.round() as i32, "#?", [255, 0, 0], 1);
            }
            DrawTool::Select => {}
        }
    }

    Some(buf)
}

// ── Annotation UI placeholder ──
// TODO: Rewrite pie menu from scratch with visible SVG icons.

/// Render the annotation UI (placeholder — transparent, invisible).
pub fn render_annotation_ui(
    _current_tool: DrawTool,
    _current_color: [u8; 3],
    _hovered: Option<usize>,
    _cursor_offset: (f64, f64),
) -> Option<OsdSprite> {
    None
}

/// Hit-test the annotation UI against cursor offset from viewport center.
pub fn annotation_ui_hit_test(
    _cursor_offset: (f64, f64),
    _selection_style: SelectionStyle,
) -> Option<usize> {
    None
}

/// Apply a pie menu action by flat index.
pub fn annotation_ui_apply(_idx: usize) -> Option<(DrawTool, [u8; 3])> {
    None
}

// ── State ──

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
    pub selection_style: SelectionStyle,
}

impl DrawModeState {
    pub fn new() -> Self {
        Self {
            tool: DrawTool::Freehand,
            color: [255, 0, 0],
            annotations: Vec::new(),
            redo_stack: Vec::new(),
            next_number: 1,
            drawing: None,
            drawing_held: false,
            space_held: false,
            pie_hover: None,
            viewport_size: (0, 0),
            free_cursor_offset: (0.0, 0.0),
            cached_pie_sprite: None,
            selection_style: SelectionStyle::Nearest,
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
            if let Some((tool, color)) = annotation_ui_apply(idx) {
                self.tool = tool;
                self.color = color;
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
    }

    pub fn update_pie_hover(&mut self, cursor_offset: (f64, f64)) {
        self.pie_hover =
            annotation_ui_hit_test(cursor_offset, self.selection_style);
    }

    pub fn pie_menu_sprite(&mut self) -> Option<&OsdSprite> {
        if self.cached_pie_sprite.is_none() {
            self.cached_pie_sprite = render_annotation_ui(
                self.tool,
                self.color,
                self.pie_hover,
                self.free_cursor_offset,
            );
        }
        self.cached_pie_sprite.as_ref()
    }

    pub fn commit_drawing(&mut self) {
        if let Some(d) = self.drawing.take() {
            let c = self.color;
            let w = 2.0;
            let ann = match d.tool {
                DrawTool::Freehand => Annotation::Freehand {
                    points: d.points,
                    color: c,
                    width: w,
                },
                DrawTool::Erase => return,
                DrawTool::Line => Annotation::Line {
                    start: d.start,
                    end: d.current,
                    color: c,
                    width: w,
                },
                DrawTool::Box => Annotation::Box {
                    top_left: (
                        d.start.0.min(d.current.0),
                        d.start.1.min(d.current.1),
                    ),
                    bottom_right: (
                        d.start.0.max(d.current.0),
                        d.start.1.max(d.current.1),
                    ),
                    color: c,
                    width: w,
                },
                DrawTool::Arrow => Annotation::Arrow {
                    start: d.start,
                    end: d.current,
                    color: c,
                    width: w,
                },
                DrawTool::Circle => {
                    let dx = d.current.0 - d.start.0;
                    let dy = d.current.1 - d.start.1;
                    Annotation::Circle {
                        center: d.start,
                        radius: (dx * dx + dy * dy).sqrt(),
                        color: c,
                        width: w,
                    }
                }
                DrawTool::Number => {
                    let val = self.next_number;
                    self.next_number += 1;
                    Annotation::Number {
                        pos: d.start,
                        value: val,
                        color: c,
                    }
                }
                DrawTool::Select => return,
            };
            self.redo_stack.clear();
            self.annotations.push(ann);
        }
    }

    pub fn undo(&mut self) {
        if let Some(ann) = self.annotations.pop() {
            if let Annotation::Number { value, .. } = ann {
                if value < self.next_number {
                    self.next_number = value;
                }
            }
            self.redo_stack.push(ann);
        }
    }

    pub fn redo(&mut self) {
        if let Some(ann) = self.redo_stack.pop() {
            if let Annotation::Number { value, .. } = ann {
                if value >= self.next_number {
                    self.next_number = value + 1;
                }
            }
            self.annotations.push(ann);
        }
    }
}
