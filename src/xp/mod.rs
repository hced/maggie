//! Cross-platform engine using winit 0.30 + wgpu 23.
//!
//! Implements `winit::application::ApplicationHandler` for the trait-based
//! event model. Screen capture is platform-gated.

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, Modifiers, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState as WinitModifiers};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowAttributes, WindowId, WindowLevel};

pub mod capture;
pub mod input;
pub mod renderer;

use crate::config::MagnifierConfig;
use crate::render::RgbaBuffer;

const MIN_ZOOM: f64 = 0.1;
const MAX_ZOOM: f64 = 20.0;
const ZOOM_STEP: f64 = 0.1;
const PAN_SPEED: f64 = 30.0;
const REDRAW_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Magnify,
    Annotation,
    Capture,
}

pub struct XpState {
    pub zoom: f64,
    pub view_center: (f64, f64),
    pub mode: AppMode,
    pub config: MagnifierConfig,
    pub exit: bool,
    pub pointer_pos: (f64, f64),
    pub pointer: input::PointerState,
    pub modifiers: input::ModifiersState,
    pub capture: Option<RgbaBuffer>,
    pub osd_visible: bool,
    pub cursor_visible: bool,
    pub minimap_visible: bool,
    pub last_draw: Instant,
    /// Annotation overlay: (rgba, width, height)
    pub annotation_overlay: Option<(Vec<u8>, u32, u32)>,
    /// Screenshot selection rect in logical coords (x0, y0, x1, y1)
    pub screenshot_rect: Option<(f64, f64, f64, f64)>,
    /// Screenshot animation phase (for border pulse)
    pub screenshot_phase: f64,
    /// Configuration window open state
    pub config_open: bool,
    /// Warning text shown as a persistent overlay when requirements are missing.
    pub warning_text: Option<Vec<String>>,
    window: Option<Arc<Window>>,
    renderer: Option<renderer::XpRenderer>,
    cap: Option<Box<dyn capture::ScreenCapture>>,
}

impl XpState {
    pub fn new(config: MagnifierConfig) -> Self {
        Self {
            zoom: 1.0,
            view_center: (0.0, 0.0),
            mode: AppMode::Magnify,
            config,
            exit: false,
            pointer_pos: (0.0, 0.0),
            pointer: input::PointerState::default(),
            modifiers: input::ModifiersState::default(),
            capture: None,
            osd_visible: true,
            cursor_visible: true,
            minimap_visible: false,
            last_draw: Instant::now(),
            annotation_overlay: None,
            screenshot_rect: None,
            screenshot_phase: 0.0,
            config_open: false,
            warning_text: None,
            window: None,
            renderer: None,
            cap: None,
        }
    }

    fn zoom_at_pointer(&mut self, delta: f64) {
        let old_zoom = self.zoom;
        self.zoom = (self.zoom + delta).clamp(MIN_ZOOM, MAX_ZOOM);
        let factor = self.zoom / old_zoom;
        let win_w = self.window.as_ref().map_or(800.0, |w| w.inner_size().width as f64);
        let win_h = self.window.as_ref().map_or(600.0, |w| w.inner_size().height as f64);
        let cx = win_w / 2.0;
        let cy = win_h / 2.0;
        let dx = (self.pointer_pos.0 - cx) / old_zoom;
        let dy = (self.pointer_pos.1 - cy) / old_zoom;
        self.view_center.0 -= dx * (factor - 1.0);
        self.view_center.1 -= dy * (factor - 1.0);
    }

    fn pan(&mut self, dx: f64, dy: f64) {
        self.view_center.0 += dx;
        self.view_center.1 += dy;
    }

