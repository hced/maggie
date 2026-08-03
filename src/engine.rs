#![allow(dead_code)]

use std::num::NonZeroU32;

use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::compositor::CompositorHandler;
use smithay_client_toolkit::compositor::FrameCallbackData;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::output::OutputHandler;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::ProvidesRegistryState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::pointer::PointerEventKind;
use smithay_client_toolkit::seat::pointer::PointerHandler;
use smithay_client_toolkit::seat::pointer::PointerEvent;
use smithay_client_toolkit::seat::keyboard::KeyboardHandler;
use smithay_client_toolkit::seat::keyboard::KeyEvent;
use smithay_client_toolkit::seat::keyboard::Keysym;
use smithay_client_toolkit::seat::keyboard::Modifiers;
use smithay_client_toolkit::seat::keyboard::RawModifiers;
use smithay_client_toolkit::seat::Capability;
use smithay_client_toolkit::seat::SeatHandler;
use smithay_client_toolkit::seat::SeatState;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::ShmHandler;

use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_callback, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
use wayland_client::{Connection, QueueHandle};
use wayland_client::Proxy;

use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::Flags, zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::capture::CaptureManager;
use crate::config::MagnifierConfig;
use crate::gpu::GpuRenderer;
use crate::render::Renderer;
use crate::render::RgbaBuffer;

const MIN_ZOOM: f64 = 1.0;
const MAX_ZOOM: f64 = 32.0;
const WHEEL_ZOOM_STEP: f64 = 0.1;
/// Linux input event code for the right mouse button.
const BTN_RIGHT: u32 = 0x111;
const EASE_TAU: f64 = 0.04;
const EASE_EPSILON: f64 = 0.05;
/// Momentum decay time constant for the `inertia` cursor-follow style.
const INERTIA_TAU: f64 = 0.12;
/// Velocity (source px/s) below which an inertia glide is considered settled.
const INERTIA_EPS: f64 = 1.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MagnifierMode {
    CenterCursor,
    EdgePan,
    MiniatureWindow,
}

pub struct MagnifierState {
    pub config: MagnifierConfig,
    pub zoom: f64,
    pub mode: MagnifierMode,
    pub osd_visible: bool,
    pub renderer: Renderer,
    pub pointer_position: (i32, i32),
}

impl MagnifierState {
    pub fn new(config: MagnifierConfig, initial_zoom: Option<f64>) -> Self {
        let zoom = initial_zoom.unwrap_or_else(|| config.default_zoom.unwrap_or(3.0));
        let renderer = Renderer::new(zoom);

        let osd_visible = config.show_osd;

        MagnifierState {
            config,
            zoom,
            mode: MagnifierMode::CenterCursor,
            osd_visible,
            renderer,
            pointer_position: (0, 0),
        }
    }

    pub fn handle_zoom_key(&mut self, key: u8) {
        if (1..=9).contains(&key) {
            self.zoom = key as f64;
            self.renderer.update_scale_factor(self.zoom);
            tracing::info!("Zoom set to {}", self.zoom);
        }
    }

    pub fn toggle_osd(&mut self) {
        self.osd_visible = !self.osd_visible;
    }

    pub fn switch_mode(&mut self, mode: MagnifierMode) {
        self.mode = mode;
        tracing::info!("Mode switched to {:?}", mode);
    }
}

pub struct CapturedFrame {
    pub buffer: RgbaBuffer,
}

pub struct MagnifierWindow {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    compositor_state: CompositorState,
    screencast_manager: Option<ZwlrScreencopyManagerV1>,
    screencast_pool: Option<SlotPool>,
    screencast_buffer: Option<smithay_client_toolkit::shm::slot::Buffer>,
    screencast_width: Option<u32>,
    screencast_height: Option<u32>,
    screencast_stride: Option<u32>,
    y_invert: bool,
    capture_retries: u8,
    capture_manager: CaptureManager,
    captured: Option<CapturedFrame>,
    gpu: Option<GpuRenderer>,
    gpu_init_failed: bool,
    state: MagnifierState,
    exit: bool,
    first_configure: bool,
    last_anim_tick: Option<std::time::Instant>,
    view_center: Option<(f64, f64)>,
    view_velocity: (f64, f64),
    last_target: Option<(f64, f64)>,
    animating: bool,
    frame_callback: Option<wl_callback::WlCallback>,
    width: u32,
    height: u32,
    current_output: Option<wl_output::WlOutput>,
    pointer_seen: bool,
    pointer_position_f: (f64, f64),
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
}


