#![allow(dead_code)]

use std::num::NonZeroU32;

use smithay_client_toolkit::compositor::CompositorHandler;
use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::compositor::FrameCallbackData;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::output::OutputHandler;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::ProvidesRegistryState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::Capability;
use smithay_client_toolkit::seat::SeatHandler;
use smithay_client_toolkit::seat::SeatState;
use smithay_client_toolkit::seat::keyboard::KeyEvent;
use smithay_client_toolkit::seat::keyboard::KeyboardHandler;
use smithay_client_toolkit::seat::keyboard::Keysym;
use smithay_client_toolkit::seat::keyboard::Modifiers;
use smithay_client_toolkit::seat::keyboard::RawModifiers;
use smithay_client_toolkit::seat::pointer::PointerEvent;
use smithay_client_toolkit::seat::pointer::PointerEventKind;
use smithay_client_toolkit::seat::pointer::PointerHandler;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::ShmHandler;
use smithay_client_toolkit::shm::slot::SlotPool;

use wayland_client::Proxy;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_callback, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
use wayland_client::{Connection, QueueHandle};

use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::Flags, zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::capture::CaptureManager;
use crate::config::MagnifierConfig;
use crate::config_window::ConfigWindow;
use crate::config_window::UiResult;
use crate::gpu::GpuRenderer;
use crate::render::Renderer;
use crate::render::RgbaBuffer;

const MIN_ZOOM: f64 = 1.0;
const MAX_ZOOM: f64 = 32.0;
const WHEEL_ZOOM_STEP: f64 = 0.1;
/// Time constant (s) for gently correcting the view-vs-hand offset after
/// hold-to-zoom release. The correction is driven by real pointer motion
/// only (never self-animated); when the pointer moves *toward* the hand
/// content the correction is boosted to at least the hand's own travel, so
/// a push to a far wall restores the reach en route (see
/// [`offset_correction_step`]).
const OFFSET_CORRECT_TAU: f64 = 0.8;
/// Per-event cap on the correction time step, so a long pause before the
/// first motion after release cannot dump the whole offset in one jump.
const OFFSET_CORRECT_DT_CAP: f64 = 0.1;

/// The fraction of the remaining view-vs-hand offset corrected by one
/// pointer-motion event `dt` seconds after the previous one. Capped so a
/// long pause never dumps the whole offset in a single step.
fn offset_correction_factor(dt: f64) -> f64 {
    (1.0 - (-dt.min(OFFSET_CORRECT_DT_CAP) / OFFSET_CORRECT_TAU).exp()).min(1.0)
}

/// Clamp a view-center coordinate (capture px) to the capture bounds. The
/// magnified cursor sits at the viewport center, which *is* the view center:
/// keeping the center inside the capture guarantees the cursor never enters
/// the black beyond-capture fill, and that every captured pixel stays
/// reachable — pushing against a screen edge always lands the view exactly
/// on the capture edge.
fn clamp_to_capture_bounds(pos: (f64, f64), bounds: (f64, f64)) -> (f64, f64) {
    (pos.0.clamp(0.0, bounds.0), pos.1.clamp(0.0, bounds.1))
}

/// One correction step for the residual view-vs-hand offset (view minus
/// hand content; hold-to-zoom, a pointer re-enter or a launch quirk can
/// leave one). The correction is driven by real pointer motion only
/// (never self-animated) and is bounded so a single event after a long
/// pause can never lurch the view.
///
/// **Reach-restoring boost:** a residual always blocks the wall in the
/// direction it points away from — pushing there stops short by the
/// residual until it heals, which is why walls were unreachable after
/// hold-to-zoom. So when the pointer moves *toward* the hand content (the
/// reach-blocking direction, `offset × travel < 0` per axis), the view
/// catches up at least as fast as the hand travels (capped at 2× the
/// hand's speed): the residual is erased during the push, and the far wall
/// is reached exactly, at any speed. Moving away heals gently (time-based)
/// and never fights the user. Returns the remaining offset.
fn offset_correction_step(
    offset: (f64, f64),
    dt: f64,
    travel: (f64, f64),
    scale: (f64, f64),
) -> (f64, f64) {
    let f = offset_correction_factor(dt);
    let heal = |o: f64, t: f64, s: f64| {
        // Time-based decay, bounded by the hand's own travel (×2) so a
        // single event after a pause can never lurch the view.
        let lim = t.abs() * s * 2.0;
        let mut corr = (o * f).clamp(-lim, lim);
        if o * t < 0.0 {
            // Pushing toward the hand content: catch up at least as fast as
            // the hand travels (still within the 2× cap), so the far wall
            // stays reachable en route.
            let catch = t.abs() * s;
            corr = if corr >= 0.0 {
                corr.max(catch)
            } else {
                corr.min(-catch)
            };
        }
        // Never overshoot the hand content.
        corr.clamp(o.min(0.0), o.max(0.0))
    };
    (
        offset.0 - heal(offset.0, travel.0, scale.0),
        offset.1 - heal(offset.1, travel.1, scale.1),
    )
}

/// Apply one correction step toward the hand content (see the Motion
/// handler): the corrected view position is `target` (`hand content +
/// remaining offset`), but an axis whose view is already pinned against a
/// capture edge is left untouched — the wall wins, so pushing against a
/// screen edge always lands the view *exactly* on the edge, and gliding
/// along an edge keeps the view on it, no matter how fast the pointer
/// moves or how large the residual offset is. `view` must already be
/// clamped to `bounds`; the result is re-clamped.
fn correct_toward_hand(view: (f64, f64), target: (f64, f64), bounds: (f64, f64)) -> (f64, f64) {
    let pinned_x = view.0 <= 0.0 || view.0 >= bounds.0;
    let pinned_y = view.1 <= 0.0 || view.1 >= bounds.1;
    let fx = if pinned_x { view.0 } else { target.0 };
    let fy = if pinned_y { view.1 } else { target.1 };
    clamp_to_capture_bounds((fx, fy), bounds)
}

/// The edge-reach margin scales with the pointer's per-event travel (see
/// [`EdgeReach`]): when the pointer is moved fast, its delivered position
/// can stop short of the physical edge (the last sample before the hand
/// stops), so the view settles short of the wall by an amount proportional
/// to speed — which is why slow pushes always reached the exact wall while
/// fast flicks stopped “arbitrarily” short. The margin is `|delta| ×
/// REACH_DELTA_FACTOR + REACH_FLOOR_LOGICAL`: small when moving slowly (no
/// magnetic wall — parking stays precise) and large exactly when the
/// delivery gap is large (the wall is always reachable, at any speed).
/// Capped so a single absurd event can never magnetize a large area.
const REACH_DELTA_FACTOR: f64 = 1.5;
/// Floor (logical px): even a sub-pixel crawl to the edge lands the view
/// exactly on the wall, so the exact border is always reachable.
const REACH_FLOOR_LOGICAL: f64 = 8.0;
/// Cap (logical px) on the reach margin: bounds the magnetic zone even for
/// a single huge motion event.
const REACH_MAX_LOGICAL: f64 = 120.0;
/// Extra view-side slack (logical px) beyond the reach margin: the view may
/// still sit slightly short of the wall from a still-healing residual; the
/// slack lets the reach close that too without teleporting across a large
/// residual.
const REACH_VIEW_SLACK_LOGICAL: f64 = 8.0;

/// Per-axis geometry for the hand-edge reach: the surface (pointer
/// coordinate range) size in logical px, the capture bound in px, and the
/// capture-per-logical-pixel scale.
#[derive(Clone, Copy)]
struct EdgeReach {
    surface: f64,
    bounds: f64,
    scale: f64,
}

impl EdgeReach {
    fn new(surface: f64, bounds: f64, scale: f64) -> Self {
        Self {
            surface,
            bounds,
            scale,
        }
    }

    /// Reach the exact capture edge when the user pushes into it. The view
    /// pans with the hand's *movement*, so its reach is bounded by the
    /// hand's delivered travel — and when the pointer is moved fast, the
    /// last delivered position can stop short of the surface edge, leaving
    /// the view short of the wall. This closes that gap: the reach margin
    /// scales with this event's own travel (`|delta|`), because the delivery
    /// gap is at most one event's travel. So a slow push near the edge keeps
    /// a small margin (no magnetic wall — you can park anywhere), while a
    /// fast flick toward the edge gets a margin large enough to bridge the
    /// gap and land the view **exactly** on the wall, at any speed. The
    /// view must already be within the (scaled) margin of the wall so it
    /// never teleports across a large still-healing residual. Pushing away,
    /// gliding (`delta_logical == 0`), or being away from the edge never
    /// triggers it, and the result never leaves the capture. `view` is in
    /// capture px; `pointer` is in logical px.
    fn apply(self, view: f64, delta_logical: f64, pointer: f64) -> f64 {
        let margin =
            (delta_logical.abs() * REACH_DELTA_FACTOR + REACH_FLOOR_LOGICAL).min(REACH_MAX_LOGICAL);
        let view_margin = (margin + REACH_VIEW_SLACK_LOGICAL) * self.scale;
        if delta_logical > 0.0
            && pointer >= self.surface - margin
            && view >= self.bounds - view_margin
        {
            self.bounds
        } else if delta_logical < 0.0 && pointer <= margin && view <= view_margin {
            0.0
        } else {
            view
        }
    }
}
/// Linux input event code for the right mouse button.
const BTN_RIGHT: u32 = 0x111;
/// Linux input event code for the middle mouse button (resets the zoom).
const BTN_MIDDLE: u32 = 0x112;

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
    /// Whether the magnified cursor sprite is drawn inside the viewport
    /// (toggled with the `toggle_cursor` key; independent of the hardware
    /// cursor, which is always hidden while the pointer is over the viewport).
    pub cursor_visible: bool,
    pub renderer: Renderer,
    pub pointer_position: (i32, i32),
}