    fn do_render(&mut self) {
        let (Some(renderer), Some(capture)) = (&mut self.renderer, &self.capture) else {
            return;
        };

        let cap_w = capture.width as f64;
        let cap_h = capture.height as f64;
        let win_w = self.window.as_ref().map_or(800.0, |w| w.inner_size().width as f64);
        let win_h = self.window.as_ref().map_or(600.0, |w| w.inner_size().height as f64);

        let view_w = win_w / self.zoom;
        let view_h = win_h / self.zoom;
        let src_x = (self.view_center.0 - view_w / 2.0) / cap_w;
        let src_y = (self.view_center.1 - view_h / 2.0) / cap_h;
        let src_w = view_w / cap_w;
        let src_h = view_h / cap_h;

        // Build screenshot overlay (scrim + border).
        let overlay_data = if self.mode == AppMode::Capture {
            let iw = win_w as u32;
            let ih = win_h as u32;
            Some(build_screenshot_overlay(iw, ih))
        } else if self.mode == AppMode::Annotation && self.annotation_overlay.is_some() {
            self.annotation_overlay.as_ref().map(|o| {
                (o.0.clone(), o.1, o.2)
            })
        } else {
            None
        };

        // Build cursor sprite: annotation crosshair in Annotation mode, magnified in Magnify mode.
        let cursor_data = if self.mode == AppMode::Annotation {
            let cursor_size = 64;
            Some(build_annotation_cursor(cursor_size))
        } else if self.cursor_visible {
            let cursor_size = (16.0 * self.zoom) as u32;
            Some(build_crosshair_sprite(cursor_size))
        } else {
            None
        };

        // Build minimap sprite.
        let minimap_data = if self.minimap_visible {
            Some(build_minimap(&capture.data, capture.width as u32, capture.height as u32,
                self.view_center, view_w, view_h))
        } else {
            None
        };

        // Build OSD legend sprite (always show when config window is open).
        let osd_data = if self.osd_visible || self.config_open {
            Some(build_osd_sprite(self.config_open, self.zoom, self.view_center, self.mode))
        } else {
            None
        };

        // Build warning sprite (persistent overlay when requirements are missing).
        let warning_data = self.warning_text.as_ref().and_then(|lines| {
            use crate::osd::{self, Corner};
            let screen_w = win_w as i32;
            let screen_h = win_h as i32;
            osd::build_osd_sprite(lines, Corner::TopRight, screen_w, screen_h)
                .map(|sprite| (sprite.buffer.data, sprite.width as u32, sprite.height as u32))
        });

        renderer.render(
            Some([src_x as f32, src_y as f32, src_w as f32, src_h as f32]),
            capture.width as u32,
            capture.height as u32,
            overlay_data.as_ref().map(|(d, w, h)| (d.as_slice(), *w, *h)),
            cursor_data.as_ref().map(|(d, w, h)| (d.as_slice(), *w, *h, [self.pointer_pos.0 as f32 - *w as f32 / 2.0, self.pointer_pos.1 as f32 - *h as f32 / 2.0])),
            minimap_data.as_ref().map(|(d, w, h)| (d.as_slice(), *w, *h, [win_w as f32 - *w as f32 - 10.0, win_h as f32 - *h as f32 - 10.0])),
            osd_data.as_ref().map(|(d, w, h)| (d.as_slice(), *w, *h, [10.0, 10.0])),
            warning_data.as_ref().map(|(d, w, h)| (d.as_slice(), *w, *h, [(win_w - *w as f64 - 28.0) as f32, 28.0])),
        );

    }
}