struct ScreencastManagerData;

impl smithay_client_toolkit::dispatch2::Dispatch2<ZwlrScreencopyManagerV1, MagnifierWindow>
    for ScreencastManagerData
{
    fn event(
        &self,
        _: &mut MagnifierWindow,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<MagnifierWindow>,
    ) {}
}

struct ScreencastFrameData;

impl smithay_client_toolkit::dispatch2::Dispatch2<ZwlrScreencopyFrameV1, MagnifierWindow>
    for ScreencastFrameData
{
    fn event(
        &self,
        state: &mut MagnifierWindow,
        frame: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as Proxy>::Event,
        _: &Connection,
        qh: &QueueHandle<MagnifierWindow>,
    ) {
        use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer { format, width, height, stride } => {
                tracing::debug!(
                    "Screencopy buffer: {}x{} stride={} format={}",
                    width,
                    height,
                    stride,
                    u32::from(format)
                );
                state.capture_retries = 0;
                let format = match format {
                    wayland_client::WEnum::Value(f) => f,
                    _ => return,
                };
                if format != wl_shm::Format::Argb8888 && format != wl_shm::Format::Xrgb8888 {
                    tracing::warn!("Unsupported screencopy format: {:?}", format);
                    return;
                }
                state.screencast_width = Some(width);
                state.screencast_height = Some(height);
                state.screencast_stride = Some(stride);
                let pool_size = height as usize * stride as usize;
                let mut pool = match SlotPool::new(pool_size, &state.shm) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!("Failed to create screencopy pool: {:?}", e);
                        return;
                    }
                };
                if let Ok((buffer, _canvas)) = pool.create_buffer(
                    width as i32,
                    height as i32,
                    stride as i32,
                    format,
                ) {
                    frame.copy(buffer.wl_buffer());
                    state.screencast_pool = Some(pool);
                    state.screencast_buffer = Some(buffer);
                    tracing::debug!("Screencopy buffer created and copy requested");
                }
            }
            Event::Flags { flags } => {
                if let wayland_client::WEnum::Value(flags) = flags {
                    state.y_invert = flags.contains(Flags::YInvert);
                    tracing::debug!("Screencopy flags: {:?}", flags);
                }
            }
            Event::Ready { .. } => {
                tracing::debug!("Screencopy frame ready");
                state.capture_retries = 0;
                if let (Some(mut pool), Some(buffer)) = (
                    state.screencast_pool.take(),
                    state.screencast_buffer.take(),
                ) && let Some(canvas) = buffer.canvas(&mut pool)
                {
                    let stride = state.screencast_stride.unwrap_or(buffer.stride() as u32) as usize;
                    let width = state
                        .screencast_width
                        .unwrap_or(buffer.stride() as u32 / 4) as usize;
                    let height = state.screencast_height.unwrap_or(buffer.height() as u32) as usize;
                    let mut data = Vec::with_capacity(width * height * 4);
                    if state.y_invert {
                        for row in canvas.chunks_exact(stride).rev().take(height) {
                            convert_row(row, width, &mut data);
                        }
                    } else {
                        for row in canvas.chunks_exact(stride).take(height) {
                            convert_row(row, width, &mut data);
                        }
                    }
                    state.captured = Some(CapturedFrame {
                        buffer: RgbaBuffer {
                            width: width as i32,
                            height: height as i32,
                            data,
                        },
                    });
                    if let Some(gpu) = &mut state.gpu {
                        gpu.upload_frame(&state.captured.as_ref().unwrap().buffer);
                    }
                    tracing::info!("Captured {}x{} frame", width, height);
                }
                state.draw_frame(qh);
            }
            Event::Failed => {
                if state.capture_retries < 3 {
                    state.capture_retries += 1;
                    tracing::warn!(
                        "Screencopy capture failed, retrying ({}/{})",
                        state.capture_retries,
                        3
                    );
                    state.screencast_pool = None;
                    state.screencast_buffer = None;
                    state.request_screencopy(qh);
                } else {
                    tracing::error!(
                        "Screencopy capture failed after {} retries, showing black overlay",
                        state.capture_retries
                    );
                    state.screencast_pool = None;
                    state.screencast_buffer = None;
                    state.draw_black_overlay(qh);
                }
            }
            _ => {
                tracing::debug!("Screencopy frame event: {:?}", event);
            }
        }
    }
}