impl MagnifierState {
    pub fn new(config: MagnifierConfig, initial_zoom: Option<f64>) -> Self {
        let max_zoom = config.max_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let zoom = initial_zoom
            .unwrap_or_else(|| config.default_zoom.unwrap_or(3.0))
            .clamp(MIN_ZOOM, max_zoom);
        let renderer = Renderer::new(zoom);

        let osd_visible = config.show_osd;

        MagnifierState {
            config,
            zoom,
            mode: MagnifierMode::CenterCursor,
            osd_visible,
            cursor_visible: true,
            renderer,
            pointer_position: (0, 0),
        }
    }

    /// The zoom level the key `1`–`9` selects: each key is a fraction of the
    /// configured max zoom, so key 9 always means `max_zoom`. Clamped to at
    /// least 1× (with `max_zoom < 9` the lower keys would otherwise go
    /// sub-1×, e.g. 0.44× at `max_zoom = 4`).
    fn zoom_for_level(&self, key: u8) -> f64 {
        let max_zoom = self.config.max_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        (max_zoom * (key as f64) / 9.0).clamp(MIN_ZOOM, max_zoom)
    }

    pub fn handle_zoom_key(&mut self, key: u8) {
        if (1..=9).contains(&key) {
            self.zoom = self.zoom_for_level(key);
            self.renderer.update_scale_factor(self.zoom);
            tracing::info!("Zoom set to {}", self.zoom);
        }
    }

    /// Reset the zoom back to the configured default (used by the middle mouse
    /// button and the `reset_zoom` keybinding).
    pub fn reset_zoom(&mut self) {
        let max_zoom = self.config.max_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let default_zoom = self
            .config
            .default_zoom
            .unwrap_or(3.0)
            .clamp(MIN_ZOOM, max_zoom);
        self.zoom = default_zoom;
        self.renderer.update_scale_factor(default_zoom);
        tracing::info!("Zoom reset to {}", self.zoom);
    }

    pub fn toggle_osd(&mut self) {
        self.osd_visible = !self.osd_visible;
    }