impl ApplicationHandler for XpState {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attrs = WindowAttributes::default()
            .with_title("Maggie")
            .with_inner_size(LogicalSize::new(800u32, 600))
            .with_decorations(false)
            .with_window_level(WindowLevel::AlwaysOnTop);

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!("Failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };

        // Confine pointer (non-fatal if it fails — e.g. some Wayland compositors).
        let _ = window.set_cursor_grab(CursorGrabMode::Confined)
            .or_else(|_| window.set_cursor_grab(CursorGrabMode::Locked));
        window.set_cursor_visible(false);
        window.set_fullscreen(Some(Fullscreen::Borderless(None)));

        // Create renderer.
        let renderer = match pollster::block_on(renderer::XpRenderer::new(window.clone())) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to create wgpu renderer: {e}");
                event_loop.exit();
                return;
            }
        };

        // Check platform requirements and initialize screen capture.
        let (mut cap, initial_warning) = match check_platform_requirements() {
            Ok(()) => match capture::create_capture() {
                Ok(c) => (c as Box<dyn capture::ScreenCapture>, None),
                Err(e) => {
                    tracing::warn!("Screen capture failed: {e}");
                    (
                        Box::new(DemoCapture) as Box<dyn capture::ScreenCapture>,
                        Some(build_platform_warning(&e.to_string())),
                    )
                }
            },
            Err(warnings) => {
                tracing::warn!("Platform requirements missing: {warnings:?}");
                (
                    Box::new(DemoCapture) as Box<dyn capture::ScreenCapture>,
                    Some(warnings),
                )
            }
        };
        self.warning_text = initial_warning;

        // Temporary: force warning display with MAGGIE_SHOW_WARNING=1
        if std::env::var("MAGGIE_SHOW_WARNING").unwrap_or_default() == "1" {
            self.warning_text = Some(vec![
                "=== Test Warning ===".to_string(),
                "This is a test of the warning popup.".to_string(),
                "".to_string(),
                "Platform requirements check works.".to_string(),
                "Press Esc/Q to quit.".to_string(),
            ]);
        }

        // Capture the initial frame.
        match cap.capture_primary() {
            Ok(frame) => {
                self.capture = Some(RgbaBuffer {
                    width: frame.width as i32,
                    height: frame.height as i32,
                    data: frame.rgba.clone(),
                });
                self.view_center = (frame.width as f64 / 2.0, frame.height as f64 / 2.0);
                tracing::info!("Captured {}x{} frame", frame.width, frame.height);
            }
            Err(e) => {
                tracing::warn!("Initial capture failed: {e}");
            }
        }

        // Store renderer, window, and capture backend.
        self.renderer = Some(renderer);
        self.window = Some(window);
        self.cap = Some(cap);

        // Upload captured frame to GPU.
        if let (Some(renderer), Some(frame)) = (&mut self.renderer, &self.capture) {
            renderer.upload_frame(&frame.data, frame.width as u32, frame.height as u32);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.exit = true;
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    let action = input::winit_key_to_action(event.physical_key, &self.modifiers);
                    match action {
                        input::Action::Quit => {
                            self.exit = true;
                            event_loop.exit();
                        }
                        input::Action::ZoomIn => self.zoom = (self.zoom + ZOOM_STEP * 2.0).clamp(MIN_ZOOM, MAX_ZOOM),
                        input::Action::ZoomOut => self.zoom = (self.zoom - ZOOM_STEP * 2.0).clamp(MIN_ZOOM, MAX_ZOOM),
                        input::Action::ZoomReset => self.zoom = 1.0,
                        input::Action::PanLeft => self.pan(-PAN_SPEED, 0.0),
                        input::Action::PanRight => self.pan(PAN_SPEED, 0.0),
                        input::Action::PanUp => self.pan(0.0, -PAN_SPEED),
                        input::Action::PanDown => self.pan(0.0, PAN_SPEED),
                        input::Action::ToggleOsd => self.osd_visible = !self.osd_visible,
                        input::Action::ToggleCursor => self.cursor_visible = !self.cursor_visible,
                        input::Action::ToggleMinimap => self.minimap_visible = !self.minimap_visible,
                        input::Action::ConfigWindow => self.config_open = !self.config_open,
                        input::Action::ScreenshotStart => self.mode = AppMode::Capture,
                        input::Action::ScreenshotConfirm | input::Action::ScreenshotCancel => {
                            self.mode = AppMode::Magnify;
                        }
                        input::Action::AnnotationToggle => {
                            self.mode = if self.mode == AppMode::Annotation {
                                AppMode::Magnify
                            } else {
                                AppMode::Annotation
                            };
                        }
                        _ => {}
                    }
                }
                // Track modifier state.
                if event.state == ElementState::Pressed || event.state == ElementState::Released {
                    // winit 0.30 provides modifiers through WindowEvent::ModifiersChanged
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.into();
            }
            WindowEvent::MouseInput { button, state, .. } => {
                match button {
                    winit::event::MouseButton::Left => {
                        self.pointer.left_pressed = state == ElementState::Pressed;
                        if state == ElementState::Pressed && self.mode == AppMode::Magnify {
                            self.mode = AppMode::Annotation;
                            // Initialize annotation overlay buffer at screen resolution.
                            let ww = self.window.as_ref().map_or(800, |w| w.inner_size().width) as u32;
                            let wh = self.window.as_ref().map_or(600, |w| w.inner_size().height) as u32;
                            let size = (ww * wh * 4) as usize;
                            self.annotation_overlay = Some((vec![0u8; size], ww, wh));
                            tracing::debug!("Entered Annotation mode, overlay {ww}x{wh}");
                        }
                    }
                    winit::event::MouseButton::Right => {
                        self.pointer.right_pressed = state == ElementState::Pressed;
                        if state == ElementState::Pressed {
                            match self.mode {
                                AppMode::Magnify => {
                                    self.exit = true;
                                    event_loop.exit();
                                }
                                _ => {
                                    self.mode = AppMode::Magnify;
                                }
                            }
                        }
                    }
                    winit::event::MouseButton::Middle => {
                        self.pointer.middle_pressed = state == ElementState::Pressed;
                    }
                    _ => {}
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let old_pos = self.pointer_pos;
                self.pointer_pos = (position.x, position.y);
                // Draw annotation stroke when LMB held in Annotation mode.
                if self.mode == AppMode::Annotation && self.pointer.left_pressed {
                    draw_stroke_line(&mut self.annotation_overlay, old_pos, self.pointer_pos, 3.0);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let scroll = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64 * ZOOM_STEP,
                    MouseScrollDelta::PixelDelta(pos) => pos.y * 0.01,
                };
                self.zoom_at_pointer(scroll);
            }
            WindowEvent::RedrawRequested => {
                self.do_render();
                self.last_draw = Instant::now();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            _ => {}
        }


    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if self.last_draw.elapsed() >= REDRAW_INTERVAL {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }
}