fn convert_row(row: &[u8], width: usize, out: &mut Vec<u8>) {
    for px in row.chunks_exact(4).take(width) {
        out.push(px[2]);
        out.push(px[1]);
        out.push(px[0]);
        out.push(255);
    }
}

delegate_registry!(MagnifierWindow);

impl ProvidesRegistryState for MagnifierWindow {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

impl ShmHandler for MagnifierWindow {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl OutputHandler for MagnifierWindow {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl CompositorHandler for MagnifierWindow {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self.animating {
            self.draw_frame(qh);
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        self.current_output = Some(output.clone());
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
        self.current_output = None;
    }
}

impl LayerShellHandler for MagnifierWindow {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.width = NonZeroU32::new(configure.new_size.0).map_or(self.width, |v| v.get());
        self.height = NonZeroU32::new(configure.new_size.1).map_or(self.height, |v| v.get());

        if let Some((w, h)) = self.output_logical_size()
            && (w != self.width || h != self.height)
        {
            self.width = w;
            self.height = h;
            self.layer.set_size(w, h);
        }

        self.ensure_gpu(conn);

        if let Some(gpu) = &mut self.gpu {
            gpu.resize(self.width as i32, self.height as i32);
        }

        self.pool
            .resize(self.width as usize * self.height as usize * 4)
            .map_err(|e| tracing::warn!("Failed to resize shm pool: {e}"))
            .ok();

        if self.first_configure {
            self.first_configure = false;
            self.request_screencopy(qh);
        }

        if self.captured.is_some() {
            self.draw_frame(qh);
        }

