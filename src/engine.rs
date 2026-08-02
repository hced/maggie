#![allow(dead_code)]

use std::num::NonZeroU32;

use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::compositor::CompositorHandler;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::output::OutputHandler;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::ProvidesRegistryState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::registry_handlers;
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
    wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
use wayland_client::{Connection, QueueHandle};
use wayland_client::Proxy;

use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::Flags, zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::capture::CaptureManager;
use crate::config::MagnifierConfig;
use crate::render::Renderer;
use crate::render::RgbaBuffer;

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
    width: u32,
    height: u32,
    data: Vec<u8>,
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
    state: MagnifierState,
    exit: bool,
    first_configure: bool,
    last_redraw: Option<std::time::Instant>,
    width: u32,
    height: u32,
    current_output: Option<wl_output::WlOutput>,
    pointer_seen: bool,
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
                        width: width as u32,
                        height: height as u32,
                        data,
                    });
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
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
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
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.width = NonZeroU32::new(configure.new_size.0).map_or(self.width, |v| v.get());
        self.height = NonZeroU32::new(configure.new_size.1).map_or(self.height, |v| v.get());

        if self.first_configure {
            self.first_configure = false;
            self.request_screencopy(qh);
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
        let mut moved = false;
        for event in events {
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            self.pointer_seen = true;
            let position = (event.position.0 as i32, event.position.1 as i32);
            if position != self.state.pointer_position {
                self.state.pointer_position = position;
                moved = true;
            }
        }
        if moved {
            self.draw_frame(qh);
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

    fn capture_scale(&self) -> i32 {
        match self
            .current_output
            .as_ref()
            .and_then(|o| self.output_state.info(o))
        {
            Some(info) => info.scale_factor.max(1),
            None => 1,
        }
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
            "Q / Esc  quit".to_string(),
        ]
    }

    fn render_frame<F>(&mut self, _qh: &QueueHandle<Self>, fill: F)
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
        self.layer.commit();
    }

    fn draw_frame(&mut self, qh: &QueueHandle<Self>) {
        let Some(source) = self.captured.as_ref() else {
            return;
        };

        if let Some(last) = self.last_redraw
            && last.elapsed() < std::time::Duration::from_millis(16)
        {
            return;
        }
        self.last_redraw = Some(std::time::Instant::now());

        let zoom = self.state.zoom;
        let view_w = (self.width as f64 / zoom).ceil() as i32;
        let view_h = (self.height as f64 / zoom).ceil() as i32;
        let scale = self.capture_scale();
        let (center_x, center_y) = if self.pointer_seen {
            (
                self.state.pointer_position.0 * scale,
                self.state.pointer_position.1 * scale,
            )
        } else {
            (source.width as i32 / 2, source.height as i32 / 2)
        };

        let src_x = (center_x - view_w / 2).clamp(0, source.width as i32 - 1);
        let src_y = (center_y - view_h / 2).clamp(0, source.height as i32 - 1);
        let src_w = view_w.min(source.width as i32 - src_x);
        let src_h = view_h.min(source.height as i32 - src_y);

        let region = RgbaBuffer {
            width: src_w,
            height: src_h,
            data: extract_region(&source.data, source.width as i32, src_x, src_y, src_w, src_h),
        };
        let scaled = self.state.renderer.render_nearest_neighbor(&region);

        let dest_w = scaled.width.min(self.width as i32);
        let dest_h = scaled.height.min(self.height as i32);
        let off_x = ((self.width as i32 - dest_w) / 2).max(0);
        let off_y = ((self.height as i32 - dest_h) / 2).max(0);

        let show_osd = self.state.osd_visible;
        let osd_lines = self.osd_lines();
        let osd_cursor = self.state.pointer_position;

        self.render_frame(qh, |canvas, width, height, stride| {
            canvas
                .chunks_exact_mut(stride as usize)
                .take(dest_h as usize)
                .for_each(|row| row.fill(0));
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
        let mut encoder = png::Encoder::new(
            std::io::BufWriter::new(file),
            captured.width,
            captured.height,
        );
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&captured.data)?;
        tracing::info!("Screenshot saved to {}", path.display());
        Ok(())
    }
}

fn extract_region(
    data: &[u8],
    width: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h {
        let src = ((y + row) as usize * width as usize + x as usize) * 4;
        let dest = &data[src..src + (w as usize) * 4];
        out.extend_from_slice(dest);
    }
    out
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
        state,
        exit: false,
        first_configure: true,
        last_redraw: None,
        width: 1920,
        height: 1080,
        current_output: None,
        pointer_seen: false,
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