/// Run the cross-platform magnifier.
pub fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("Maggie (cross-platform) starting");

    let config = crate::config::load_config().unwrap_or_default();
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut state = XpState::new(config);
    event_loop.run_app(&mut state)?;

    Ok(())
}

/// Check platform-specific requirements for screen capture.
/// Returns Ok(()) if all requirements are met, or Err with warning lines.
fn check_platform_requirements() -> Result<(), Vec<String>> {
    let mut warnings = Vec::new();

    #[cfg(target_os = "linux")]
    {
        // Check for PipeWire daemon
        if !check_command_exists("pipewire") {
            warnings.push("Missing: pipewire daemon".to_string());
            warnings.push("Install: pipewire pipewire-pulse".to_string());
        }
        // Check for xdg-desktop-portal
        if !check_command_exists("xdg-desktop-portal") {
            warnings.push("Missing: xdg-desktop-portal".to_string());
            warnings.push("Install: xdg-desktop-portal".to_string());
        }
        // Check for a portal backend
        let has_portal_backend = check_command_exists("xdg-desktop-portal-gnome")
            || check_command_exists("xdg-desktop-portal-kde")
            || check_command_exists("xdg-desktop-portal-gtk")
            || check_command_exists("xdg-desktop-portal-hyprland")
            || check_command_exists("xdg-desktop-portal-wlr")
            || check_command_exists("xdg-desktop-portal-cosmic")
            || check_command_exists("xdg-desktop-portal-generic");
        if !has_portal_backend {
            warnings.push("Missing: xdg-desktop-portal backend".to_string());
            warnings.push("Install: xdg-desktop-portal-gnome (or -kde/-wlr/-hyprland)".to_string());
        }
    }

    if !warnings.is_empty() {
        warnings.insert(0, "=== Requirements Missing ===".to_string());
        warnings.push("".to_string());
        warnings.push("Maggie will start in demo mode.".to_string());
        warnings.push("Press Esc/Q to quit.".to_string());
        return Err(warnings);
    }

    Ok(())
}