        self.layer.commit();
    }
}

impl PointerHandler for MagnifierWindow {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            self.pointer_seen = true;
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    let position = event.position;
                    self.pointer_position_f = position;
                    self.state.pointer_position = (position.0 as i32, position.1 as i32);
                    self.draw_frame(qh);
                }
                PointerEventKind::Motion { .. } => {
                    let position = event.position;
                    if position != self.pointer_position_f {
                        self.pointer_position_f = position;
                        self.state.pointer_position = (position.0 as i32, position.1 as i32);
                        self.animating = true;
                        self.draw_frame(qh);
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    // Right mouse button quits, same as Q.
                    if button == BTN_RIGHT {
                        self.exit = true;
                    }
                }
                PointerEventKind::Axis { vertical, .. } => {
                    let mut steps = if vertical.value120 != 0 {
                        vertical.value120 as f64 / 120.0
                    } else {
                        vertical.discrete as f64
                    };
                    if steps != 0.0 {
                        if !self.state.config.invert_scroll_zoom {
                            steps = -steps;
                        }
                        let new_zoom = match self.state.config.scroll_zoom_mode {
                            crate::config::ScrollZoomMode::Levels => {
                                let whole =
                                    (self.state.zoom - self.state.zoom.floor()).abs() < 1e-9;
                                if steps > 0.0 {
                                    if whole {
                                        self.state.zoom + 1.0
                                    } else {
                                        self.state.zoom.ceil()
                                    }
                                } else if whole {
                                    self.state.zoom - 1.0
                                } else {
                                    self.state.zoom.floor()
                                }
                                .clamp(1.0, 9.0)
                            }
                            crate::config::ScrollZoomMode::Factor => (self.state.zoom
                                * (1.0 + steps * WHEEL_ZOOM_STEP))
                                .clamp(MIN_ZOOM, MAX_ZOOM),
                        };
                        if (new_zoom - self.state.zoom).abs() > 1e-9 {
                            self.state.zoom = new_zoom;
                            self.state.renderer.update_scale_factor(new_zoom);
                            self.view_center = None;
                            self.view_velocity = (0.0, 0.0);
                            self.last_target = None;
                            self.animating = false;
                            tracing::info!("Wheel zoom set to {}", self.state.zoom);
                            self.draw_frame(qh);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl KeyboardHandler for MagnifierWindow {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        tracing::debug!("Key press: {:?}", event);

        let keysym_str = keysym_to_string(event.keysym);

        if let Some(zoom_level) = match keysym_str.as_str() {
            "1" => Some(1),
            "2" => Some(2),
            "3" => Some(3),
            "4" => Some(4),
            "5" => Some(5),
            "6" => Some(6),
            "7" => Some(7),
            "8" => Some(8),
            "9" => Some(9),
            _ => None,
        } {
            self.state.handle_zoom_key(zoom_level);
            self.view_center = None;
            self.view_velocity = (0.0, 0.0);
            self.last_target = None;
            self.animating = false;
            self.draw_frame(qh);
        }

        let config_key = &self.state.config.keybindings;

        if keysym_str == config_key.toggle_osd {
            self.state.toggle_osd();
            self.draw_frame(qh);
            tracing::info!("OSD toggled: {}", self.state.osd_visible);
        } else if keysym_str == config_key.screenshot_manual {
            tracing::info!("Manual screenshot mode - not yet implemented");
        } else if keysym_str == config_key.screenshot_window {
            tracing::info!("Window screenshot mode - not yet implemented");
        } else if keysym_str == config_key.screenshot_fullscreen {
            if let Err(e) = self.save_screenshot() {
                tracing::error!("Failed to save screenshot: {:#}", e);
            }
        } else if keysym_str == config_key.config_window {
            tracing::info!("Configuration window - not yet implemented");
        } else if keysym_str == config_key.anti_aliasing {
            tracing::info!("Anti-aliasing toggle - not yet implemented");
        } else if keysym_str == config_key.mode_center_cursor {
            self.state.switch_mode(MagnifierMode::CenterCursor);
        } else if keysym_str == config_key.mode_edge_pan {
            self.state.switch_mode(MagnifierMode::EdgePan);
        } else if keysym_str == config_key.mode_miniature {
            self.state.switch_mode(MagnifierMode::MiniatureWindow);
        }

        if event.keysym == Keysym::Escape || event.keysym == Keysym::q {
            self.exit = true;
        }
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        tracing::debug!("Key repeat: {:?}", event);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
}

impl SeatHandler for MagnifierWindow {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            tracing::debug!("Setting keyboard capability");
            let keyboard = self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .expect("Failed to create keyboard");
            self.keyboard = Some(keyboard);
        }

        if capability == Capability::Pointer && self.pointer.is_none() {
            tracing::debug!("Setting pointer capability");
            let pointer = self
                .seat_state
                .get_pointer(qh, &seat)
                .expect("Failed to create pointer");
            self.pointer = Some(pointer);
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_some() {
            tracing::debug!("Unsetting keyboard capability");
            self.keyboard.take().unwrap().release();
        }

        if capability == Capability::Pointer && self.pointer.is_some() {
            tracing::debug!("Unsetting pointer capability");
            self.pointer.take().unwrap().release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

fn keysym_to_string(keysym: Keysym) -> String {
    match keysym {
        Keysym::k => "k".to_string(),
        Keysym::s => "s".to_string(),
        Keysym::w => "w".to_string(),
        Keysym::f => "f".to_string(),
        Keysym::a => "a".to_string(),
        Keysym::_1 => "1".to_string(),
        Keysym::_2 => "2".to_string(),
        Keysym::_3 => "3".to_string(),
        Keysym::_4 => "4".to_string(),
        Keysym::_5 => "5".to_string(),
        Keysym::_6 => "6".to_string(),
        Keysym::_7 => "7".to_string(),
        Keysym::_8 => "8".to_string(),
        Keysym::_9 => "9".to_string(),
        Keysym::q => "q".to_string(),
        Keysym::Escape => "Escape".to_string(),
        _ => format!("{}", u32::from(keysym)),
    }
}

smithay_client_toolkit::delegate_dispatch2!(MagnifierWindow);

impl MagnifierWindow {
    fn request_screencopy(&mut self, qh: &QueueHandle<Self>) {
        let Some(manager) = &self.screencast_manager else {
            return;
        };
        let output = self
            .current_output
            .clone()
            .or_else(|| self.output_state.outputs().next());
        let Some(output) = output else {
            tracing::error!("No output available for screencopy");
            return;
        };

        let _frame = manager.capture_output(true as i32, &output, qh, ScreencastFrameData);
    }

    fn osd_lines(&self) -> Vec<String> {
        let config_key = &self.state.config.keybindings;
        vec![
            format!("maggie  zoom {}x", self.state.zoom),
            "1-9  zoom level".to_string(),
            format!("{}  toggle OSD", config_key.toggle_osd),
            format!("{}  screenshot fullscreen", config_key.screenshot_fullscreen),
            format!("{}  manual selection", config_key.screenshot_manual),
            format!("{}  window selection", config_key.screenshot_window),
            format!("{}  config window", config_key.config_window),
            "Q / Esc / RMB  quit".to_string(),
        ]
    }

    fn request_frame_callback(&mut self, qh: &QueueHandle<Self>) {
        let surface = self.layer.wl_surface().clone();
        let callback = surface.clone().frame(qh, FrameCallbackData(surface));
        self.frame_callback = Some(callback);
    }

    /// Initialize the GPU renderer at first configure, once the real output
    /// size is known. The EGL window is created at its final size so the very
    /// first presented buffer already covers the whole output; resizing a
    /// wl_egl_window before the first swap has no effect in practice.
    fn ensure_gpu(&mut self, conn: &Connection) {
        if self.gpu.is_some() || self.gpu_init_failed {
            return;
        }
        match GpuRenderer::init(
            conn.backend().display_ptr() as *mut std::os::raw::c_void,
            self.layer.wl_surface(),
            self.width as i32,
            self.height as i32,
        ) {
            Ok(mut gpu) => {
                self.layer
                    .wl_surface()
                    .set_buffer_scale(crate::gpu::RENDER_SCALE);
                if let Some(captured) = &self.captured {
                    gpu.upload_frame(&captured.buffer);
                }
                self.gpu = Some(gpu);
            }
            Err(e) => {
                self.gpu_init_failed = true;
                tracing::warn!("GPU rendering unavailable, falling back to CPU path: {:#}", e);
            }
        }
    }

    /// Logical size of the output the surface is on, falling back to the first
    /// known output. Used to size the viewport over the entire physical screen
    /// (shell bars included) regardless of what the compositor first proposes.
    fn output_logical_size(&self) -> Option<(u32, u32)> {
        if let Some(output) = &self.current_output
            && let Some((w, h)) = self.output_state.info(output).and_then(|i| i.logical_size)
        {
            return Some((w.max(0) as u32, h.max(0) as u32));
        }
        for output in self.output_state.outputs() {
            if let Some((w, h)) = self.output_state.info(&output).and_then(|i| i.logical_size) {
                return Some((w.max(0) as u32, h.max(0) as u32));
            }
        }
        None
    }

    fn render_frame<F>(&mut self, qh: &QueueHandle<Self>, fill: F)
    where
        F: FnOnce(&mut [u8], i32, i32, i32),
    {
        let width = self.width;
        let height = self.height;
        let stride = width as i32 * 4;

        let (buffer, canvas) = match self.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok(result) => result,
            Err(e) => {
                tracing::error!("Failed to create buffer: {:?}", e);
                return;
            }
        };

        fill(canvas, width as i32, height as i32, stride);

        let surface = self.layer.wl_surface();
        buffer.attach_to(surface).expect("buffer attach");
        surface.damage(0, 0, width as i32, height as i32);
        if self.animating {
            self.request_frame_callback(qh);
        }
        self.layer.commit();
    }

    fn draw_frame(&mut self, qh: &QueueHandle<Self>) {
        let Some(captured) = self.captured.as_ref() else {
            return;
        };
        let source_w = captured.buffer.width;
        let source_h = captured.buffer.height;

        let zoom = self.state.zoom;
        let scale_x = source_w as f64 / self.width as f64;
        let scale_y = source_h as f64 / self.height as f64;
        let target = if self.pointer_seen {
            (
                self.pointer_position_f.0 * scale_x,
                self.pointer_position_f.1 * scale_y,
            )
        } else {
            (source_w as f64 / 2.0, source_h as f64 / 2.0)
        };

        let (center_x, center_y, animating) = match self.view_center {
            Some((cx, cy)) => {
                let dt = self.last_anim_tick.map_or(0.016, |t| t.elapsed().as_secs_f64());
                self.last_anim_tick = Some(std::time::Instant::now());
                match self.state.config.cursor_follow {
                    crate::config::CursorFollow::Snap => {
                        self.view_center = Some(target);
                        self.animating = false;
                        (target.0, target.1, false)
                    }
                    crate::config::CursorFollow::Ease => {
                        let dist = ((cx - target.0).powi(2) + (cy - target.1).powi(2)).sqrt();
                        if dist < EASE_EPSILON {
                            self.view_center = Some(target);
                            self.animating = false;
                            (target.0, target.1, false)
                        } else {
                            let k = 1.0 - (-dt / EASE_TAU).exp();
                            let nx = cx + (target.0 - cx) * k;
                            let ny = cy + (target.1 - cy) * k;
                            self.view_center = Some((nx, ny));
                            (nx, ny, true)
                        }
                    }
                    crate::config::CursorFollow::Inertia => {
                        let cursor_moved = self.last_target != Some(target);
                        let (mut vx, mut vy) = self.view_velocity;
                        if cursor_moved {
                            vx += (target.0 - cx) * (dt / EASE_TAU);
                            vy += (target.1 - cy) * (dt / EASE_TAU);
                        } else {
                            let decay = (-dt / INERTIA_TAU).exp();
                            vx *= decay;
                            vy *= decay;
                        }
                        let nx = cx + vx * dt;
                        let ny = cy + vy * dt;
                        self.view_velocity = (vx, vy);
                        self.view_center = Some((nx, ny));
                        let settled = !cursor_moved && vx.hypot(vy) < INERTIA_EPS;
                        if settled {
                            self.animating = false;
                            self.view_center = Some(target);
                            (target.0, target.1, false)
                        } else {
                            (nx, ny, true)
                        }
                    }
                }
            }
            None => {
                self.view_center = Some(target);
                (target.0, target.1, false)
            }
        };
        self.last_target = Some(target);
        self.animating = animating;

        let view_w = self.width as f64 / zoom;
        let view_h = self.height as f64 / zoom;
        let src_x = (center_x - view_w / 2.0).clamp(0.0, (source_w as f64 - view_w).max(0.0));
        let src_y = (center_y - view_h / 2.0).clamp(0.0, (source_h as f64 - view_h).max(0.0));

        let lines = self.osd_lines();

        if let Some(gpu) = &mut self.gpu {
            let osd = if self.state.osd_visible {
                crate::osd::build_osd_sprite(
                    &lines,
                    (
                        self.state.pointer_position.0 * crate::gpu::RENDER_SCALE,
                        self.state.pointer_position.1 * crate::gpu::RENDER_SCALE,
                    ),
                    self.width as i32 * crate::gpu::RENDER_SCALE,
                    self.height as i32 * crate::gpu::RENDER_SCALE,
                )
            } else {
                None
            };
            let uv = (
                src_x / source_w as f64,
                src_y / source_h as f64,
                view_w.min(source_w as f64) / source_w as f64,
                view_h.min(source_h as f64) / source_h as f64,
            );
            gpu.draw(Some(uv), osd.as_ref());
            if self.animating {
                self.request_frame_callback(qh);
            }
            return;
        }

        let dest_w = (view_w.min(source_w as f64) * zoom).round() as i32;
        let dest_h = (view_h.min(source_h as f64) * zoom).round() as i32;
        let off_x = ((self.width as i32 - dest_w) / 2).max(0);
        let off_y = ((self.height as i32 - dest_h) / 2).max(0);

        let scaled = self.state.renderer.render_bilinear(
            &captured.buffer,
            (src_x, src_y),
            dest_w,
            dest_h,
        );

        let show_osd = self.state.osd_visible;
        let osd_lines = self.osd_lines();
        let osd_cursor = self.state.pointer_position;

        self.render_frame(qh, |canvas, width, height, stride| {
            canvas.fill(0);
            for y in 0..dest_h {
                let src_row = &scaled.data[(y as usize) * (scaled.width as usize) * 4..];
                let dest_row = &mut canvas
                    [((y + off_y) as usize) * (stride as usize) + (off_x as usize) * 4..];
                dest_row[..(dest_w as usize) * 4]
                    .copy_from_slice(&src_row[..(dest_w as usize) * 4]);
            }
            if show_osd {
                crate::osd::draw_osd(canvas, width, height, &osd_lines, osd_cursor);
            }
        });
    }

    fn draw_black_overlay(&mut self, qh: &QueueHandle<Self>) {
        if let Some(gpu) = &mut self.gpu {
            gpu.draw(None, None);
            return;
        }
        self.render_frame(qh, |canvas, _width, _height, _stride| {
            canvas.chunks_exact_mut(4).for_each(|chunk| {
                chunk[0] = 0;
                chunk[1] = 0;
                chunk[2] = 0;
                chunk[3] = 255;
            });
        });
    }

    fn save_screenshot(&mut self) -> anyhow::Result<()> {
        let Some(captured) = &self.captured else {
            tracing::warn!("No captured frame yet");
            return Ok(());
        };
        let path = self.capture_manager.generate_screenshot_path()?;
        let file = std::fs::File::create(&path)?;
        let buffer = &captured.buffer;
        let mut encoder = png::Encoder::new(
            std::io::BufWriter::new(file),
            buffer.width as u32,
            buffer.height as u32,
        );
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&buffer.data)?;
        tracing::info!("Screenshot saved to {}", path.display());
        Ok(())
    }
}

pub fn run(initial_zoom: Option<f64>) -> anyhow::Result<()> {
    let config = crate::config::load_config()?;
    tracing::debug!("Config loaded");

    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("Cannot connect to Wayland display: {}", e))?;
    tracing::debug!("Connected to Wayland display");

    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("Compositor not available: {:?}", e))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("Layer shell not available: {:?}", e))?;
    let shm = Shm::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("SHM not available: {:?}", e))?;
    let output_state = OutputState::new(&globals, &qh);

    let screencast_manager = globals
        .bind::<ZwlrScreencopyManagerV1, _, _>(&qh, 1..=3, ScreencastManagerData)
        .ok();

    let mut event_queue = event_queue;

    let surface = compositor.create_surface(&qh);

    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("maggie"), None);
    layer.set_anchor(Anchor::all());
    // A negative exclusive zone marks the surface as "dont care": the
    // compositor hands it the full output geometry instead of shrinking it
    // around the reserved zones of lower layer-shell surfaces (bars, docks).
    // Without this, niri sizes the overlay to the screen minus the top bar,
    // leaving the real bar covering the magnifier's top strip.
    layer.set_exclusive_zone(-1);
    // On-demand keyboard focus (not exclusive): the compositor's own global
    // keybindings keep working while the magnifier stays keyboard-receivable.
    layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    layer.set_size(0, 0);
    layer.commit();