    pub fn toggle_cursor(&mut self) {
        self.cursor_visible = !self.cursor_visible;
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
    /// The view center (capture px) was initialized on the launch pointer
    /// position once; later pointer enters never re-center (which would jump).
    launch_centered: bool,
    /// The launch pointer position (logical px) recorded on the first enter,
    /// applied with the real capture scale once the capture exists (the
    /// enter can arrive before the screencopy completes).
    launch_position: Option<(f64, f64)>,
    view_center: Option<(f64, f64)>,
    /// Wall-clock time of the last pointer-motion event, driving the offset
    /// correction's time constant (only real motion corrects — never
    /// self-animated).
    last_motion_at: Option<std::time::Instant>,
    animating: bool,
    frame_callback: Option<wl_callback::WlCallback>,
    width: u32,
    height: u32,
    current_output: Option<wl_output::WlOutput>,
    pointer_seen: bool,
    pointer_position_f: (f64, f64),
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    magnified_cursor: Option<crate::cursor::MagnifiedCursor>,
    blank_cursor_surface: Option<wl_surface::WlSurface>,
    cursor_pool: Option<SlotPool>,
    /// A cursor surface showing the real system cursor (from the loaded theme)
    /// at its native size, used while the Configuration window is open so the
    /// UI is operated with a visible pointer. The hotspot is stored alongside.
    config_cursor_surface: Option<wl_surface::WlSurface>,
    config_cursor_pool: Option<SlotPool>,
    config_cursor_hotspot: Option<(i32, i32)>,
    /// The egui Configuration window; present while it is open. While it is
    /// open the whole surface shows the UI and pointer/keyboard input is
    /// forwarded to it instead of driving the magnifier.
    config_window: Option<ConfigWindow>,
    /// Latest wl_pointer enter serial, used to reset the cursor to the default
    /// (Configuration window open) or re-hide it with the blank surface
    /// (closed) when the pointer is over the surface.
    last_pointer_serial: Option<u32>,
    /// Hold-to-zoom: while the configured modifier is held, vertical pointer
    /// motion changes the zoom continuously instead of in steps.
    hold_to_zoom_active: bool,
    /// Pointer Y (logical) of the previous motion event while hold-to-zoom is
    /// active; the per-event delta drives the zoom change.
    hold_zoom_last_y: f64,
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
    ) {
    }
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
            Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
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
                if let Ok((buffer, _canvas)) =
                    pool.create_buffer(width as i32, height as i32, stride as i32, format)
                {
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
                if let (Some(mut pool), Some(buffer)) =
                    (state.screencast_pool.take(), state.screencast_buffer.take())
                    && let Some(canvas) = buffer.canvas(&mut pool)
                {
                    let stride = state.screencast_stride.unwrap_or(buffer.stride() as u32) as usize;
                    let width =
                        state.screencast_width.unwrap_or(buffer.stride() as u32 / 4) as usize;
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
                    // If the pointer already entered (recording its launch
                    // position) before this first capture completed, apply
                    // the launch centering now with the real scale.
                    state.apply_launch_centering();
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
                } else if state.captured.is_none() {
                    tracing::error!(
                        "Screencopy capture failed after {} retries, showing black overlay",
                        state.capture_retries
                    );
                    state.screencast_pool = None;
                    state.screencast_buffer = None;
                    state.draw_black_overlay(qh);
                } else {
                    // A failed clean re-capture must never replace an already
                    // good frozen frame (e.g. with the black overlay).
                    tracing::error!(
                        "Screencopy re-capture failed after {} retries; keeping existing frame",
                        state.capture_retries
                    );
                    state.screencast_pool = None;
                    state.screencast_buffer = None;
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

    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        // The Configuration window repaints continuously while it is open.
        if self.animating || self.config_window.is_some() {
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

        if let Some(cw) = &mut self.config_window {
            cw.resize(self.width as i32, self.height as i32);
        }

        self.pool
            .resize(self.width as usize * self.height as usize * 4)
            .map_err(|e| tracing::warn!("Failed to resize shm pool: {e}"))
            .ok();

        if self.first_configure && self.request_screencopy(qh) {
            // Request the first capture immediately, before anything has been
            // rendered: the frame appears as fast as possible after launch,
            // contains no feedback of our own output, and — because the
            // capture is requested with `overlay_cursor = 0` — no baked system
            // cursor either. The view then centers exactly on the pointer's
            // launch position when the pointer enters the surface. If the
            // request could not be issued yet (no output known), keep
            // `first_configure` set so the next configure retries it.
            self.first_configure = false;
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
            if let PointerEventKind::Enter { serial, .. } = event.kind {
                self.last_pointer_serial = Some(serial);
                // Pick the cursor surface for the current mode using this
                // fresh enter serial (stale serials are ignored by the
                // compositor, which is why re-asserting elsewhere fails): the
                // Configuration window shows the real system cursor, the
                // magnifier hides the hardware cursor with the blank surface.
                if self.config_window.is_some() {
                    if self.ensure_config_cursor(qh)
                        && let (Some(pointer), Some(surface), Some(hot)) = (
                            &self.pointer,
                            &self.config_cursor_surface,
                            self.config_cursor_hotspot,
                        )
                    {
                        pointer.set_cursor(serial, Some(surface), hot.0, hot.1);
                    }
                } else if let (Some(pointer), Some(surface)) =
                    (&self.pointer, &self.blank_cursor_surface)
                {
                    pointer.set_cursor(serial, Some(surface), 0, 0);
                }
            }
            // While the Configuration window is open, every pointer event goes
            // to the egui UI (the magnifier ignores them). A button press also
            // re-asserts the visible cursor with its fresh serial, covering the
            // case where the open_config serial was stale (the cursor then
            // appears on the first click even without leaving the surface).
            if self.config_window.is_some() {
                if let PointerEventKind::Press { serial, .. } = event.kind
                    && self.ensure_config_cursor(qh)
                    && let (Some(pointer), Some(surface), Some(hot)) = (
                        &self.pointer,
                        &self.config_cursor_surface,
                        self.config_cursor_hotspot,
                    )
                {
                    pointer.set_cursor(serial, Some(surface), hot.0, hot.1);
                }
                self.forward_pointer_to_config(event);
                continue;
            }
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    let position = event.position;
                    self.pointer_position_f = position;
                    self.state.pointer_position = (position.0 as i32, position.1 as i32);
                    // The first capture already happened at first configure
                    // (cursor-free via `overlay_cursor = 0`). This first
                    // enter is where the pointer was at launch, so center
                    // the view exactly on that position (a draw before the
                    // enter would otherwise have left it at the capture
                    // center). Later enters never re-center — that would
                    // jump the view. The enter can arrive before the
                    // capture completes, so record the position and apply
                    // it with the real scale when the capture exists.
                    if !self.launch_centered {
                        self.launch_position = Some(position);
                        self.apply_launch_centering();
                    }
                    self.draw_frame(qh);
                }
                PointerEventKind::Motion { .. } => {
                    let position = event.position;
                    if position != self.pointer_position_f {
                        let now = std::time::Instant::now();
                        let dt = self
                            .last_motion_at
                            .map_or(0.016, |t| now.duration_since(t).as_secs_f64());
                        self.last_motion_at = Some(now);
                        let dx = position.0 - self.pointer_position_f.0;
                        let dy = position.1 - self.pointer_position_f.1;
                        self.pointer_position_f = position;
                        self.state.pointer_position = (position.0 as i32, position.1 as i32);
                        // The view pans with the hand's *movement* (relative
                        // deltas), never by re-centering on the hand's
                        // absolute position — the magnified cursor sits at
                        // the dead center of the viewport, so releasing
                        // hold-to-zoom (or any other state change) can never
                        // make the view jump to the hand.
                        let (sx, sy) = self.capture_scale();
                        let bounds = match &self.captured {
                            Some(c) => (c.buffer.width as f64, c.buffer.height as f64),
                            None => (f64::MAX, f64::MAX),
                        };
                        if self.hold_to_zoom_active {
                            // Hold-to-zoom: vertical motion zooms continuously
                            // (position-based, so it stays smooth at any event
                            // rate; moving up zooms in, down zooms out) and
                            // the view y stays locked to the anchor captured
                            // at press. Only horizontal motion pans the view.
                            let dy_zoom = position.1 - self.hold_zoom_last_y;
                            self.hold_zoom_last_y = position.1;
                            let max_zoom = self.state.config.max_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
                            let new_zoom = (self.state.zoom
                                - dy_zoom * self.state.config.hold_to_zoom_speed)
                                .clamp(MIN_ZOOM, max_zoom);
                            if (new_zoom - self.state.zoom).abs() > 1e-9 {
                                self.state.zoom = new_zoom;
                                self.state.renderer.update_scale_factor(new_zoom);
                                if let Some(cursor) = &mut self.magnified_cursor {
                                    cursor.update_zoom(new_zoom);
                                }
                            }
                            if let Some((cx, cy)) = self.view_center {
                                let nx = self.clamp_to_capture((cx + dx * sx, cy)).0;
                                let reach = EdgeReach::new(self.width as f64, bounds.0, sx);
                                self.view_center =
                                    Some((reach.apply(nx, dx, self.pointer_position_f.0), cy));
                            }
                        } else if let Some((cx, cy)) = self.view_center {
                            // The view pans with the hand's *movement*
                            // (relative deltas) and is hard-clamped to the
                            // capture: the magnified cursor sits at the
                            // viewport center, so pushing against a screen
                            // edge always lands the view *exactly* on the
                            // capture edge (never in the black beyond-capture
                            // fill), and every captured pixel stays reachable.
                            let (nx, ny) = self.clamp_to_capture((cx + dx * sx, cy + dy * sy));
                            // The view-vs-hand offset (view minus hand
                            // content; hold-to-zoom locks the view y while the
                            // hand travels to zoom, and a launch quirk or
                            // resize can leave a residual). Left alone it
                            // shifts the reachable pan range and creates
                            // invisible limits, so it is corrected by real
                            // pointer motion only, every event: each motion
                            // pulls the view a small fraction of the remaining
                            // offset toward the hand content, so navigation is
                            // always fully restored without any jump or
                            // self-animation. The correction never fights a
                            // wall: an axis already pinned to a capture edge is
                            // left untouched, so the view always reaches — and
                            // glides along — the exact edges, regardless of
                            // pointer speed. In steady state the offset is zero
                            // (the view pans 1:1 with the hand), so this is
                            // dormant.
                            let hand = (
                                self.pointer_position_f.0 * sx,
                                self.pointer_position_f.1 * sy,
                            );
                            let offset = (nx - hand.0, ny - hand.1);
                            let (fx, fy) = if offset.0.hypot(offset.1) > 0.5 {
                                let (rox, roy) =
                                    offset_correction_step(offset, dt, (dx, dy), (sx, sy));
                                correct_toward_hand((nx, ny), (hand.0 + rox, hand.1 + roy), bounds)
                            } else {
                                (nx, ny)
                            };
                            // Reach the exact edge when the hand reaches the
                            // physical edge: the compositor's last delivered
                            // pointer position can lag the hand's true stop
                            // by up to one event's travel when it is moved
                            // fast (the delivered position is sampled before
                            // the hand actually stops), which made the view
                            // stop short of the walls at speed while slow
                            // motion always reached. The reach margin scales
                            // with this event's own travel so it always
                            // bridges the gap at any speed (see
                            // [`EdgeReach::apply`]). The correction above can
                            // never fight this, and the view never leaves the
                            // capture.
                            let reach_x = EdgeReach::new(self.width as f64, bounds.0, sx);
                            let reach_y = EdgeReach::new(self.height as f64, bounds.1, sy);
                            self.view_center = Some((
                                reach_x.apply(fx, dx, self.pointer_position_f.0),
                                reach_y.apply(fy, dy, self.pointer_position_f.1),
                            ));
                        }
                        // Diagnostic: near the surface edges, log the raw
                        // geometry so the wall-reach behavior can be verified
                        // against the compositor's delivered pointer
                        // positions (run with `RUST_LOG=maggie=debug`). The
                        // residual (view minus hand content, capture px) is
                        // what discriminates a delivery-gap shortfall (view
                        // tracks the hand, residual ~0, pointer short of the
                        // surface edge) from a residual shortfall (view lags
                        // the hand content).
                        if tracing::enabled!(tracing::Level::DEBUG)
                            && (self.pointer_position_f.0 < 40.0
                                || self.pointer_position_f.0 > self.width as f64 - 40.0
                                || self.pointer_position_f.1 < 40.0
                                || self.pointer_position_f.1 > self.height as f64 - 40.0)
                        {
                            let hand_content = (
                                self.pointer_position_f.0 * sx,
                                self.pointer_position_f.1 * sy,
                            );
                            let residual = match self.view_center {
                                Some((vx, vy)) => (vx - hand_content.0, vy - hand_content.1),
                                None => (0.0, 0.0),
                            };
                            tracing::debug!(
                                pointer = ?self.pointer_position_f,
                                surface = ?(self.width, self.height),
                                view = ?self.view_center,
                                hand_content = ?hand_content,
                                residual = ?residual,
                                delta = ?(dx, dy),
                                reach_margin = {
                                    let m = (dx.abs() * REACH_DELTA_FACTOR + REACH_FLOOR_LOGICAL)
                                        .min(REACH_MAX_LOGICAL);
                                    format!("{:.1}", m)
                                },
                                zoom = self.state.zoom,
                                "near-edge motion"
                            );
                        }
                        self.draw_frame(qh);
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    // Right mouse button quits, same as Q.
                    if button == BTN_RIGHT {
                        self.exit = true;
                    }
                    // Middle mouse button resets the zoom to the default;
                    // the view stays put (zoom scales around the center).
                    if button == BTN_MIDDLE {
                        self.state.reset_zoom();
                        if let Some(cursor) = &mut self.magnified_cursor {
                            cursor.update_zoom(self.state.zoom);
                        }
                        self.draw_frame(qh);
                    }
                }
                PointerEventKind::Leave { serial, .. } => {
                    // Restore default cursor when leaving our surface
                    if let Some(pointer) = &self.pointer {
                        pointer.set_cursor(serial, None, 0, 0);
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
                        let max_zoom = self.state.config.max_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
                        let new_zoom = match self.state.config.scroll_zoom_mode {
                            crate::config::ScrollZoomMode::Levels => {
                                // The wheel walks the same discrete levels as
                                // the 1-9 keys: level i = max_zoom * i / 9.
                                // On a level, step to the neighbour; off a
                                // level, snap to the next one in the wheel
                                // direction (mirrors the old ceil/floor
                                // behaviour, extended to any max zoom).
                                const LEVELS: f64 = 9.0;
                                // Clamp the level index so a zoom beyond the
                                // current max (e.g. max lowered mid-session)
                                // snaps to the top level instead of walking
                                // backwards through the wheel.
                                let idx_f =
                                    ((self.state.zoom / max_zoom) * LEVELS).clamp(1.0, LEVELS);
                                let idx = idx_f.round();
                                let on_level = (idx_f - idx).abs() < 1e-6;
                                let next = if on_level {
                                    if steps > 0.0 {
                                        (idx + 1.0).min(LEVELS)
                                    } else {
                                        (idx - 1.0).max(1.0)
                                    }
                                } else if steps > 0.0 {
                                    idx.ceil().min(LEVELS)
                                } else {
                                    idx.floor().max(1.0)
                                };
                                (max_zoom * next / LEVELS).max(MIN_ZOOM)
                            }
                            crate::config::ScrollZoomMode::Factor => (self.state.zoom
                                * (1.0 + steps * WHEEL_ZOOM_STEP))
                                .clamp(MIN_ZOOM, max_zoom),
                        };
                        if (new_zoom - self.state.zoom).abs() > 1e-9 {
                            self.state.zoom = new_zoom;
                            self.state.renderer.update_scale_factor(new_zoom);
                            if let Some(cursor) = &mut self.magnified_cursor {
                                cursor.update_zoom(new_zoom);
                            }
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
        // Keyboard focus left the surface (e.g. the compositor moved it): the
        // key-release for a held hold-to-zoom modifier may never arrive, so
        // disarm here to avoid an unexpectedly armed state on the next motion.
        self.hold_to_zoom_active = false;
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

        // Configuration window mode: forward keys to egui (typing, focus
        // navigation) instead of driving the magnifier.
        if let Some(cw) = &mut self.config_window {
            if let Some(key) = crate::config_window::keysym_to_egui_key(event.keysym) {
                cw.key(key, true, false);
            }
            if let Some(text) = &event.utf8 {
                cw.text(text.clone());
            }
            self.draw_frame(qh);
            return;
        }

        let keysym_str = keysym_to_string(event.keysym);

        // Hold-to-zoom: pressing the configured modifier arms smooth zooming.
        // The baseline is the current pointer Y, so the zoom does not jump on
        // the first motion event. While held, the motion handler zooms on
        // vertical motion and only pans horizontally, so the view y naturally
        // stays locked to the content under the centered cursor.
        if keysym_str == self.state.config.keybindings.hold_to_zoom {
            self.hold_to_zoom_active = true;
            self.hold_zoom_last_y = self.pointer_position_f.1;
            // Ensure the view center is initialized so the vertical lock
            // engages immediately (in case no motion/draw happened before
            // the press).
            if self.view_center.is_none() {
                let (sx, sy) = self.capture_scale();
                self.view_center = Some(self.clamp_to_capture((
                    self.pointer_position_f.0 * sx,
                    self.pointer_position_f.1 * sy,
                )));
            }
        }

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
            if let Some(cursor) = &mut self.magnified_cursor {
                cursor.update_zoom(self.state.zoom);
            }
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
            self.open_config(qh);
        } else if keysym_str == config_key.anti_aliasing {
            tracing::info!("Anti-aliasing toggle - not yet implemented");
        } else if keysym_str == config_key.mode_center_cursor {
            self.state.switch_mode(MagnifierMode::CenterCursor);
        } else if keysym_str == config_key.mode_edge_pan {
            self.state.switch_mode(MagnifierMode::EdgePan);
        } else if keysym_str == config_key.mode_miniature {
            self.state.switch_mode(MagnifierMode::MiniatureWindow);
        } else if keysym_str == config_key.toggle_cursor {
            self.state.toggle_cursor();
            self.draw_frame(qh);
            tracing::info!("Magnified cursor visible: {}", self.state.cursor_visible);
        } else if keysym_str == config_key.reset_zoom {
            self.state.reset_zoom();
            if let Some(cursor) = &mut self.magnified_cursor {
                cursor.update_zoom(self.state.zoom);
            }
            self.draw_frame(qh);
        }

        if event.keysym == Keysym::Escape || event.keysym == Keysym::q {
            self.exit = true;
        }
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        tracing::debug!("Key repeat: {:?}", event);
        if let Some(cw) = &mut self.config_window
            && let Some(key) = crate::config_window::keysym_to_egui_key(event.keysym)
        {
            cw.key(key, true, true);
            self.draw_frame(qh);
        }
    }

    fn release_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if let Some(cw) = &mut self.config_window
            && let Some(key) = crate::config_window::keysym_to_egui_key(event.keysym)
        {
            cw.key(key, false, false);
            self.draw_frame(qh);
        }
        // Releasing the hold-to-zoom modifier stops smooth zooming. The view
        // stays exactly where it is (no jump, no self-animation). Because the
        // view y was locked while the hand travelled to zoom, the view now
        // sits offset from the hand's content; the Motion handler corrects
        // that offset with real pointer motion, restoring the full pan range.
        if keysym_to_string(event.keysym) == self.state.config.keybindings.hold_to_zoom {
            let was_active = self.hold_to_zoom_active;
            self.hold_to_zoom_active = false;
            if was_active {
                // Fresh baseline so the first correction step after the
                // release never dumps the whole offset (a pause before the
                // next motion would otherwise make dt huge).
                self.last_motion_at = Some(std::time::Instant::now());
                self.draw_frame(qh);
            }
        }
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        if let Some(cw) = &mut self.config_window {
            cw.set_modifiers(egui::Modifiers {
                alt: modifiers.alt,
                ctrl: modifiers.ctrl,
                shift: modifiers.shift,
                mac_cmd: modifiers.logo,
                command: modifiers.ctrl,
            });
        }
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
            self.pointer = Some(pointer.clone());

            // Create a blank cursor surface (1x1 transparent) to hide the hardware cursor.
            // The pool is kept on the window so the backing wl_shm_pool survives: dropping
            // it here would invalidate the buffer while the compositor may still reference it.
            let cursor_surface = self.compositor_state.create_surface(qh);
            let mut cursor_pool =
                SlotPool::new(4, &self.shm).expect("Failed to create cursor pool");
            if let Ok((buffer, canvas)) =
                cursor_pool.create_buffer(1, 1, 4, wl_shm::Format::Argb8888)
            {
                canvas[0] = 0; // R
                canvas[1] = 0; // G
                canvas[2] = 0; // B
                canvas[3] = 0; // A (transparent)
                buffer.attach_to(&cursor_surface).expect("buffer attach");
                cursor_surface.commit();
                self.blank_cursor_surface = Some(cursor_surface);
                self.cursor_pool = Some(cursor_pool);
            }
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
            self.blank_cursor_surface.take();
            self.cursor_pool.take();
            self.config_cursor_surface.take();
            self.config_cursor_pool.take();
            self.config_cursor_hotspot.take();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

/// Map a wl_pointer button code to the egui pointer button, if recognized.
fn wl_button_to_egui(button: u32) -> Option<egui::PointerButton> {
    match button {
        0x110 => Some(egui::PointerButton::Primary), // BTN_LEFT
        0x111 => Some(egui::PointerButton::Secondary), // BTN_RIGHT
        0x112 => Some(egui::PointerButton::Middle),  // BTN_MIDDLE
        _ => None,
    }
}

/// Normalize a keysym into the string used to match keybindings. Printable
/// ASCII keysyms map to their character (so any letter/digit/punctuation key
/// can be bound); the named keys that bindings use get canonical names
/// (`Tab`, `Escape`, and the modifier keys — either side collapses to the
/// same name, e.g. `Super_L`/`Super_R` → `Super`); anything else falls back
/// to its numeric value, which no binding matches.
fn keysym_to_string(keysym: Keysym) -> String {
    use smithay_client_toolkit::seat::keyboard::Keysym as K;
    match keysym {
        K::Escape => "Escape".to_string(),
        K::Tab => "Tab".to_string(),
        K::space => "Space".to_string(),
        K::Super_L | K::Super_R => "Super".to_string(),
        K::Control_L | K::Control_R => "Control".to_string(),
        K::Alt_L | K::Alt_R => "Alt".to_string(),
        K::Shift_L | K::Shift_R => "Shift".to_string(),
        _ => {
            let value = u32::from(keysym);
            if (0x21..=0x7E).contains(&value)
                && let Some(c) = char::from_u32(value)
            {
                c.to_string()
            } else {
                format!("{}", value)
            }
        }
    }
}

smithay_client_toolkit::delegate_dispatch2!(MagnifierWindow);

impl MagnifierWindow {
    /// Request a screencopy of the current output. Returns `false` (without
    /// retrying) when the capture cannot be issued yet — e.g. no output is
    /// known — so callers can retry later instead of leaving the overlay
    /// permanently invisible.
    fn request_screencopy(&mut self, qh: &QueueHandle<Self>) -> bool {
        let Some(manager) = &self.screencast_manager else {
            return false;
        };
        let output = self
            .current_output
            .clone()
            .or_else(|| self.output_state.outputs().next());
        let Some(output) = output else {
            tracing::error!("No output available for screencopy");
            return false;
        };

        // `overlay_cursor = 0`: the wlr-screencopy protocol requires the
        // compositor to NOT include the cursor in the capture. This is the
        // definitive cursor-exclusion mechanism (honored by niri, wlroots,
        // KWin, Hyprland, ...) — the frozen frame can never contain a baked
        // copy of the system cursor, regardless of pointer focus or timing.
        let _frame = manager.capture_output(false as i32, &output, qh, ScreencastFrameData);
        true
    }

    /// The per-logical-pixel capture scale (`capture / logical`), used to
    /// convert pointer-motion deltas into capture-px view panning. Falls back
    /// to 1.0 before the first capture arrives.
    fn capture_scale(&self) -> (f64, f64) {
        match &self.captured {
            Some(c) => (
                c.buffer.width as f64 / self.width as f64,
                c.buffer.height as f64 / self.height as f64,
            ),
            None => (1.0, 1.0),
        }
    }

    /// Clamp a view-center coordinate to the frozen capture's bounds
    /// (capture px). The magnified cursor sits at the viewport center, which
    /// *is* the view center: keeping the center inside the capture guarantees
    /// the cursor never enters the black beyond-capture fill, and that every
    /// captured pixel stays reachable — pushing against a screen edge always
    /// lands the view exactly on the capture edge. No-op before the first
    /// capture arrives.
    fn clamp_to_capture(&self, pos: (f64, f64)) -> (f64, f64) {
        match &self.captured {
            Some(c) => {
                clamp_to_capture_bounds(pos, (c.buffer.width as f64, c.buffer.height as f64))
            }
            None => pos,
        }
    }

    /// Center the view on the launch pointer's content once, using the real
    /// capture scale. Requires both the launch position (first enter) and the
    /// capture; the enter can arrive before the screencopy completes.
    fn apply_launch_centering(&mut self) {
        if self.launch_centered || self.captured.is_none() {
            return;
        }
        if let Some(position) = self.launch_position.take() {
            let (sx, sy) = self.capture_scale();
            self.view_center = Some(self.clamp_to_capture((position.0 * sx, position.1 * sy)));
            self.launch_centered = true;
        }
    }

    fn osd_lines(&self) -> Vec<String> {
        let config_key = &self.state.config.keybindings;
        vec![
            format!("maggie  zoom {:.2}x", self.state.zoom),
            "1-9  zoom level".to_string(),
            format!("{}  toggle OSD", config_key.toggle_osd),
            format!(
                "{}  screenshot fullscreen",
                config_key.screenshot_fullscreen
            ),
            format!("{}  manual selection", config_key.screenshot_manual),
            format!("{}  window selection", config_key.screenshot_window),
            format!("{}  config window", config_key.config_window),
            format!("{}  toggle cursor", config_key.toggle_cursor),
            format!("hold {} + move  smooth zoom", config_key.hold_to_zoom),
            format!("MMB / {}  reset zoom", config_key.reset_zoom),
            "Q / Esc / RMB  quit".to_string(),
        ]
    }

    fn request_frame_callback(&mut self, qh: &QueueHandle<Self>) {
        let surface = self.layer.wl_surface().clone();
        let callback = surface.clone().frame(qh, FrameCallbackData(surface));
        self.frame_callback = Some(callback);
    }

    /// Build the cursor surface that shows the real system cursor at its
    /// native size (used while the Configuration window is open), converting
    /// the straight-alpha RGBA theme image into a premultiplied ARGB8888 shm
    /// buffer. Cached: subsequent calls are no-ops. Returns `false` on
    /// failure so callers can skip `set_cursor`.
    fn ensure_config_cursor(&mut self, qh: &QueueHandle<Self>) -> bool {
        if self.config_cursor_surface.is_some() {
            return true;
        }
        let Some(cursor) = &self.magnified_cursor else {
            return false;
        };
        let (base, (hx, hy)) = cursor.base_image();
        let (w, h) = (base.width, base.height);
        let mut pool = match SlotPool::new((w * h * 4) as usize, &self.shm) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to create config cursor pool: {e}");
                return false;
            }
        };
        let surface = self.compositor_state.create_surface(qh);
        if let Ok((buffer, canvas)) = pool.create_buffer(w, h, w * 4, wl_shm::Format::Argb8888) {
            // Straight-alpha RGBA -> premultiplied BGRA (ARGB8888 little-
            // endian), as the compositor expects for cursor buffers.
            for (px, out) in base.data.chunks_exact(4).zip(canvas.chunks_exact_mut(4)) {
                let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
                out[0] = (b * a / 255) as u8;
                out[1] = (g * a / 255) as u8;
                out[2] = (r * a / 255) as u8;
                out[3] = a as u8;
            }
            buffer.attach_to(&surface).expect("buffer attach");
            surface.commit();
            self.config_cursor_surface = Some(surface);
            self.config_cursor_pool = Some(pool);
            self.config_cursor_hotspot = Some((hx.round() as i32, hy.round() as i32));
            true
        } else {
            false
        }
    }

    /// Open the egui Configuration window over the whole surface. Requires the
    /// GPU render path (egui-glow paints into the EGL surface); on the CPU
    /// fallback the window is unavailable and a warning is logged.
    fn open_config(&mut self, qh: &QueueHandle<Self>) {
        if self.config_window.is_some() {
            return;
        }
        let Some(gpu) = &self.gpu else {
            tracing::warn!("Configuration window requires the GPU render path");
            return;
        };
        match ConfigWindow::new(
            gpu.glow(),
            self.width as i32,
            self.height as i32,
            self.state.config.clone(),
        ) {
            Ok(cw) => self.config_window = Some(cw),
            Err(e) => {
                tracing::error!("Failed to open Configuration window: {e:#}");
                return;
            }
        }
        // Never enter hold-to-zoom inside the window (the modifier would be
        // forwarded to egui anyway).
        self.hold_to_zoom_active = false;
        // Keyboard focus stays `on-demand` (set at startup): niri grants it
        // to layer surfaces at map time (pointer over the surface) and again
        // on every click, so the UI's text fields receive keys after the
        // first click. Crucially, we must NOT toggle interactivity here:
        // niri clears its remembered on-demand focus while a surface is
        // `exclusive` and only re-grants it on a click, which would leave the
        // magnifier's global keys dead after closing the window.
        // Show the real system cursor so the UI can be operated with a
        // visible pointer. If the serial is stale the compositor ignores this
        // call, but the next pointer enter (fresh serial) re-asserts it.
        if let Some(serial) = self.last_pointer_serial
            && self.ensure_config_cursor(qh)
            && let (Some(pointer), Some(surface), Some(hot)) = (
                &self.pointer,
                &self.config_cursor_surface,
                self.config_cursor_hotspot,
            )
        {
            pointer.set_cursor(serial, Some(surface), hot.0, hot.1);
        }
        tracing::info!("Configuration window opened");
        self.draw_frame(qh);
    }

    /// Close the Configuration window and return to the magnifier.
    fn close_config(&mut self, qh: &QueueHandle<Self>) {
        if let Some(mut cw) = self.config_window.take() {
            cw.destroy();
        } else {
            return;
        }
        // Interactivity stays `on-demand` (unchanged from startup), so the
        // keyboard focus held before/while the window was open is preserved
        // and the magnifier's global keys keep working immediately.
        // Re-hide the hardware cursor if the pointer is over the surface.
        if let (Some(pointer), Some(surface), Some(serial)) = (
            &self.pointer,
            &self.blank_cursor_surface,
            self.last_pointer_serial,
        ) {
            pointer.set_cursor(serial, Some(surface), 0, 0);
        }
        tracing::info!("Configuration window closed");
        self.draw_frame(qh);
    }

    /// Route a pointer event to the egui Configuration window.
    fn forward_pointer_to_config(&mut self, event: &PointerEvent) {
        let Some(cw) = &mut self.config_window else {
            return;
        };
        match event.kind {
            PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                let pos = egui::pos2(event.position.0 as f32, event.position.1 as f32);
                cw.pointer_moved(pos);
            }
            PointerEventKind::Press { button, .. } => {
                if let Some(b) = wl_button_to_egui(button) {
                    cw.pointer_button(b, true);
                }
            }
            PointerEventKind::Release { button, .. } => {
                if let Some(b) = wl_button_to_egui(button) {
                    cw.pointer_button(b, false);
                }
            }
            PointerEventKind::Axis {
                vertical,
                horizontal,
                ..
            } => {
                let dy = if vertical.value120 != 0 {
                    vertical.value120 as f32 / 120.0
                } else {
                    vertical.discrete as f32
                };
                let dx = if horizontal.value120 != 0 {
                    horizontal.value120 as f32 / 120.0
                } else {
                    horizontal.discrete as f32
                };
                // Wayland axis is positive when scrolling down; egui's wheel
                // delta is positive when scrolling up.
                cw.pointer_axis(egui::vec2(dx, -dy));
            }
            PointerEventKind::Leave { .. } => {
                cw.pointer_left();
            }
        }
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
                tracing::warn!(
                    "GPU rendering unavailable, falling back to CPU path: {:#}",
                    e
                );
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
        // Configuration window mode: paint the egui UI instead of the magnifier.
        if let Some(mut cw) = self.config_window.take() {
            let result = cw.update(&mut self.state.config);
            if result == UiResult::Continue {
                cw.paint();
                if let Some(gpu) = &self.gpu {
                    gpu.swap_buffers();
                }
            }
            self.config_window = Some(cw);
            if result == UiResult::Close {
                self.close_config(qh);
                return;
            }
            // Keep repainting while the window is open (widgets, caret, ...).
            self.request_frame_callback(qh);
            return;
        }

        let Some(captured) = self.captured.as_ref() else {
            return;
        };
        let source_w = captured.buffer.width;
        let source_h = captured.buffer.height;

        let zoom = self.state.zoom;
        let scale_x = source_w as f64 / self.width as f64;
        let scale_y = source_h as f64 / self.height as f64;

        // Dest size and letterbox offset of the magnified quad (the GPU path
        // fills the whole buffer and ignores the offsets). Computed early so
        // the hold-to-zoom anchor below can compensate for the letterbox.
        let view_w = self.width as f64 / zoom;
        let view_h = self.height as f64 / zoom;
        let dest_w = (view_w.min(source_w as f64) * zoom).round() as i32;
        let dest_h = (view_h.min(source_h as f64) * zoom).round() as i32;
        let off_x = ((self.width as i32 - dest_w) / 2).max(0);
        let off_y = ((self.height as i32 - dest_h) / 2).max(0);
        let target = if self.pointer_seen {
            (
                self.pointer_position_f.0 * scale_x,
                self.pointer_position_f.1 * scale_y,
            )
        } else {
            (source_w as f64 / 2.0, source_h as f64 / 2.0)
        };

        // The view center is maintained by the pointer-motion handler, which
        // pans it by the hand's *movement* (relative deltas). The magnified
        // cursor always sits at the dead center of the viewport, and only the
        // magnified screen moves — so no state change (releasing hold-to-zoom
        // included) can ever make the view jump to the hand. The center is
        // only initialized from the pointer content once, before the first
        // motion event (e.g. at launch).
        let (center_x, center_y, animating) = match self.view_center {
            Some((cx, cy)) => (cx, cy, false),
            None => {
                self.view_center = Some(target);
                (target.0, target.1, false)
            }
        };
        self.animating = animating;

        // Hard invariant: the view center (which is where the magnified
        // cursor sits — the dead center of the viewport) never leaves the
        // captured screen. The magnified *view* may still sample past the
        // frozen frame near the edges (that region is painted black), but the
        // cursor itself can never enter that black zone, and every captured
        // pixel stays reachable. Only the magnified screen moves when the
        // mouse moves; the cursor sprite stays still in the exact center.
        let (center_x, center_y) = self.clamp_to_capture((center_x, center_y));
        let src_x = center_x - view_w / 2.0;
        let src_y = center_y - view_h / 2.0;

        let lines = self.osd_lines();

        // The magnified cursor is always drawn at the exact center of the
        // viewport (the center of the magnified quad; the quad fills the
        // screen at zoom >= 1).
        let cursor_logical =
            if self.pointer_seen && self.state.cursor_visible && self.magnified_cursor.is_some() {
                Some((
                    off_x as f64 + dest_w as f64 / 2.0,
                    off_y as f64 + dest_h as f64 / 2.0,
                ))
            } else {
                None
            };

        // The OSD ring marks the magnified cursor (always at the viewport
        // center); fall back to the hand position when no magnified cursor is
        // drawn.
        let osd_ring = cursor_logical
            .map(|(cx, cy)| (cx as i32, cy as i32))
            .unwrap_or(self.state.pointer_position);

        if let Some(gpu) = &mut self.gpu {
            let osd = if self.state.osd_visible {
                crate::osd::build_osd_sprite(
                    &lines,
                    (
                        osd_ring.0 * crate::gpu::RENDER_SCALE,
                        osd_ring.1 * crate::gpu::RENDER_SCALE,
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
            // The GPU buffer is RENDER_SCALE x the logical size, so both the
            // cursor sprite, its hotspot and its position must be scaled to
            // match.
            let cursor = cursor_logical.map(|(cx, cy)| {
                let (buf, (hx, hy)) = self
                    .magnified_cursor
                    .as_mut()
                    .expect("magnified cursor present")
                    .sprite(crate::gpu::RENDER_SCALE as f64);
                (
                    (
                        (cx * crate::gpu::RENDER_SCALE as f64) as i32,
                        (cy * crate::gpu::RENDER_SCALE as f64) as i32,
                    ),
                    buf,
                    (hx, hy),
                )
            });
            gpu.draw(Some(uv), osd.as_ref(), cursor.as_ref());
            if self.animating {
                self.request_frame_callback(qh);
            }
            return;
        }

        let scaled =
            self.state
                .renderer
                .render_bilinear(&captured.buffer, (src_x, src_y), dest_w, dest_h);

        let show_osd = self.state.osd_visible;
        let osd_lines = self.osd_lines();
        let osd_cursor = osd_ring; // Precompute the cursor sprite up front: the fill closure below cannot
        // borrow `self` while `render_frame` holds `&mut self`.
        let cursor_buf = cursor_logical.map(|(cx, cy)| {
            let (buf, (hx, hy)) = self
                .magnified_cursor
                .as_mut()
                .expect("magnified cursor present")
                .sprite(1.0);
            (
                (cx as i32, cy as i32),
                buf,
                (hx.round() as i32, hy.round() as i32),
            )
        });

        self.render_frame(qh, |canvas, width, height, stride| {
            canvas.fill(0);
            for y in 0..dest_h {
                let src_row = &scaled.data[(y as usize) * (scaled.width as usize) * 4..];
                let dest_row = &mut canvas
                    [((y + off_y) as usize) * (stride as usize) + (off_x as usize) * 4..];
                dest_row[..(dest_w as usize) * 4]
                    .copy_from_slice(&src_row[..(dest_w as usize) * 4]);
            }
            if let Some((cursor_pos, ref cursor_sprite, hotspot)) = cursor_buf {
                Self::draw_cursor_at(canvas, stride, cursor_pos, cursor_sprite, hotspot);
            }
            if show_osd {
                crate::osd::draw_osd(canvas, width, height, &osd_lines, osd_cursor);
            }
        });
    }

    /// Blit a magnified-cursor sprite so its hotspot lands exactly on `pos`.
    /// `stride` is the byte stride of a canvas row (width * 4); the usable
    /// pixel width of each row is stride / 4.
    fn draw_cursor_at(
        canvas: &mut [u8],
        stride: i32,
        pos: (i32, i32),
        cursor: &RgbaBuffer,
        hotspot: (i32, i32),
    ) {
        let (cursor_w, cursor_h) = (cursor.width, cursor.height);
        let (pos_x, pos_y) = pos;
        let (hot_x, hot_y) = hotspot;
        let canvas_w = stride / 4;
        let canvas_h = canvas.len() as i32 / stride;

        for y in 0..cursor_h {
            let dest_y = pos_y - hot_y + y;
            if dest_y < 0 || dest_y >= canvas_h {
                continue;
            }
            for x in 0..cursor_w {
                let dest_x = pos_x - hot_x + x;
                if dest_x < 0 || dest_x >= canvas_w {
                    continue;
                }
                let src_idx = ((y as usize) * (cursor_w as usize) + x as usize) * 4;
                let dest_idx = (dest_y as usize) * (stride as usize) + (dest_x as usize) * 4;
                let src_pixel = &cursor.data[src_idx..src_idx + 4];
                let dest_pixel = &mut canvas[dest_idx..dest_idx + 4];

                // Alpha blending
                let src_a = src_pixel[3] as f32 / 255.0;
                for i in 0..4 {
                    dest_pixel[i] = (src_pixel[i] as f32 * src_a
                        + dest_pixel[i] as f32 * (1.0 - src_a))
                        .round() as u8;
                }
            }
        }
    }

    fn draw_black_overlay(&mut self, qh: &QueueHandle<Self>) {
        if let Some(gpu) = &mut self.gpu {
            gpu.draw(None, None, None);
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
    let shm =
        Shm::bind(&globals, &qh).map_err(|e| anyhow::anyhow!("SHM not available: {:?}", e))?;
    let output_state = OutputState::new(&globals, &qh);

    let screencast_manager = globals
        .bind::<ZwlrScreencopyManagerV1, _, _>(&qh, 1..=3, ScreencastManagerData)
        .ok();

    let mut event_queue = event_queue;

    let surface = compositor.create_surface(&qh);

    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("maggie"), None);
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
    let start_zoom = state.zoom;

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
        launch_centered: false,
        launch_position: None,
        view_center: None,
        last_motion_at: None,
        animating: false,
        frame_callback: None,
        width: 1920,
        height: 1080,
        current_output: None,
        pointer_seen: false,
        pointer_position_f: (0.0, 0.0),
        keyboard: None,
        pointer: None,
        magnified_cursor: Some(crate::cursor::MagnifiedCursor::new(start_zoom)),
        blank_cursor_surface: None,
        cursor_pool: None,
        config_cursor_surface: None,
        config_cursor_pool: None,
        config_cursor_hotspot: None,
        config_window: None,
        last_pointer_serial: None,
        hold_to_zoom_active: false,
        hold_zoom_last_y: 0.0,
    };

    tracing::info!(
        "Maggie magnifier started with zoom {} on layer surface",
        window.state.zoom
    );

    if window.screencast_manager.is_some() {
        tracing::info!("Screencopy manager available");
    }

    loop {
        if window.exit {
            tracing::info!("Exiting");
            break;
        }
        // Block on the display, dispatching as events arrive (pointer motion,
        // capture Ready, frame callbacks). The first capture was already
        // requested at first configure, so the frame appears with no delay.
        event_queue.blocking_dispatch(&mut window)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysym_to_string_maps_named_keys() {
        use smithay_client_toolkit::seat::keyboard::Keysym as K;
        assert_eq!(keysym_to_string(K::Tab), "Tab");
        assert_eq!(keysym_to_string(K::space), "Space");
        assert_eq!(keysym_to_string(K::Escape), "Escape");
        assert_eq!(keysym_to_string(K::Super_L), "Super");
        assert_eq!(keysym_to_string(K::Super_R), "Super");
        assert_eq!(keysym_to_string(K::Control_L), "Control");
        assert_eq!(keysym_to_string(K::c), "c");
        assert_eq!(keysym_to_string(K::k), "k");
    }

    #[test]
    fn cursor_toggle_flips_visibility() {
        let mut state = MagnifierState::new(MagnifierConfig::default(), Some(3.0));
        assert!(state.cursor_visible);
        state.toggle_cursor();
        assert!(!state.cursor_visible);
        state.toggle_cursor();
        assert!(state.cursor_visible);
    }

    #[test]
    fn clamp_keeps_view_center_inside_capture() {
        let bounds = (3200.0, 2000.0);
        assert_eq!(clamp_to_capture_bounds((-10.0, 5.0), bounds), (0.0, 5.0));
        assert_eq!(
            clamp_to_capture_bounds((3300.0, -3.0), bounds),
            (3200.0, 0.0)
        );
        assert_eq!(clamp_to_capture_bounds((10.0, 20.0), bounds), (10.0, 20.0));
        // Pushing against a wall lands exactly on the capture edge.
        assert_eq!(clamp_to_capture_bounds((3200.0, 2000.0), bounds), bounds);
        assert_eq!(clamp_to_capture_bounds((1e9, 1e9), bounds), bounds);
    }

    #[test]
    fn view_round_trips_reach_both_edges_exactly_with_residual_offset() {
        // Simulate the motion handler (pan + hard clamp + wall-aware offset
        // correction + hand-edge reach) with a leftover view-vs-hand offset
        // AND a pointer whose delivered position stops short of the surface
        // edge (edge clamping / fast-stop lag). Repeated full left-right
        // panning must always land the view *exactly* on both edges — the
        // wall wins — and the offset must decay away during free motion.
        let scale = 1.5;
        let capture = 3000.0;
        let surface = capture / scale; // the hand's logical coordinate range
        // The hand's delivered travel stops 3 capture px short of the right
        // edge, which used to make the view stop short of the wall.
        let (hand_min, hand_max) = (0.0, (capture - 3.0) / scale);
        // Residual offset: view 300 capture px ahead of the hand.
        let mut view: f64 = 300.0;
        let mut seen_left_edge = false;
        let mut seen_right_edge = false;
        for _ in 0..30 {
            // Pan left to the wall, then right to the wall, several times.
            for (dir, target_hand) in [(-1.0, hand_min), (1.0, hand_max)] {
                let mut hand: f64 = if dir < 0.0 { hand_max } else { hand_min };
                while hand != target_hand {
                    let step = (target_hand - hand).abs().min(16.0) * dir;
                    hand += step;
                    // Pan + hard clamp: the wall wins, exactly as in the
                    // motion handler.
                    let nx = (view + step * scale).clamp(0.0, capture);
                    let hand_content = hand * scale;
                    let offset = nx - hand_content;
                    let corrected = if offset.abs() > 0.5 {
                        let (rox, _) = offset_correction_step(
                            (offset, 0.0),
                            0.016,
                            (step, 0.0),
                            (scale, scale),
                        );
                        // Wall-aware correction: a pinned axis never moves.
                        let pinned = nx <= 0.0 || nx >= capture;
                        if pinned {
                            nx
                        } else {
                            (hand_content + rox).clamp(0.0, capture)
                        }
                    } else {
                        nx
                    };
                    let reach = EdgeReach::new(surface, capture, scale);
                    view = reach.apply(corrected, step, hand);
                }
                if view == 0.0 {
                    seen_left_edge = true;
                }
                if view == capture {
                    seen_right_edge = true;
                }
            }
        }
        assert!(seen_left_edge, "view must reach the exact left edge");
        assert!(seen_right_edge, "view must reach the exact right edge");
    }

    #[test]
    fn reach_margin_scales_with_event_travel() {
        // The reach margin is proportional to the pointer's per-event travel:
        // a fast flick whose delivered position stops well short of the
        // surface edge (up to one event's travel) still lands the view
        // exactly on the wall, while a slow push keeps a small margin so the
        // edge is never magnetic.
        let scale = 1.5;
        let surface = 2133.0;
        let bounds = 3200.0;
        let reach = EdgeReach::new(surface, bounds, scale);
        // Fast flick: 60 logical px short of the edge, event travel 40 px
        // (margin = 68 px) -> the gap is bridged, view lands on the wall.
        assert_eq!(reach.apply(3150.0, 40.0, surface - 60.0), bounds);
        // The same shortfall at slow speed (travel 1 px, margin ~9.5 px)
        // does not bridge it: the view stays put (no magnetic wall).
        assert_eq!(reach.apply(3150.0, 1.0, surface - 60.0), 3150.0);
        // Pushing away from the edge never triggers, however large the
        // travel (here: pushing left while the pointer sits near the right
        // edge).
        assert_eq!(reach.apply(3150.0, -40.0, surface - 10.0), 3150.0);
        // A view far from the wall never triggers (no teleports).
        assert_eq!(reach.apply(2500.0, 40.0, surface - 60.0), 2500.0);
    }

    #[test]
    fn reach_margin_is_capped() {
        // The margin is capped (REACH_MAX_LOGICAL) so a single absurd event
        // cannot create a huge magnetic zone: a pointer 150 logical px short
        // of the edge never triggers even with extreme travel.
        let reach = EdgeReach::new(2133.0, 3200.0, 1.5);
        assert_eq!(reach.apply(3100.0, 500.0, 2133.0 - 150.0), 3100.0);
        // A 1000 px travel still fires within the cap (margin = 120 px).
        assert_eq!(reach.apply(3150.0, 1000.0, 2133.0 - 100.0), 3200.0);
    }

    #[test]
    fn reach_wall_edge_lands_on_the_wall_when_the_hand_reaches_the_edge() {
        let scale = 1.5;
        let surface = 2133.0;
        let bounds = 3200.0;
        let reach = EdgeReach::new(surface, bounds, scale);
        // The exact failure the user reported: the pointer's delivered
        // position stops short of the surface edge when moved fast, so the
        // view settles short of the wall. The hand being pushed into the
        // edge (delivered position within EDGE_MARGIN of the surface edge)
        // must land the view exactly on the wall.
        assert_eq!(reach.apply(3180.0, 5.0, 2120.0), bounds);
        // Pushing left into the left edge lands on 0.
        assert_eq!(reach.apply(20.0, -5.0, 10.0), 0.0);
        // Pushing away from an edge never triggers.
        assert_eq!(reach.apply(3180.0, -5.0, 2120.0), 3180.0);
        // Hand mid-screen never triggers.
        assert_eq!(reach.apply(3180.0, 5.0, 1000.0), 3180.0);
        // A view too far from the wall never triggers (no teleports).
        assert_eq!(reach.apply(2800.0, 5.0, 2120.0), 2800.0);
        // Gliding (no movement this event) never triggers.
        assert_eq!(reach.apply(3180.0, 0.0, 2120.0), 3180.0);
    }

    #[test]
    fn reach_wall_edge_is_speed_and_direction_safe() {
        let scale = 1.5;
        let surface = 2133.0;
        let bounds = 3200.0;
        let reach = EdgeReach::new(surface, bounds, scale);
        // Even a tiny push while the hand is jammed at the edge lands on the
        // wall (this is what slow crawling needed before).
        assert_eq!(reach.apply(3199.0, 0.1, 2130.0), bounds);
        // The hand within the margin but not pushing: untouched.
        assert_eq!(reach.apply(3190.0, 0.0, 2120.0), 3190.0);
        // Pushing toward the edge with the hand just outside the margin:
        // untouched (the margin bounds the magnetic feel).
        assert_eq!(reach.apply(3190.0, 5.0, 2100.0), 3190.0);
    }

    #[test]
    fn correct_toward_hand_moves_free_view_toward_target() {
        let bounds = (3200.0, 2000.0);
        // Free view: the corrected position is the target (hand + remaining
        // offset), clamped to the capture.
        assert_eq!(
            correct_toward_hand((1000.0, 500.0), (980.0, 520.0), bounds),
            (980.0, 520.0)
        );
        // An out-of-bounds target is clamped back inside.
        assert_eq!(
            correct_toward_hand((1000.0, 500.0), (-5.0, 2100.0), bounds),
            (0.0, 2000.0)
        );
    }

    #[test]
    fn correct_toward_hand_never_pulls_a_view_off_a_wall() {
        let bounds = (3200.0, 2000.0);
        // View pinned on the right wall (the wall won the clamp), hand still
        // behind it: the correction must NOT drag the view off the wall —
        // this is what made fast pushes stop short of the exact border.
        let pinned = (3200.0, 1000.0);
        let target = (3100.0, 1100.0);
        assert_eq!(
            correct_toward_hand(pinned, target, bounds),
            (3200.0, 1100.0)
        );
        // Same on the left wall.
        let pinned = (0.0, 1000.0);
        assert_eq!(
            correct_toward_hand(pinned, (100.0, 900.0), bounds),
            (0.0, 900.0)
        );
        // Pinned on both axes: nothing moves.
        let corner = (0.0, 2000.0);
        assert_eq!(correct_toward_hand(corner, (500.0, 1500.0), bounds), corner);
    }

    #[test]
    fn correct_toward_hand_glides_along_pinned_edge() {
        let bounds = (3200.0, 2000.0);
        // View at the right wall, gliding vertically: the x stays pinned to
        // the exact edge while the free y axis is corrected.
        let view = (3200.0, 1000.0);
        let target = (3190.0, 1010.0);
        assert_eq!(correct_toward_hand(view, target, bounds), (3200.0, 1010.0));
        // Gliding along the bottom edge keeps the y pinned.
        let view = (1600.0, 2000.0);
        let target = (1620.0, 1980.0);
        assert_eq!(correct_toward_hand(view, target, bounds), (1620.0, 2000.0));
    }

    #[test]
    fn offset_correction_factor_is_bounded_and_capped() {
        // No time elapsed -> no correction.
        assert_eq!(offset_correction_factor(0.0), 0.0);
        // A normal inter-event gap corrects a small fraction.
        let f = offset_correction_factor(0.016);
        assert!(f > 0.0 && f < 0.1, "f = {f}");
        // Monotonic in dt.
        assert!(offset_correction_factor(0.05) > offset_correction_factor(0.01));
        // A huge pause is capped: it must never dump the whole offset at once.
        let f_big = offset_correction_factor(10.0);
        assert!(
            f_big <= 1.0 - (-OFFSET_CORRECT_DT_CAP / OFFSET_CORRECT_TAU).exp() + 1e-9,
            "f_big = {f_big}"
        );
        assert!(f_big > 0.0 && f_big < 0.3);
    }

    #[test]
    fn offset_correction_converges_to_zero_over_motion() {
        // Repeated motion-driven steps (16 ms apart) must erase a large
        // residual offset (the kind hold-to-zoom accumulates) without ever
        // overshooting past zero.
        let mut o: (f64, f64) = (930.0, -240.0);
        let mut steps = 0;
        while o.0.hypot(o.1) >= 0.5 && steps < 100_000 {
            let f = offset_correction_factor(0.016);
            o = (o.0 - o.0 * f, o.1 - o.1 * f);
            steps += 1;
        }
        assert!(o.0.hypot(o.1) < 0.5, "offset {o:?} after {steps} steps");
        assert!(steps < 100_000);
        // The view-side correction subtracts the offset, so it moves toward
        // the hand content and never flips sign.
        assert!(o.0.abs() < 0.5 && o.1.abs() < 0.5);
    }

    #[test]
    fn offset_correction_step_after_pause_does_not_lurch() {
        // A huge dt after a pause (capped internally) with the hand barely
        // moved: the per-event correction must be bounded by the hand's own
        // travel, not by the time — no visible jump on the first motion
        // after releasing hold-to-zoom.
        let o: (f64, f64) = (930.0, -240.0);
        let travel = (2.0, -1.0);
        let scale = (1.5, 1.5);
        let after = offset_correction_step(o, 10.0, travel, scale);
        // Corrected at most 2x the hand's travel in each axis.
        assert!(o.0 - after.0 <= travel.0.abs() * scale.0 * 2.0 + 1e-9);
        assert!(o.1 - after.1 <= travel.1.abs() * scale.1 * 2.0 + 1e-9);
        // And it never overshoots past zero.
        assert!(after.0 >= 0.0 && after.1 <= 0.0);
        // Continuous motion still converges.
        let mut o2 = o;
        for _ in 0..100_000 {
            o2 = offset_correction_step(o2, 0.016, (20.0, -20.0), scale);
            if o2.0.hypot(o2.1) < 0.5 {
                break;
            }
        }
        assert!(o2.0.hypot(o2.1) < 0.5, "offset {o2:?}");
    }

    #[test]
    fn offset_correction_boost_heals_fast_toward_hand_but_never_overshoots() {
        // Pushing toward the blocked wall (= toward the hand content) the
        // view catches up fast, but the correction never passes the hand.
        // o = +5 (view below the hand), t = -100 (pushing up toward it).
        let after = offset_correction_step((5.0, 0.0), 0.016, (-100.0, 0.0), (1.5, 1.5));
        assert_eq!(after, (0.0, 0.0), "fully healed in one push, no overshoot");
        // Moving away from the hand content heals gently (time-based only),
        // so the correction never fights or lurches the user.
        let away = offset_correction_step((300.0, 0.0), 0.016, (100.0, 0.0), (1.5, 1.5));
        assert!(
            away.0 > 290.0,
            "away-motion heals gently (time-based), got {}",
            away.0
        );
        // The boosted correction is still bounded by 2x the hand's travel.
        let big = offset_correction_step((5000.0, 0.0), 0.016, (10.0, 0.0), (1.5, 1.5));
        assert!(
            5000.0 - big.0 <= 10.0 * 1.5 * 2.0 + 1e-9,
            "bounded by 2x travel"
        );
    }

    #[test]
    fn boost_restores_far_wall_reach_after_hold_to_zoom() {
        // The exact failure the user reported: after a hold-to-zoom zoom-out
        // the view is left above the hand content (negative y offset), so
        // pushing down to the bottom wall used to stop short by the residual
        // (arbitrary distance, worse when moving fast). With the boost the
        // residual is erased en route and the wall is reached exactly.
        let scale = 1.5;
        let capture = 3000.0;
        let surface = capture / scale;
        // View 300 capture px above the hand content (zoom-out residual).
        let mut view: f64 = 1200.0;
        let mut hand: f64 = 1000.0;
        let mut reached_wall = false;
        // Push down toward the bottom wall in 16-logical-px hand steps.
        while hand < surface {
            let step = 16.0;
            hand += step;
            let nx = (view + step * scale).clamp(0.0, capture);
            let hand_content = hand * scale;
            let offset = nx - hand_content;
            let corrected = if offset.abs() > 0.5 {
                let (rox, _) =
                    offset_correction_step((offset, 0.0), 0.016, (step, 0.0), (scale, scale));
                // Wall-aware correction: a pinned axis never moves.
                let pinned = nx <= 0.0 || nx >= capture;
                if pinned {
                    nx
                } else {
                    (hand_content + rox).clamp(0.0, capture)
                }
            } else {
                nx
            };
            let reach = EdgeReach::new(surface, capture, scale);
            view = reach.apply(corrected, step, hand);
            if view == capture {
                reached_wall = true;
                break;
            }
        }
        assert!(
            reached_wall,
            "view must reach the exact bottom wall, got {view}"
        );
        // Without the boost (old time-only correction) the same push stops
        // short of the wall by most of the residual.
        let mut old_view: f64 = 1200.0;
        let mut hand2: f64 = 1000.0;
        let mut old_reached = false;
        while hand2 < 2000.0 {
            let step = 16.0;
            hand2 += step;
            let nx = (old_view + step * scale).clamp(0.0, capture);
            let offset = nx - hand2 * scale;
            let corrected = if offset.abs() > 0.5 {
                // Time-only healing, no travel boost.
                let f = offset_correction_factor(0.016);
                let lim = step * scale * 2.0;
                let corr = (offset * f).clamp(-lim, lim);
                (hand2 * scale + (offset - corr)).clamp(0.0, capture)
            } else {
                nx
            };
            old_view = corrected;
            if old_view == capture {
                old_reached = true;
                break;
            }
        }
        assert!(
            !old_reached && old_view < capture,
            "old behavior stopped short (view was {old_view})"
        );
    }

    #[test]
    fn zoom_keys_are_percentages_of_max_zoom() {
        let config = MagnifierConfig {
            max_zoom: 18.0,
            ..MagnifierConfig::default()
        };
        let mut state = MagnifierState::new(config, Some(3.0));
        state.handle_zoom_key(9);
        assert!(
            (state.zoom - 18.0).abs() < 1e-9,
            "key 9 = max, got {}",
            state.zoom
        );
        state.handle_zoom_key(1);
        assert!(
            (state.zoom - 2.0).abs() < 1e-9,
            "key 1 = max/9, got {}",
            state.zoom
        );
        state.handle_zoom_key(5);
        assert!(
            (state.zoom - 10.0).abs() < 1e-9,
            "key 5 = max*5/9, got {}",
            state.zoom
        );
    }

    #[test]
    fn zoom_keys_never_go_below_1x() {
        // With max_zoom < 9 the lowest keys would compute to sub-1x; they must
        // be clamped to the 1x minimum instead.
        let config = MagnifierConfig {
            max_zoom: 4.0,
            ..MagnifierConfig::default()
        };
        let mut state = MagnifierState::new(config, Some(3.0));
        state.handle_zoom_key(1);
        assert_eq!(
            state.zoom, 1.0,
            "key 1 must clamp to 1x, got {}",
            state.zoom
        );
        state.handle_zoom_key(9);
        assert_eq!(state.zoom, 4.0, "key 9 is the max, got {}", state.zoom);
    }

    #[test]
    fn default_max_zoom_keeps_historical_levels() {
        // With the default max zoom of 9, keys 1-9 map to 1x..9x exactly.
        let config = MagnifierConfig::default();
        let mut state = MagnifierState::new(config, Some(3.0));
        for key in 1..=9u8 {
            state.handle_zoom_key(key);
            assert!(
                (state.zoom - key as f64).abs() < 1e-9,
                "key {key} should be {key}x, got {}",
                state.zoom
            );
        }
    }

    #[test]
    fn reset_zoom_returns_to_configured_default() {
        let config = MagnifierConfig {
            default_zoom: Some(4.0),
            ..MagnifierConfig::default()
        };
        let mut state = MagnifierState::new(config, Some(7.0));
        state.handle_zoom_key(9);
        state.reset_zoom();
        assert!(
            (state.zoom - 4.0).abs() < 1e-9,
            "reset to default, got {}",
            state.zoom
        );
    }

    /// Regression test for the magnified-cursor blit: `draw_cursor_at` must
    /// draw the reticle (white ring + black center) at the requested position
    /// without mirroring, and never write outside the canvas.
    fn canvas_at(canvas: &[u8], stride: i32, x: i32, y: i32) -> Option<[u8; 4]> {
        if x < 0 || y < 0 || x >= stride / 4 || y >= canvas.len() as i32 / stride {
            return None;
        }
        let i = (y as usize * stride as usize + x as usize * 4) as usize;
        Some([canvas[i], canvas[i + 1], canvas[i + 2], canvas[i + 3]])
    }
    #[test]
    fn draw_cursor_at_blits_ring_and_center_unmirrored() {
        let mut canvas = vec![0u8; 64 * 64 * 4];
        let stride = 64 * 4;
        let (sprite, (hx, hy)) = crate::cursor::MagnifiedCursor::from_reticle(1.0).sprite(1.0);
        let hotspot = (hx.round() as i32, hy.round() as i32);

        MagnifierWindow::draw_cursor_at(&mut canvas, stride, (32, 32), &sprite, hotspot);

        // Black center lands at the requested spot (reticle hotspot is center).
        assert_eq!(canvas_at(&canvas, stride, 32, 32), Some([0, 0, 0, 255]));
        // White ring is present (sprite is not a plain black square).
        assert_eq!(
            canvas_at(&canvas, stride, 32 + 5, 32),
            Some([255, 255, 255, 255])
        );
        // Corners of the sprite footprint stay untouched (transparent region).
        assert_eq!(
            canvas_at(&canvas, stride, 32 + 7, 32 + 7),
            Some([0, 0, 0, 0])
        );
    }

    #[test]
    fn draw_cursor_at_moves_with_position_without_mirroring() {
        let mut canvas = vec![0u8; 64 * 64 * 4];
        let stride = 64 * 4;
        let (sprite, (hx, hy)) = crate::cursor::MagnifiedCursor::from_reticle(1.0).sprite(1.0);
        let hotspot = (hx.round() as i32, hy.round() as i32);

        MagnifierWindow::draw_cursor_at(&mut canvas, stride, (40, 32), &sprite, hotspot);
        // Center moved +8 in x: the pixel at 40 must be the black center, and
        // the mirrored destination (24) must stay untouched.
        assert_eq!(canvas_at(&canvas, stride, 40, 32), Some([0, 0, 0, 255]));
        assert_eq!(canvas_at(&canvas, stride, 24, 32), Some([0, 0, 0, 0]));
    }

    #[test]
    fn draw_cursor_at_places_hotspot_at_pos() {
        let mut canvas = vec![0u8; 64 * 64 * 4];
        let stride = 64 * 4;
        // A synthetic 2x2 sprite with a green pixel at (0, 0) and a red
        // hotspot pixel at (1, 1).
        let mut base = crate::render::RgbaBuffer::new(2, 2);
        base.set_pixel(0, 0, [0, 255, 0, 255]);
        base.set_pixel(1, 1, [255, 0, 0, 255]);
        let mut cursor = crate::cursor::MagnifiedCursor::from_parts_for_test(base, (1.0, 1.0));
        let (sprite, (hx, hy)) = cursor.sprite(1.0);
        let hotspot = (hx.round() as i32, hy.round() as i32);

        MagnifierWindow::draw_cursor_at(&mut canvas, stride, (50, 50), &sprite, hotspot);
        // The hotspot pixel of the sprite lands exactly on pos.
        assert_eq!(canvas_at(&canvas, stride, 50, 50), Some([255, 0, 0, 255]));
        // The rest of the sprite extends up-left of the hotspot.
        assert_eq!(canvas_at(&canvas, stride, 49, 49), Some([0, 255, 0, 255]));
    }

    #[test]
    fn draw_cursor_at_clamps_at_canvas_edges_without_panicking() {
        let mut canvas = vec![0u8; 64 * 64 * 4];
        let stride = 64 * 4;
        let (sprite, (hx, hy)) = crate::cursor::MagnifiedCursor::from_reticle(1.0).sprite(1.0);
        let hotspot = (hx.round() as i32, hy.round() as i32);

        // Sprite center at each corner: most of the sprite is clipped, but the
        // blit must not panic or write out of bounds.
        for pos in [(0, 0), (0, 63), (63, 0), (63, 63)] {
            canvas.fill(0);
            MagnifierWindow::draw_cursor_at(&mut canvas, stride, pos, &sprite, hotspot);
        }
        // A fully out-of-view sprite is a no-op.
        MagnifierWindow::draw_cursor_at(&mut canvas, stride, (-50, -50), &sprite, hotspot);
    }
}