/// Check if a command exists in PATH.
#[cfg(target_os = "linux")]
fn check_command_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build platform-specific warning messages when capture fails.
fn build_platform_warning(error: &str) -> Vec<String> {
    let mut lines = vec!["=== Warning ===".to_string()];

    #[cfg(target_os = "linux")]
    {
        lines.push("Screen capture failed on Linux.".to_string());
        lines.push("".to_string());
        lines.push("Maggie needs these for screen capture:".to_string());
        lines.push("  - pipewire".to_string());
        lines.push("  - xdg-desktop-portal".to_string());
        lines.push("  - a portal backend (gnome/kde/wlr)".to_string());
        lines.push("".to_string());
        lines.push("Or use the native Wayland binary: maggie".to_string());
    }

    #[cfg(target_os = "macos")]
    {
        lines.push("Screen capture failed on macOS.".to_string());
        lines.push("".to_string());
        lines.push("Grant Screen Recording permission:".to_string());
        lines.push("System Settings > Privacy & Security".to_string());
        lines.push("> Screen Recording > add Maggie".to_string());
        lines.push("".to_string());
        lines.push("Then restart Maggie.".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        lines.push("Screen capture failed on Windows.".to_string());
        lines.push("".to_string());
        lines.push("DXGI Desktop Duplication requires:".to_string());
        lines.push("  - A compatible GPU".to_string());
        lines.push("  - A running desktop session".to_string());
    }

    // Fallback for unknown platforms
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        lines.push(format!("Error: {error}"));
    }

    if !error.is_empty() && !lines.iter().any(|l| l.contains(error)) {
        lines.push("".to_string());
        lines.push(format!("Details: {error}"));
    }

    lines.push("".to_string());
    lines.push("Starting in demo mode.".to_string());
    lines.push("Press Esc/Q to quit.".to_string());
    lines
}

/// Dummy capture for development/testing.
struct DemoCapture;

impl DemoCapture {
    fn new() -> Self { Self }
}

impl capture::ScreenCapture for DemoCapture {
    fn capture_primary(&mut self) -> anyhow::Result<capture::CapturedScreen> {
        let width = 800u32;
        let height = 600u32;
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;
                rgba[idx] = (x * 255 / width) as u8;
                rgba[idx + 1] = (y * 255 / height) as u8;
                rgba[idx + 2] = 128;
                rgba[idx + 3] = 255;
            }
        }
        Ok(capture::CapturedScreen { rgba, width, height })
    }
}

/// Build a crosshair cursor sprite (RGBA).
/// Returns (rgba_data, width, height).
fn build_crosshair_sprite(size: u32) -> (Vec<u8>, u32, u32) {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let center = size as f32 / 2.0;
    let arm_thickness = (size as f32 * 0.04).max(1.0);
    let gap = size as f32 * 0.08;

    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 + 0.5 - center).abs();
            let dy = (y as f32 + 0.5 - center).abs();
            let half = size as f32 / 2.0;

            // Horizontal arm: |y| < arm_thickness, gap <= |x| < half
            let on_h_arm = dy < arm_thickness && dx >= gap && dx < half;
            // Vertical arm: |x| < arm_thickness, gap <= |y| < half
            let on_v_arm = dx < arm_thickness && dy >= gap && dy < half;
            // Center circle
            let on_center = dx * dx + dy * dy < gap * gap * 0.5;
            // Outline around arms
            let on_h_outline = dy < arm_thickness + 1.0 && dx >= gap - 1.0 && dx < half + 1.0
                && !(dy < arm_thickness && dx >= gap && dx < half);
            let on_v_outline = dx < arm_thickness + 1.0 && dy >= gap - 1.0 && dy < half + 1.0
                && !(dx < arm_thickness && dy >= gap && dy < half);

            let idx = ((y * size + x) * 4) as usize;
            if on_h_arm || on_v_arm || on_center {
                // White pixel with black outline for visibility
                rgba[idx] = 255;
                rgba[idx + 1] = 255;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = 255;
            } else if on_h_outline || on_v_outline {
                rgba[idx] = 0;
                rgba[idx + 1] = 0;
                rgba[idx + 2] = 0;
                rgba[idx + 3] = 200;
            } else {
                rgba[idx + 3] = 0; // transparent
            }
        }
    }
    (rgba, size, size)
}