    let pool = SlotPool::new(1920 * 1080 * 4, &shm)?;
    let capture_manager = CaptureManager::new(
        config.screenshot_path.clone(),
        config.screenshot_filename_pattern.clone(),
    );
    let state = MagnifierState::new(config, initial_zoom);

    let mut window = MagnifierWindow {
        registry_state: RegistryState::new(&globals),
        output_state,
        seat_state: SeatState::new(&globals, &qh),
        shm,
        pool,
        layer,
        compositor_state: compositor,
        screencast_manager,
        screencast_pool: None,
        screencast_buffer: None,
        screencast_width: None,
        screencast_height: None,
        screencast_stride: None,
        y_invert: false,
        capture_retries: 0,
        capture_manager,
        captured: None,
        gpu: None,
        gpu_init_failed: false,
        state,
        exit: false,
        first_configure: true,
        last_anim_tick: None,
        view_center: None,
        view_velocity: (0.0, 0.0),
        last_target: None,
        animating: false,
        frame_callback: None,
        width: 1920,
        height: 1080,
        current_output: None,
        pointer_seen: false,
        pointer_position_f: (0.0, 0.0),
        keyboard: None,
        pointer: None,
    };

    tracing::info!(
        "Maggie magnifier started with zoom {} on layer surface",
        window.state.zoom
    );

    if window.screencast_manager.is_some() {
        tracing::info!("Screencopy manager available");
    }

    loop {
        event_queue.blocking_dispatch(&mut window)?;

        if window.exit {
            tracing::info!("Exiting");
            break;
        }
    }

    Ok(())
}