/// Draw a line segment into the annotation overlay buffer.
/// Uses Bresenham-like anti-aliased line drawing.
fn draw_stroke_line(
    overlay: &mut Option<(Vec<u8>, u32, u32)>,
    from: (f64, f64),
    to: (f64, f64),
    thickness: f32,
) {
    let Some((data, w, h)) = overlay else { return };
    let w = *w;
    let h = *h;
    let color = [255, 80, 80, 200]; // red annotation stroke
    let radius = (thickness / 2.0) as i32;

    // Bresenham-like stepping
    let dx = (to.0 - from.0).abs();
    let dy = (to.1 - from.1).abs();
    let steps = (dx.max(dy) as i32).max(1);
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let px = from.0 + (to.0 - from.0) * t;
        let py = from.1 + (to.1 - from.1) * t;
        // Draw a small filled circle at (px, py)
        let cx = px as i32;
        let cy = py as i32;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx * dx + dy * dy <= radius * radius {
                    let sx = cx + dx;
                    let sy = cy + dy;
                    if sx >= 0 && sx < w as i32 && sy >= 0 && sy < h as i32 {
                        let idx = ((sy as u32 * w + sx as u32) * 4) as usize;
                        if idx + 3 < data.len() {
                            data[idx] = color[0];
                            data[idx + 1] = color[1];
                            data[idx + 2] = color[2];
                            data[idx + 3] = color[3];
                        }
                    }
                }
            }
        }
    }
}

/// Build a screenshot-mode overlay: semi-transparent scrim + bright border.
/// Returns (rgba, width, height).
fn build_screenshot_overlay(w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let border_width = 3;
    // Scrim: dark semi-transparent fill
    for chunk in rgba.chunks_exact_mut(4) {
        chunk[0] = 0;
        chunk[1] = 0;
        chunk[2] = 0;
        chunk[3] = 120; // 47% opacity scrim
    }
    // Bright border around the edges
    let pulse = ((std::time::Instant::now().elapsed().as_secs_f64() * 3.0).sin() * 0.3 + 0.7) as u8;
    let color = [50 * pulse, 200 * pulse, 255, 220];
    for y in 0..h {
        for x in 0..w {
            let on_border = x < border_width || x >= w - border_width ||
                            y < border_width || y >= h - border_width;
            if on_border {
                let idx = ((y * w + x) * 4) as usize;
                rgba[idx..idx + 4].copy_from_slice(&color);
            }
        }
    }
    (rgba, w, h)
}

/// Build a crosshair-style annotation cursor.
/// This is the "inverted color" cursor drawn over the capture.
fn build_annotation_cursor(size: u32) -> (Vec<u8>, u32, u32) {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let center = size as f32 / 2.0;
    let arm_thickness = (size as f32 * 0.04).max(1.0);
    let gap = size as f32 * 0.08;

    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 + 0.5 - center).abs();
            let dy = (y as f32 + 0.5 - center).abs();
            let half = size as f32 / 2.0;

            let on_h_arm = dy < arm_thickness && dx >= gap && dx < half;
            let on_v_arm = dx < arm_thickness && dy >= gap && dy < half;
            let on_center = dx * dx + dy * dy < gap * gap * 0.5;

            let idx = ((y * size + x) * 4) as usize;
            if on_h_arm || on_v_arm || on_center {
                // White luminance for diff-blend inversion
                rgba[idx] = 255;
                rgba[idx + 1] = 255;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = 255;
            }
            // else: transparent (default 0,0,0,0)
        }
    }
    (rgba, size, size)
}

/// Build a minimap sprite: downsampled capture with viewport rectangle.
fn build_minimap(
    capture_data: &[u8],
    cap_w: u32,
    cap_h: u32,
    view_center: (f64, f64),
    view_w: f64,
    view_h: f64,
) -> (Vec<u8>, u32, u32) {
    let mm_w = 200u32;
    let mm_h = (mm_w as f64 * cap_h as f64 / cap_w as f64) as u32;
    let mm_h = mm_h.max(1);
    let mut rgba = vec![0u8; (mm_w * mm_h * 4) as usize];

    // Downsample: nearest-neighbor sampling
    let scale_x = cap_w as f64 / mm_w as f64;
    let scale_y = cap_h as f64 / mm_h as f64;
    for my in 0..mm_h {
        let cy = (my as f64 * scale_y) as u32;
        for mx in 0..mm_w {
            let cx = (mx as f64 * scale_x) as u32;
            let si = ((cy * cap_w + cx) * 4) as usize;
            let di = ((my * mm_w + mx) * 4) as usize;
            if si + 3 < capture_data.len() && di + 3 < rgba.len() {
                // Darken the capture for minimap
                rgba[di] = capture_data[si] / 3;
                rgba[di + 1] = capture_data[si + 1] / 3;
                rgba[di + 2] = capture_data[si + 2] / 3;
                rgba[di + 3] = 200;
            }
        }
    }

    // Draw viewport rectangle
    let vx0 = ((view_center.0 - view_w / 2.0) / cap_w as f64 * mm_w as f64) as i32;
    let vy0 = ((view_center.1 - view_h / 2.0) / cap_h as f64 * mm_h as f64) as i32;
    let vx1 = vx0 + (view_w / cap_w as f64 * mm_w as f64) as i32;
    let vy1 = vy0 + (view_h / cap_h as f64 * mm_h as f64) as i32;
    let border = 2i32;
    for my in 0..mm_h as i32 {
        for mx in 0..mm_w as i32 {
            let on_border = (mx >= vx0 && mx < vx1 && my >= vy0 && my < vy1) &&
                (mx < vx0 + border || mx >= vx1 - border || my < vy0 + border || my >= vy1 - border);
            if on_border {
                let idx = ((my as u32 * mm_w + mx as u32) * 4) as usize;
                if idx + 3 < rgba.len() {
                    rgba[idx] = 255;
                    rgba[idx + 1] = 255;
                    rgba[idx + 2] = 255;
                    rgba[idx + 3] = 255;
                }
            }
        }
    }

    (rgba, mm_w, mm_h)
}

/// Build the OSD legend sprite (RGBA).
/// Shows keybindings at the top-left corner, with extended config info when open.
fn build_osd_sprite(
    config_open: bool,
    zoom: f64,
    view_center: (f64, f64),
    mode: AppMode,
) -> (Vec<u8>, u32, u32) {
    use crate::osd::{self, Corner};
    let mut lines = vec![
        "Maggie Cross-Platform".to_string(),
        format!("Zoom: {:.1}x  Center: ({:.0},{:.0})", zoom, view_center.0, view_center.1),
        format!("Mode: {:?}", mode),
        "Scroll: zoom  |  Arrows: pan".to_string(),
        "F1/F5: OSD  |  F2: cursor  |  F3: minimap".to_string(),
        "F4: config  |  LMB: annotation  |  RMB: quit".to_string(),
        "Esc/Q: quit  |  S: screenshot  |  +/-: zoom".to_string(),
    ];
    if config_open {
        lines.push("".to_string());
        lines.push("═══ Configuration ═══".to_string());
        lines.push("Zoom: Scroll wheel or +/- keys".to_string());
        lines.push("Pan: Arrow keys or mouse drag".to_string());
        lines.push("Screenshot: S key, Esc to cancel".to_string());
        lines.push("Annotation: LMB to draw, RMB to exit".to_string());
        lines.push("Minimap: F3 to toggle".to_string());
    }
    if let Some(sprite) = osd::build_osd_sprite(&lines, Corner::TopLeft, 800, 600) {
        (sprite.buffer.data, sprite.width as u32, sprite.height as u32)
    } else {
        (vec![], 0, 0)
    }
}
