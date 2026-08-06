#![allow(dead_code)]

use std::num::NonZeroU32;
use std::path::PathBuf;

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
    wl_callback, wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface,
};
use wayland_client::{Connection, QueueHandle};

use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::Flags, zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::capture::CaptureManager;
use crate::capture::ScreenshotRegion;
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

/// The edge-reach margin scales with the pointer's recent *peak* travel
/// (see [`TravelHistory`] and [`reach_margin`]): when the pointer is moved
/// fast, its delivered position can stop short of the physical edge — the
/// gap is the travel the hand covered *after* the last delivered sample,
/// which was accumulated at the **peak** speed of the flick, frames before
/// the hand decelerates. Sizing the margin from the current event's own
/// (decelerating) travel therefore under-covers the gap exactly when the
/// pointer reaches the edge region, which is why fast flicks stopped
/// “arbitrarily” short while slow motion always reached. The margin is
/// `peak × REACH_DELTA_FACTOR + REACH_FLOOR_LOGICAL`: small while moving
/// slowly (no magnetic wall — parking stays precise) and large exactly
/// when the delivery gap is large (the wall is always reachable, at any
/// speed). Capped so an extreme flick can never magnetize a huge area.
const REACH_DELTA_FACTOR: f64 = 1.5;
/// Floor (logical px): even a sub-pixel crawl to the edge lands the view
/// exactly on the wall, so the exact border is always reachable.
const REACH_FLOOR_LOGICAL: f64 = 8.0;
/// Cap (logical px) on the reach margin: bounds the magnetic zone even for
/// an extreme motion burst.
const REACH_MAX_LOGICAL: f64 = 200.0;
/// Extra view-side slack (logical px) beyond the reach margin: the view may
/// still sit slightly short of the wall from a still-healing residual; the
/// slack lets the reach close that too without teleporting across a large
/// residual.
const REACH_VIEW_SLACK_LOGICAL: f64 = 8.0;
/// How many of the most recent per-event pointer travels are kept per axis
/// to estimate the approach speed for the edge reach. Long enough to cover
/// a delivery staleness of a few frames, short enough that a slow crawl
/// right after a fast phase loses the inflated margin within a few events.
const REACH_HISTORY_LEN: usize = 4;

/// App-side key repeat for held Screenshot-Mode nudge keys: the delay before
/// the first repeat fires, then the interval between repeats (~30 Hz). The
/// repeat is driven by the **event loop itself** (a bounded poll timeout in
/// `dispatch_with_timeout`, plus `fire_repeat_nudges` in `draw_frame`)
/// because the compositor (niri et al.) may not send `wl_keyboard`
/// repeated-key events to clients using a manual event loop (sctk 0.21's
/// repeat engine needs a calloop `LoopHandle`, which this app does not run),
/// and frame callbacks proved unreliable as a repeat clock on niri.
const NUDGE_REPEAT_DELAY_MS: u64 = 400;
const NUDGE_REPEAT_INTERVAL_MS: u64 = 33;
/// Minimum interval between redraws caused by pointer motion (8.33 ms ≈
/// 120 Hz). Pointer events arrive at the libinput sample rate — up to
/// ~1000 Hz for high-polling mice — and redrawing + presenting a full-screen
/// frame for each one floods the compositor with commits. When other
/// surfaces underneath also repaint (a web browser with hover effects,
/// animations or video), the compositor's frame scheduling starves and the
/// magnifier's panning visibly lags, whereas over an idle surface (e.g.
/// Blender's viewport) there is no competition and it feels snappy. The
/// display presents at 60–144 Hz anyway, so capping the redraw rate at a
/// fraction of the event rate keeps panning just as smooth while cutting
/// compositor load by an order of magnitude.
const MOTION_REDRAW_INTERVAL: std::time::Duration = std::time::Duration::from_micros(8333);
/// Whether a motion-driven redraw is due: always for the first draw (no
/// previous draw), otherwise once [`MOTION_REDRAW_INTERVAL`] has elapsed
/// since `last_draw_at`.
fn motion_redraw_due(last_draw_at: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    match last_draw_at {
        None => true,
        Some(last) => now.duration_since(last) >= MOTION_REDRAW_INTERVAL,
    }
}

/// The reach margin (logical px) for one axis, sized from the pointer's
/// recent peak travel: `peak × REACH_DELTA_FACTOR + REACH_FLOOR_LOGICAL`,
/// capped at [`REACH_MAX_LOGICAL`].
fn reach_margin(peak: f64) -> f64 {
    (peak * REACH_DELTA_FACTOR + REACH_FLOOR_LOGICAL).min(REACH_MAX_LOGICAL)
}

/// Next zoom for the scroll wheel in `Levels` mode. Walks the same discrete
/// levels as the `1`–`9` keys (level *i* = `max × i/9`), extended with a
/// **level 0 at the runtime minimum** (1×, or the fully-zoomed-out view when
/// `min_zoom` allows), so the most zoomed-out level is always reachable with
/// the wheel — the bare key levels miss it whenever `max/9 > min` (e.g. max
/// 12 with a 1× minimum used to bottom out at 1.33×). On a level it steps to
/// the neighbour; off a level it snaps to the next level in the wheel
/// direction. `steps` is the already direction-corrected wheel delta:
/// positive zooms in, negative zooms out. The result never leaves
/// `min_zoom..=max_zoom`.
fn wheel_levels_next(zoom: f64, min_zoom: f64, max_zoom: f64, steps: f64) -> f64 {
    // Below the wheel's floor (only reachable via the `0` key when 0 % zoom
    // is not allowed): zooming out stays put (the view is already fully
    // zoomed out) instead of snapping back up, and zooming in returns to the
    // floor — no direction-reversed jump.
    if zoom < min_zoom {
        return if steps < 0.0 { zoom } else { min_zoom };
    }
    const LEVELS: f64 = 9.0;
    // Index of the current zoom in the level space: at/below the midpoint
    // between the minimum and level 1 it counts as level 0; otherwise it
    // rounds into 1..=LEVELS (clamped so a zoom beyond the current max snaps
    // to the top level instead of walking backwards through the wheel).
    let level_1 = max_zoom / LEVELS;
    let idx_f = if zoom <= (min_zoom + level_1) / 2.0 {
        0.0
    } else {
        ((zoom / max_zoom) * LEVELS).clamp(1.0, LEVELS)
    };
    let idx = idx_f.round();
    let on_level = (idx_f - idx).abs() < 1e-6;
    let next = if on_level {
        // Exactly on a level: step to the neighbour in the wheel direction.
        if steps > 0.0 {
            (idx + 1.0).min(LEVELS)
        } else {
            (idx - 1.0).max(0.0)
        }
    } else if steps > 0.0 {
        // Between levels: snap to the next level in the wheel direction.
        idx_f.ceil().min(LEVELS)
    } else {
        idx_f.floor().max(0.0)
    };
    if next == 0.0 {
        min_zoom
    } else {
        (max_zoom * next / LEVELS).max(min_zoom)
    }
}

/// The zoom at which the **whole captured screen exactly fills the viewport**
/// — the "fully zoomed out" / "0 %" view (the `0` key and, when
/// `allow_zero_zoom` is on, the wheel/keys/hold-to-zoom all end here). It is
/// `1 / max(capture-per-viewport-pixel scale)` per axis, so the limiting axis
/// shows the entire capture edge-to-edge and no axis leaves black bars, and
/// it never exceeds 1× (zooming out past it would make the screen smaller
/// than the viewport — the broken look the user reported).
fn fit_zoom(capture: (f64, f64), viewport: (f64, f64)) -> f64 {
    let sx = capture.0 / viewport.0.max(1.0);
    let sy = capture.1 / viewport.1.max(1.0);
    (1.0 / sx.max(sy)).min(1.0)
}

/// The zoom readout shown in the OSD: **`0x`** at the fully-zoomed-out view
/// (the whole captured screen filling the viewport — the state the `0` key,
/// and with `allow_zero_zoom` the wheel/keys/hold-to-zoom, end at), otherwise
/// the plain factor (e.g. `3.00x`). The minimum reads as `0x` so the most-
/// zoomed-out state is unambiguous: the user asked for the zoom to be able to
/// *reach 0*, and it should read as such. (`0%` was tried first, but the
/// built-in bitmap font's `%` glyph was illegible — it read like a `2`.) Only
/// shown when the fit zoom is genuinely below 1× (real zoom-out headroom
/// exists) — on a setup where the whole screen already fills the viewport at
/// 1× (`fit == 1`) the `1` key would otherwise read as "0x".
fn zoom_readout(zoom: f64, fit: f64) -> String {
    if fit < 1.0 && (zoom - fit).abs() < 1e-9 {
        "0x".to_string()
    } else {
        format!("{zoom:.2}x")
    }
}

/// Snap a capture-px position to the nearest whole capture pixel. The
/// screenshot selection must align with the pixel grid of the frozen frame
/// — the grid the user sees as the magnified pixels (each magnified block
/// is one capture px scaled by the zoom) — so the drag anchor and live
/// corner are snapped here and the resulting rectangle is always
/// integer-valued. That makes the saved crop exactly match the visually
/// selected region: the save path rounds independently, and an un-snapped
/// fractional rect could otherwise crop up to a pixel off from what was
/// shown on screen.
fn snap_capture_px(pos: (f64, f64)) -> (f64, f64) {
    (pos.0.round(), pos.1.round())
}

/// Normalize a screenshot drag (anchor, live pointer) into an axis-aligned
/// selection rectangle in capture px, clamped to the capture bounds with a
/// minimum size of 1 px per axis (a plain click without drag still yields a
/// valid, nudgeable rectangle). Callers pass [`snap_capture_px`] values so
/// the rectangle stays aligned to the magnified pixel grid.
fn normalize_screenshot_rect(
    p0: (f64, f64),
    p1: (f64, f64),
    bounds: (f64, f64),
) -> (f64, f64, f64, f64) {
    // The bounds are the capture size (always >= 1 px), but guard the clamps
    // anyway so degenerate bounds can never panic `f64::clamp`.
    let bx = bounds.0.max(1.0);
    let by = bounds.1.max(1.0);
    let x0 = p0.0.min(p1.0).clamp(0.0, bx - 1.0);
    let y0 = p0.1.min(p1.1).clamp(0.0, by - 1.0);
    let x1 = p0.0.max(p1.0).clamp(1.0, bx).max(x0 + 1.0).min(bx);
    let y1 = p0.1.max(p1.1).clamp(1.0, by).max(y0 + 1.0).min(by);
    (x0, y0, x1, y1)
}

/// One edge of the screenshot selection rectangle. The border under the
/// magnified cursor (its 'click anchor' — the viewport center, which is where
/// the cursor sprite sits) is the one WASD nudges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenshotBorder {
    Left,
    Right,
    Top,
    Bottom,
}

/// The screenshot-mode overlay cache key: whether the mode is active, the
/// selection rect (capture px) the overlay was filled for, and the active
/// border highlighted at that time. Rebuilding on rect *or* active-border
/// change keeps the highlight honest while plain pointer motion (which
/// rarely flips the active border) stays smooth.
type ScreenshotOverlayState = (bool, Option<(f64, f64, f64, f64)>, Option<ScreenshotBorder>);

/// Distance from `(px, py)` to the line segment `(ax, ay)`–`(bx, by)`.
/// Used for border detection so the *visible* drawn edge (a finite segment)
/// is what counts, not an infinite line through it.
fn dist_point_to_segment(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = bx - ax;
    let dy = by - ay;
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

/// The selection edge closest to `point` (capture px). Distances are measured
/// to the **visible border segments** (the four drawn edges of the rectangle,
/// each a finite line between two corners) — not to infinite lines through
/// the edges — so for a wide rectangle a cursor far to the right selects the
/// short right edge, not the long top/bottom edges. Ties are broken
/// deterministically in the order left < right < top < bottom.
fn active_screenshot_border(rect: (f64, f64, f64, f64), point: (f64, f64)) -> ScreenshotBorder {
    let (x0, y0, x1, y1) = rect;
    let (px, py) = point;
    let dl = dist_point_to_segment(px, py, x0, y0, x0, y1); // left edge
    let dr = dist_point_to_segment(px, py, x1, y0, x1, y1); // right edge
    let dt = dist_point_to_segment(px, py, x0, y0, x1, y0); // top edge
    let db = dist_point_to_segment(px, py, x0, y1, x1, y1); // bottom edge
    let mut best = ScreenshotBorder::Left;
    let mut best_d = dl;
    if dr < best_d {
        best = ScreenshotBorder::Right;
        best_d = dr;
    }
    if dt < best_d {
        best = ScreenshotBorder::Top;
        best_d = dt;
    }
    if db < best_d {
        best = ScreenshotBorder::Bottom;
    }
    best
}

/// Nudge the selection by 1 capture px. The **active border** (always the one
/// closest to the cursor — no proximity requirement) is moveable in all four
/// WASD directions: on its own axis the key moves the border (W/S move
/// horizontal top/bottom edges up/down, A/D move vertical left/right edges
/// left/right), and **off-axis the whole rectangle translates** along the
/// key's direction (A/D on a horizontal border move the whole rect
/// horizontally, W/S on a vertical border move it vertically). The rectangle
/// never collapses below 1 px per axis, never leaves the capture bounds, and
/// a translate never shrinks it (it simply doesn't move at the edge).
fn nudge_screenshot_border(
    rect: (f64, f64, f64, f64),
    border: ScreenshotBorder,
    key: char,
    bounds: (f64, f64),
) -> (f64, f64, f64, f64) {
    let (mut x0, mut y0, mut x1, mut y1) = rect;
    match (border, key) {
        // On-axis: move the border itself.
        (ScreenshotBorder::Top, 'w') => y0 = (y0 - 1.0).max(0.0),
        (ScreenshotBorder::Top, 's') => y0 = (y0 + 1.0).min(y1 - 1.0),
        (ScreenshotBorder::Bottom, 'w') => y1 = (y1 - 1.0).max(y0 + 1.0),
        (ScreenshotBorder::Bottom, 's') => y1 = (y1 + 1.0).min(bounds.1),
        (ScreenshotBorder::Left, 'a') => x0 = (x0 - 1.0).max(0.0),
        (ScreenshotBorder::Left, 'd') => x0 = (x0 + 1.0).min(x1 - 1.0),
        (ScreenshotBorder::Right, 'a') => x1 = (x1 - 1.0).max(x0 + 1.0),
        (ScreenshotBorder::Right, 'd') => x1 = (x1 + 1.0).min(bounds.0),
        // Off-axis: translate the whole rectangle along the key's direction
        // (only when it stays fully inside the capture, so it never shrinks).
        (ScreenshotBorder::Top | ScreenshotBorder::Bottom, 'a') if x0 - 1.0 >= 0.0 => {
            x0 -= 1.0;
            x1 -= 1.0;
        }
        (ScreenshotBorder::Top | ScreenshotBorder::Bottom, 'd') if x1 + 1.0 <= bounds.0 => {
            x0 += 1.0;
            x1 += 1.0;
        }
        (ScreenshotBorder::Left | ScreenshotBorder::Right, 'w') if y0 - 1.0 >= 0.0 => {
            y0 -= 1.0;
            y1 -= 1.0;
        }
        (ScreenshotBorder::Left | ScreenshotBorder::Right, 's') if y1 + 1.0 <= bounds.1 => {
            y0 += 1.0;
            y1 += 1.0;
        }
        _ => {}
    }
    (x0, y0, x1, y1)
}

/// Fill one solid rectangle of pixels (RGBA `px`) in an overlay/canvas.
/// Clamped to the buffer; empty when `x1 <= x0` or `y1 <= y0`.
#[allow(clippy::too_many_arguments)]
fn fill_px(
    ov: &mut [u8],
    width: i32,
    height: i32,
    x0: i32,
    x1: i32,
    y0: i32,
    y1: i32,
    px: [u8; 4],
) {
    let x0 = x0.clamp(0, width);
    let x1 = x1.clamp(0, width);
    let y0 = y0.clamp(0, height);
    let y1 = y1.clamp(0, height);
    // A degenerate or inverted range (e.g. a selection narrower than its own
    // border) is a no-op — never a `ov[start..end]` panic.
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    for y in y0..y1 {
        let row = (y as usize) * (width as usize) * 4;
        let start = row + (x0 as usize) * 4;
        let end = row + (x1 as usize) * 4;
        for chunk in ov[start..end].chunks_exact_mut(4) {
            chunk.copy_from_slice(&px);
        }
    }
}

/// Fill a screenshot-mode overlay buffer (RGBA, tightly packed `width*4` rows)
/// with the selection scrim + border: pixels inside the selection are
/// transparent, the `border_px`-thick edge frame is the selection color, and
/// everything outside is a dim translucent black so the selection pops. The
/// **active border** (the one WASD nudges — always the edge closest to the
/// cursor, no proximity requirement) is drawn thicker and lightened so it is
/// obvious which edge will move. With no rectangle yet (`rect == None`) a
/// light full-screen scrim signals the mode. `rect` is in the overlay's own
/// pixel coordinates (logical px on the CPU path, RENDER_SCALE-scaled on the
/// GPU path). Filled with row segments (not per-pixel branches) so rebuilding
/// the overlay after a nudge or drag is cheap.
/// Flip the effective screenshot scale (real size <-> magnified).
fn toggle_screenshot_scale(mode: crate::config::ScreenshotScale) -> crate::config::ScreenshotScale {
    match mode {
        crate::config::ScreenshotScale::Real => crate::config::ScreenshotScale::Magnified,
        crate::config::ScreenshotScale::Magnified => crate::config::ScreenshotScale::Real,
    }
}

/// Advance a repeat deadline past `now` by whole `interval` steps (drift-free
/// cadence): repeats land on a steady grid even when frames arrive late, and
/// a burst of late frames can never fire more than one repeat per interval.
fn advance_repeat_deadline(
    now: std::time::Instant,
    next_at: std::time::Instant,
    interval: std::time::Duration,
) -> std::time::Instant {
    let mut deadline = next_at;
    while now >= deadline {
        deadline += interval;
    }
    deadline
}

fn fill_screenshot_overlay(
    overlay: &mut [u8],
    width: i32,
    height: i32,
    rect: Option<(f64, f64, f64, f64)>,
    color: [u8; 3],
    border_px: i32,
    active_border: Option<ScreenshotBorder>,
) {
    let b = border_px.max(1);
    if rect.is_none() {
        // No selection yet: nothing to draw — the magnified screen must look
        // exactly as it did in the magnifier (opaque, undimmed). Still clear
        // the whole buffer so no stale pixels from an earlier selection
        // survive (the buffer is reused across rebuilds).
        fill_px(overlay, width, height, 0, width, 0, height, [0, 0, 0, 0x00]);
        return;
    }
    let (rx0, ry0, rx1, ry1) = rect.unwrap();
    let (rx0, ry0, rx1, ry1) = (
        rx0.round() as i32,
        ry0.round() as i32,
        rx1.round() as i32,
        ry1.round() as i32,
    );
    // Fully transparent scrim: the magnified screen stays opaque and
    // undimmed everywhere — only the opaque border ring marks the selection.
    // The full-buffer transparent reset doubles as the "transparent interior"
    // (the rect area is never touched again except by border bands).
    let scrim = [0u8, 0, 0, 0x00];
    let border = [color[0], color[1], color[2], 0xFF];
    // The active border is thicker and lightened 60 % toward white.
    let active = [
        (color[0] as u16 + (255 - color[0] as u16) * 3 / 5) as u8,
        (color[1] as u16 + (255 - color[1] as u16) * 3 / 5) as u8,
        (color[2] as u16 + (255 - color[2] as u16) * 3 / 5) as u8,
        0xFF,
    ];
    let ab = b * 2;
    let (top_w, bot_w, left_w, right_w) = (
        if active_border == Some(ScreenshotBorder::Top) {
            ab
        } else {
            b
        },
        if active_border == Some(ScreenshotBorder::Bottom) {
            ab
        } else {
            b
        },
        if active_border == Some(ScreenshotBorder::Left) {
            ab
        } else {
            b
        },
        if active_border == Some(ScreenshotBorder::Right) {
            ab
        } else {
            b
        },
    );
    // Fill order: a full-screen scrim, then the transparent interior, then
    // the border ring around the rect. Every band is bounded by the rect's
    // own edges (`rx0..rx1`, `ry0..ry1`) — never full-width strips — so a
    // tiny selection (e.g. the 1px rect a click produces) renders as a small
    // outlined box instead of lines spanning the whole screen, and the
    // interior fill can never invert (crash) when the rect is narrower than
    // its border.
    // 1. Reset the whole buffer (transparent) — this is also the selection's
    //    transparent interior.
    fill_px(overlay, width, height, 0, width, 0, height, scrim);
    // 2. Border ring: top / bottom / left / right bands, active edge thicker
    // and lightened toward white. The bands of the **active** edge are drawn
    // last so they win the corner pixels where two bands meet.
    let (top_px, bot_px, left_px, right_px) = (
        if active_border == Some(ScreenshotBorder::Top) {
            active
        } else {
            border
        },
        if active_border == Some(ScreenshotBorder::Bottom) {
            active
        } else {
            border
        },
        if active_border == Some(ScreenshotBorder::Left) {
            active
        } else {
            border
        },
        if active_border == Some(ScreenshotBorder::Right) {
            active
        } else {
            border
        },
    );
    // Horizontal (top/bottom) bands first, vertical (left/right) after — so a
    // vertical active edge wins its corners; then, when the active edge is
    // horizontal, redraw the horizontal bands so they win theirs.
    fill_px(overlay, width, height, rx0, rx1, ry0, ry0 + top_w, top_px);
    fill_px(overlay, width, height, rx0, rx1, ry1 - bot_w, ry1, bot_px);
    fill_px(overlay, width, height, rx0, rx0 + left_w, ry0, ry1, left_px);
    fill_px(
        overlay,
        width,
        height,
        rx1 - right_w,
        rx1,
        ry0,
        ry1,
        right_px,
    );
    match active_border {
        Some(ScreenshotBorder::Top) => {
            fill_px(overlay, width, height, rx0, rx1, ry0, ry0 + top_w, top_px);
        }
        Some(ScreenshotBorder::Bottom) => {
            fill_px(overlay, width, height, rx0, rx1, ry1 - bot_w, ry1, bot_px);
        }
        _ => {}
    }
}

/// Nearest-neighbor upscale a tightly packed RGBA buffer by `scale` (>= 1).
/// Returns `(data, new_width, new_height)`. Used to save a magnified
/// screenshot: the selected region scaled to the current zoom, matching the
/// crisp pixelated look of the magnifier.
fn upscale_nearest(src: &[u8], w: u32, h: u32, scale: f64) -> (Vec<u8>, u32, u32) {
    let nw = ((w as f64) * scale).round().max(1.0) as u32;
    let nh = ((h as f64) * scale).round().max(1.0) as u32;
    let mut out = vec![0u8; (nw as usize) * (nh as usize) * 4];
    for y in 0..nh {
        let sy = ((y as f64) / scale) as usize;
        for x in 0..nw {
            let sx = ((x as f64) / scale) as usize;
            let si = (sy * w as usize + sx) * 4;
            let di = (y as usize * nw as usize + x as usize) * 4;
            out[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    (out, nw, nh)
}

/// Alpha-blend a screenshot overlay buffer (tightly packed `width*4` rows)
/// into an RGBA canvas in place (the CPU fallback path; on the GPU path the
/// overlay is uploaded as a texture and the sprite shader blends it).
fn blend_overlay_into(canvas: &mut [u8], stride: i32, overlay: &[u8], width: i32, height: i32) {
    for y in 0..height {
        for x in 0..width {
            let di = (y as usize * stride as usize + x as usize) * 4;
            let si = (y as usize * width as usize + x as usize) * 4;
            let a = overlay[si + 3] as f32 / 255.0;
            if a <= 0.0 {
                continue;
            }
            if a >= 1.0 {
                canvas[di..di + 4].copy_from_slice(&overlay[si..si + 4]);
            } else {
                for c in 0..3 {
                    canvas[di + c] = (overlay[si + c] as f32 * a
                        + canvas[di + c] as f32 * (1.0 - a))
                        .round() as u8;
                }
                canvas[di + 3] = 255;
            }
        }
    }
}

/// Rolling window of the most recent per-axis pointer travels (logical px
/// per event), used to size the edge-reach margin. The compositor's last
/// delivered pointer position can lag the hand's true stop by up to a few
/// frames — a gap accumulated at the *peak* speed of a flick, before the
/// hand decelerates — so the margin tracks the recent **peak** travel, not
/// the current (decelerating) event's travel: the wall stays reachable
/// through the whole approach. Because the window is short and is flushed
/// after a pause, a slow crawl never inherits an inflated margin (no
/// magnetic wall while parking).
#[derive(Clone, Copy)]
struct TravelHistory {
    buf: [(f64, f64); REACH_HISTORY_LEN],
    next: usize,
    filled: usize,
}

impl TravelHistory {
    fn new() -> Self {
        Self {
            buf: [(0.0, 0.0); REACH_HISTORY_LEN],
            next: 0,
            filled: 0,
        }
    }

    /// Record one event's travel (logical px per axis).
    fn push(&mut self, d: (f64, f64)) {
        self.buf[self.next] = d;
        self.next = (self.next + 1) % REACH_HISTORY_LEN;
        self.filled = (self.filled + 1).min(REACH_HISTORY_LEN);
    }

    /// The largest |travel| per axis in the window. Zero until the first
    /// event is pushed (gliding never inflates the margin).
    fn peak(&self) -> (f64, f64) {
        let mut px: f64 = 0.0;
        let mut py: f64 = 0.0;
        for &(x, y) in &self.buf[..self.filled] {
            px = px.max(x.abs());
            py = py.max(y.abs());
        }
        (px, py)
    }
}

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
    /// the view short of the wall. This closes that gap: `margin` is sized
    /// by the caller from the pointer's recent **peak** travel (see
    /// [`TravelHistory`] / [`reach_margin`]), because the delivery gap was
    /// accumulated at the peak speed of the flick, before the hand
    /// decelerated. So a slow push near the edge keeps a small margin (no
    /// magnetic wall — you can park anywhere), while a fast flick toward
    /// the edge gets a margin large enough to bridge the gap and land the
    /// view **exactly** on the wall, at any speed. The view must already be
    /// within the (scaled) margin of the wall so it never teleports across
    /// a large still-healing residual. Pushing away, gliding
    /// (`delta_logical == 0`), or being away from the edge never triggers
    /// it, and the result never leaves the capture. `view` is in capture
    /// px; `pointer` is in logical px.
    fn apply(self, view: f64, delta_logical: f64, pointer: f64, margin: f64) -> f64 {
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
/// Linux input event code for the left mouse button (draws the screenshot
/// selection rectangle in Screenshot Mode).
const BTN_LEFT: u32 = 0x110;
/// How long the "Saved …" screenshot heads-up stays in the OSD legend.
const SCREENSHOT_NOTICE_SECS: u64 = 4;
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
        let min_zoom = config.min_zoom();
        let zoom = initial_zoom
            .unwrap_or_else(|| config.default_zoom.unwrap_or(3.0))
            .clamp(min_zoom, max_zoom);
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
    /// least the configured minimum (1× by default; with `max_zoom < 9` the
    /// lower keys would otherwise go sub-1×, e.g. 0.44× at `max_zoom = 4`;
    /// with `allow_zero_zoom` enabled they may go below 1×, down to the
    /// minimum).
    fn zoom_for_level(&self, key: u8) -> f64 {
        let max_zoom = self.config.max_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        (max_zoom * (key as f64) / 9.0).clamp(self.config.min_zoom(), max_zoom)
    }

    /// State-level core for the numeric zoom keys (also the unit-test target;
    /// the runtime path in `MagnifierWindow::press_key` applies the
    /// fully-zoomed-out fit on top via [`MagnifierWindow::set_zoom`], so this
    /// clamps only to the config minimum, not the capture-dependent fit).
    pub fn handle_zoom_key(&mut self, key: u8) {
        if key == 0 {
            // State-level: the `0` key selects the minimum (0), which the
            // engine maps at runtime to the fully-zoomed-out view (the whole
            // captured screen filling the viewport) — always available,
            // regardless of the allow-zero setting.
            self.zoom = 0.0;
            self.renderer.update_scale_factor(0.0);
            tracing::info!("Zoom set to {}", self.zoom);
        } else if (1..=9).contains(&key) {
            self.zoom = self.zoom_for_level(key);
            self.renderer.update_scale_factor(self.zoom);
            tracing::info!("Zoom set to {}", self.zoom);
        }
    }

    /// State-level core for reset-to-default (also the unit-test target; the
    /// runtime path applies the fully-zoomed-out fit on top via
    /// [`MagnifierWindow::set_zoom`], so a default of 0 % lands on the whole-
    /// screen view). Used by the middle mouse button and the `reset_zoom`
    /// keybinding.
    pub fn reset_zoom(&mut self) {
        let max_zoom = self.config.max_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let min_zoom = self.config.min_zoom();
        let default_zoom = self
            .config
            .default_zoom
            .unwrap_or(3.0)
            .clamp(min_zoom, max_zoom);
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
    /// A motion-driven redraw was throttled and is waiting for the next
    /// [`MOTION_REDRAW_INTERVAL`] deadline (see [`MagnifierWindow::request_motion_redraw`]).
    redraw_pending: bool,
    /// When the last motion-driven redraw happened; used to cap the redraw
    /// rate so high-frequency pointer events cannot flood the compositor.
    last_draw_at: Option<std::time::Instant>,
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
    /// Whether Screenshot Mode is active (entered with the `screenshot_manual`
    /// key, default `S`; `F` enters it with the whole screen pre-selected).
    /// While active the user drags a selection rectangle over the frozen
    /// frame (LMB), nudges its nearest border with WASD, selects the whole
    /// screen with `F`, saves with Return and cancels with Esc/Q/RMB.
    screenshot_active: bool,
    /// Whether the left mouse button is currently held to draw the selection.
    screenshot_dragging: bool,
    /// The drag anchor in capture px (where the LMB was pressed); the
    /// selection rectangle spans from here to the live pointer position.
    screenshot_drag_start: Option<(f64, f64)>,
    /// The settled selection rectangle in capture px, normalized
    /// `(x0, y0, x1, y1)` with `x0 <= x1` and `y0 <= y1`, clamped to the
    /// frozen capture. `None` while in Screenshot Mode with nothing selected.
    screenshot_rect: Option<(f64, f64, f64, f64)>,
    /// The screenshot save scale toggled while in Screenshot Mode (`None` =
    /// the configured `screenshot_scale` default). The toggle key flips it
    /// between real size and magnified; reset on every mode entry.
    effective_screenshot_scale: Option<crate::config::ScreenshotScale>,
    /// App-side key repeat for a held WASD nudge key in Screenshot Mode:
    /// `(key, next_deadline)`. While set, the event loop's `dispatch_with_timeout`
    /// wakes on the deadline and `draw_frame` fires repeat nudges on the
    /// cadence (no reliance on compositor repeat events or frame callbacks).
    /// Cleared on key release and on leaving the mode. Tracks only the most
    /// recent held nudge key (pressing a second key replaces the first;
    /// releasing any nudge key clears the hold) — one direction at a time,
    /// which is all nudging needs.
    nudge_hold: Option<(char, std::time::Instant)>,
    /// The magnified-cursor position offset (logical px) captured when
    /// Screenshot Mode is entered: the difference between the viewport center
    /// (where the cursor sprite sits) and the live pointer. The sprite then
    /// follows the pointer *plus* this offset, so pressing the screenshot key
    /// never jumps the cursor — it stays exactly where it was, and only
    /// relative pointer movement moves it.
    screenshot_cursor_offset: Option<(f64, f64)>,
    /// Transient OSD message (e.g. "Saved ~/Pictures/...") with the time it
    /// was set; shown in the legend for a few seconds.
    screenshot_notice: Option<(String, std::time::Instant)>,
    /// Cached screenshot-mode overlay buffer (dim + selection border). Only
    /// rebuilt when the screenshot-mode state changes (mode on/off or the
    /// selection rectangle) — plain pointer motion must NOT refill or
    /// re-upload it, or the mouse feels laggy in Screenshot Mode.
    screenshot_overlay: Option<RgbaBuffer>,
    /// The screenshot-mode state `(active, selection rect)` the cached overlay
    /// buffer was filled for; the overlay is rebuilt only when this changes.
    screenshot_overlay_state: Option<ScreenshotOverlayState>,
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
    /// Recent per-event pointer travels, sizing the edge-reach margin (see
    /// [`TravelHistory`]).
    reach_travel: TravelHistory,
    /// The (per-axis) pointer travel of the most recent motion event. The
    /// edge reach (see [`MagnifierWindow::apply_edge_reach`]) uses this
    /// stored direction instead of the current event's delta, so it also
    /// fires when the pointer has stopped short of the physical edge and no
    /// further motion events are delivered — the exact case where a
    /// per-event evaluation missed the final shortfall.
    last_motion_delta: Option<(f64, f64)>,
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
                    // The capture's dimensions define the fully-zoomed-out
                    // view: clamp the current zoom up to the runtime minimum
                    // (e.g. a default zoom of 0 % launches at the whole-screen
                    // view, never below it).
                    let min_zoom = state.runtime_min_zoom();
                    if state.state.zoom < min_zoom {
                        state.set_zoom(state.state.zoom, min_zoom);
                    }
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
            // Screenshot Mode: pointer input draws the selection rectangle
            // (LMB drag), cancels on RMB, and never pans the view (the
            // selection is in capture px; the view stays put).
            if self.screenshot_active {
                self.handle_screenshot_pointer(event, qh);
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
                        // Track the recent peak travel for the edge reach (see
                        // [`TravelHistory`] / [`MagnifierWindow::apply_edge_reach`]):
                        // the delivery gap is accumulated at the *peak* speed
                        // of a flick, so the reach margin must outlive the fast
                        // phase through the deceleration. A pause (a new burst
                        // of motion after a stop) flushes the window so an old
                        // flick can never make a later slow crawl magnetic.
                        if dt > 0.1 {
                            self.reach_travel = TravelHistory::new();
                        }
                        self.reach_travel.push((dx, dy));
                        // The edge reach itself is applied per rendered frame
                        // in [`MagnifierWindow::apply_edge_reach`] (draw_frame
                        // is the single choke point after any state change),
                        // using this stored direction.
                        self.last_motion_delta = Some((dx, dy));
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
                            let min_zoom = self.runtime_min_zoom();
                            let new_zoom = if self.state.zoom < min_zoom && dy_zoom > 0.0 {
                                // Below the floor (the 0 key with 0 % not
                                // allowed): zooming out stays put.
                                self.state.zoom
                            } else {
                                (self.state.zoom - dy_zoom * self.state.config.hold_to_zoom_speed)
                                    .clamp(min_zoom, max_zoom)
                            };
                            if (new_zoom - self.state.zoom).abs() > 1e-9 {
                                self.set_zoom(new_zoom, min_zoom);
                            }
                            if let Some((cx, cy)) = self.view_center {
                                let nx = self.clamp_to_capture((cx + dx * sx, cy)).0;
                                // The x edge reach is applied by the per-frame
                                // [`MagnifierWindow::apply_edge_reach`] on the
                                // next draw.
                                self.view_center = Some((nx, cy));
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
                            // The edge reach is applied by the per-frame
                            // [`MagnifierWindow::apply_edge_reach`] on the
                            // next draw, using the stored last-motion
                            // direction.
                            self.view_center = Some((fx, fy));
                        }
                        self.request_motion_redraw(qh);
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    // Right mouse button quits, same as Q.
                    if button == BTN_RIGHT {
                        self.exit = true;
                    }
                    // Middle mouse button resets the zoom to the default;
                    // the view stays put (zoom scales around the center). The
                    // runtime minimum applies, so a default of 0 % lands on
                    // the fully-zoomed-out view.
                    if button == BTN_MIDDLE {
                        let default_zoom = self.state.config.default_zoom.unwrap_or(3.0);
                        self.set_zoom(default_zoom, self.runtime_min_zoom());
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
                        let min_zoom = self.runtime_min_zoom();
                        let new_zoom = match self.state.config.scroll_zoom_mode {
                            crate::config::ScrollZoomMode::Levels => {
                                // The wheel walks the key levels extended with
                                // a level 0 at the runtime minimum (see
                                // [`wheel_levels_next`]): the most zoomed-out
                                // level is always reachable with the wheel.
                                wheel_levels_next(self.state.zoom, min_zoom, max_zoom, steps)
                            }
                            crate::config::ScrollZoomMode::Factor => {
                                if self.state.zoom < min_zoom && steps < 0.0 {
                                    // Below the floor (the 0 key with 0 % not
                                    // allowed): zooming out stays put.
                                    self.state.zoom
                                } else {
                                    (self.state.zoom * (1.0 + steps * WHEEL_ZOOM_STEP))
                                        .clamp(min_zoom, max_zoom)
                                }
                            }
                        };
                        if (new_zoom - self.state.zoom).abs() > 1e-9 {
                            self.set_zoom(new_zoom, min_zoom);
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
        self.reach_travel = TravelHistory::new();
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

        // Screenshot Mode: `F` selects the whole screen, Return saves, Esc/Q
        // cancel, WASD nudges the border under the cursor, and all other
        // magnifier keys are suspended. The config-window key cancels the
        // selection and falls through so the window still opens.
        if self.screenshot_active {
            if keysym_str == self.state.config.keybindings.config_window {
                self.exit_screenshot_mode();
            } else if keysym_str == self.state.config.keybindings.toggle_osd {
                // The Key Legend stays toggleable in Screenshot Mode.
                self.state.toggle_osd();
                self.draw_frame(qh);
                return;
            } else {
                self.handle_screenshot_key(event.keysym, &keysym_str);
                self.draw_frame(qh);
                return;
            }
        }

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
            "0" => Some(0),
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
            if zoom_level == 0 {
                // 0 = fully zoomed out: the whole captured screen fills the
                // viewport. Always available, regardless of the allow-zero
                // setting. min 0.0 = no lower clamp: fit_zoom is always > 0.
                self.set_zoom(self.fit_zoom(), 0.0);
            } else {
                // Keys 1-9 are percentages of the max zoom, clamped to the
                // runtime minimum (fully zoomed out when allowed, else 1x).
                self.set_zoom(
                    self.state.zoom_for_level(zoom_level),
                    self.runtime_min_zoom(),
                );
            }
            self.draw_frame(qh);
        }

        let config_key = &self.state.config.keybindings;

        if keysym_str == config_key.toggle_osd {
            self.state.toggle_osd();
            self.draw_frame(qh);
            tracing::info!("OSD toggled: {}", self.state.osd_visible);
        } else if keysym_str == config_key.screenshot_manual {
            // S: enter Screenshot Mode (drag a manual selection rectangle).
            self.enter_screenshot_mode(false);
            self.draw_frame(qh);
        } else if keysym_str == config_key.screenshot_window {
            tracing::info!("Window screenshot mode - not yet implemented");
        } else if keysym_str == config_key.screenshot_fullscreen {
            // F: enter Screenshot Mode with the whole frozen frame selected
            // (Return saves it).
            self.enter_screenshot_mode(true);
            self.draw_frame(qh);
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
            let default_zoom = self.state.config.default_zoom.unwrap_or(3.0);
            self.set_zoom(default_zoom, self.runtime_min_zoom());
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
        if self.screenshot_active {
            // Screenshot Mode and the Configuration window are mutually
            // exclusive (opening the window exits the mode, and keys are
            // forwarded to egui before the screenshot branch runs), so the
            // egui forwarding below is unreachable while this branch is on.
            // Held WASD nudges repeat through the app-side timer (see
            // `fire_repeat_nudges`), so compositor-sent repeats are only used
            // as a fallback on compositors that deliver them — never doubled
            // with the timer (`nudge_hold` is armed by every WASD press).
            if self.nudge_hold.is_none() {
                let keysym_str = keysym_to_string(event.keysym);
                if matches!(
                    keysym_str.as_str(),
                    "w" | "W" | "a" | "A" | "s" | "S" | "d" | "D"
                ) && let Some(key) = keysym_str.to_ascii_lowercase().chars().next()
                {
                    self.nudge_screenshot(key);
                    self.draw_frame(qh);
                }
            }
            return;
        }
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
        // Releasing a WASD nudge key stops its repeat (app-side repeat).
        if self.screenshot_active && self.nudge_hold.is_some() {
            let keysym_str = keysym_to_string(event.keysym);
            if matches!(
                keysym_str.as_str(),
                "w" | "W" | "a" | "A" | "s" | "S" | "d" | "D"
            ) {
                self.nudge_hold = None;
            }
        }
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
                // next motion would otherwise make dt huge). Also flush the
                // reach-travel history: hold-to-zoom's vertical travel would
                // otherwise inflate the y-reach margin for the next few
                // events and could snap the view to a wall from further away
                // than the user's actual push speed justifies.
                self.last_motion_at = Some(std::time::Instant::now());
                self.reach_travel = TravelHistory::new();
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

// User data for the opaque-region `wl_region` created at startup (see the
// `set_opaque_region` call in `run`). `wl_region` has no events, so this
// handler is a no-op.
impl wayland_client::Dispatch<wl_region::WlRegion, ()> for MagnifierWindow {
    fn event(
        _state: &mut Self,
        _region: &wl_region::WlRegion,
        event: <wl_region::WlRegion as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wl_region has no events.
        let _ = event;
    }
}

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

    /// The zoom at which the whole frozen capture fills the viewport — the
    /// "0 %" / fully-zoomed-out view. Falls back to 1× before the capture
    /// arrives.
    fn fit_zoom(&self) -> f64 {
        match &self.captured {
            Some(c) => fit_zoom(
                (c.buffer.width as f64, c.buffer.height as f64),
                (self.width as f64, self.height as f64),
            ),
            None => 1.0,
        }
    }

    /// The runtime minimum zoom for the wheel, hold-to-zoom, the `1`–`9` keys
    /// and the reset-to-default: the fully-zoomed-out view when
    /// `allow_zero_zoom` is on, otherwise 1×. (The `0` key always reaches the
    /// fully-zoomed-out view regardless of the setting.)
    fn runtime_min_zoom(&self) -> f64 {
        if self.state.config.min_zoom() == 0.0 {
            self.fit_zoom()
        } else {
            1.0
        }
    }

    /// Set the zoom, clamped to `min..=max`, and push it to the renderer and
    /// the magnified cursor. Every zoom operation funnels through this so the
    /// runtime minimum (fit / 1×) is applied consistently.
    fn set_zoom(&mut self, zoom: f64, min: f64) {
        let max = self.state.config.max_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let z = zoom.clamp(min, max);
        self.state.zoom = z;
        self.state.renderer.update_scale_factor(z);
        if let Some(cursor) = &mut self.magnified_cursor {
            cursor.update_zoom(z);
        }
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

    /// Map a logical (viewport) position to capture px through the current
    /// view, accounting for the letterbox when zoomed out below the
    /// screen-filling zoom. Clamped to the capture; before the first capture
    /// arrives it returns the input unchanged.
    /// The center of the magnified quad in logical (viewport) px — where the
    /// magnified cursor sprite sits in normal mode. Used to anchor the sprite
    /// position when entering Screenshot Mode so the cursor does not jump.
    fn viewport_center(&self) -> Option<(f64, f64)> {
        let c = self.captured.as_ref()?;
        let zoom = self.state.zoom.max(1e-3);
        let view_w = self.width as f64 / zoom;
        let view_h = self.height as f64 / zoom;
        let dest_w = (view_w.min(c.buffer.width as f64) * zoom).round();
        let dest_h = (view_h.min(c.buffer.height as f64) * zoom).round();
        if dest_w <= 0.0 || dest_h <= 0.0 {
            return None;
        }
        let off_x = ((self.width as f64 - dest_w) / 2.0).max(0.0);
        let off_y = ((self.height as f64 - dest_h) / 2.0).max(0.0);
        Some((off_x + dest_w / 2.0, off_y + dest_h / 2.0))
    }

    /// The magnified cursor's logical (viewport) position in Screenshot Mode:
    /// the live pointer plus the mode-entry offset, clamped to the magnified
    /// quad so it never floats in the letterbox black bars. In normal mode
    /// the sprite is pinned to the viewport center instead.
    fn screenshot_cursor_logical(&self) -> Option<(f64, f64)> {
        if !self.screenshot_active {
            return self.viewport_center();
        }
        let c = self.captured.as_ref()?;
        let zoom = self.state.zoom.max(1e-3);
        let view_w = self.width as f64 / zoom;
        let view_h = self.height as f64 / zoom;
        let dest_w = (view_w.min(c.buffer.width as f64) * zoom).round();
        let dest_h = (view_h.min(c.buffer.height as f64) * zoom).round();
        if dest_w <= 0.0 || dest_h <= 0.0 {
            return None;
        }
        let off_x = ((self.width as f64 - dest_w) / 2.0).max(0.0);
        let off_y = ((self.height as f64 - dest_h) / 2.0).max(0.0);
        let (ox, oy) = self.screenshot_cursor_offset.unwrap_or((0.0, 0.0));
        Some((
            (self.pointer_position_f.0 + ox).clamp(off_x, off_x + dest_w),
            (self.pointer_position_f.1 + oy).clamp(off_y, off_y + dest_h),
        ))
    }

    /// The magnified cursor sprite's position in capture px (where the user
    /// aims/draws in Screenshot Mode): the logical sprite position mapped to
    /// capture coordinates, falling back to the raw pointer when no capture
    /// exists yet.
    fn screenshot_capture_position(&self) -> (f64, f64) {
        self.screenshot_cursor_logical()
            .map(|pos| self.logical_to_capture(pos))
            .unwrap_or_else(|| self.logical_to_capture(self.pointer_position_f))
    }

    fn logical_to_capture(&self, pos: (f64, f64)) -> (f64, f64) {
        let Some(c) = &self.captured else {
            return pos;
        };
        let zoom = self.state.zoom.max(1e-3);
        let view_w = self.width as f64 / zoom;
        let view_h = self.height as f64 / zoom;
        let dest_w = (view_w.min(c.buffer.width as f64) * zoom).round();
        let dest_h = (view_h.min(c.buffer.height as f64) * zoom).round();
        if dest_w <= 0.0 || dest_h <= 0.0 {
            return pos;
        }
        let off_x = ((self.width as f64 - dest_w) / 2.0).max(0.0);
        let off_y = ((self.height as f64 - dest_h) / 2.0).max(0.0);
        let (cx, cy) = self
            .view_center
            .unwrap_or((c.buffer.width as f64 / 2.0, c.buffer.height as f64 / 2.0));
        let src_x = cx - view_w / 2.0;
        let src_y = cy - view_h / 2.0;
        let sx = view_w / dest_w;
        let sy = view_h / dest_h;
        (
            ((pos.0 - off_x) * sx + src_x).clamp(0.0, c.buffer.width as f64),
            ((pos.1 - off_y) * sy + src_y).clamp(0.0, c.buffer.height as f64),
        )
    }

    /// Inverse of [`Self::logical_to_capture`]: capture px to logical
    /// (viewport) px. Unchanged before the first capture arrives.
    fn capture_to_logical(&self, pos: (f64, f64)) -> (f64, f64) {
        let Some(c) = &self.captured else {
            return pos;
        };
        let zoom = self.state.zoom.max(1e-3);
        let view_w = self.width as f64 / zoom;
        let view_h = self.height as f64 / zoom;
        let dest_w = (view_w.min(c.buffer.width as f64) * zoom).round();
        let dest_h = (view_h.min(c.buffer.height as f64) * zoom).round();
        if dest_w <= 0.0 || dest_h <= 0.0 {
            return pos;
        }
        let off_x = ((self.width as f64 - dest_w) / 2.0).max(0.0);
        let off_y = ((self.height as f64 - dest_h) / 2.0).max(0.0);
        let (cx, cy) = self
            .view_center
            .unwrap_or((c.buffer.width as f64 / 2.0, c.buffer.height as f64 / 2.0));
        let src_x = cx - view_w / 2.0;
        let src_y = cy - view_h / 2.0;
        let sx = view_w / dest_w;
        let sy = view_h / dest_h;
        ((pos.0 - src_x) / sx + off_x, (pos.1 - src_y) / sy + off_y)
    }

    /// Enter Screenshot Mode. `fullscreen` pre-selects the whole frozen frame
    /// (the `F` key); otherwise the mode starts with no selection and the
    /// user drags one out.
    fn enter_screenshot_mode(&mut self, fullscreen: bool) {
        self.screenshot_active = true;
        // Each entry starts at the configured default save scale; the toggle
        // key then flips it while in the mode.
        self.effective_screenshot_scale = None;
        self.screenshot_dragging = false;
        self.screenshot_drag_start = None;
        self.screenshot_rect = if fullscreen {
            self.captured
                .as_ref()
                .map(|c| (0.0, 0.0, c.buffer.width as f64, c.buffer.height as f64))
        } else {
            None
        };
        // Anchor the magnified cursor to its current visual position (the
        // viewport center) so entering the mode never jumps it: the sprite
        // follows the pointer plus this offset, and only *relative* pointer
        // movement moves it.
        self.screenshot_cursor_offset = self.viewport_center().map(|center| {
            (
                center.0 - self.pointer_position_f.0,
                center.1 - self.pointer_position_f.1,
            )
        });
        tracing::info!("Screenshot mode entered (fullscreen: {fullscreen})");
    }

    /// Leave Screenshot Mode, discarding the selection.
    fn exit_screenshot_mode(&mut self) {
        self.screenshot_active = false;
        self.screenshot_dragging = false;
        self.screenshot_drag_start = None;
        self.screenshot_rect = None;
        self.effective_screenshot_scale = None;
        self.nudge_hold = None;
        self.screenshot_cursor_offset = None;
        tracing::info!("Screenshot mode exited");
    }

    /// The screenshot save scale currently in effect: the configured
    /// `screenshot_scale` default, overridden while in Screenshot Mode by the
    /// scale-toggle key.
    fn effective_screenshot_scale(&self) -> crate::config::ScreenshotScale {
        self.effective_screenshot_scale
            .unwrap_or(self.state.config.screenshot_scale)
    }

    /// Fire any pending repeat nudges for a held WASD key. Called from
    /// `draw_frame` (which runs on every event and on the loop's repeat
    /// deadline wake-ups, see [`NUDGE_REPEAT_DELAY_MS`]); the cadence is
    /// drift-free, so a delayed wake-up never bunches up extra nudges.
    fn fire_repeat_nudges(&mut self) {
        let Some((key, next_at)) = self.nudge_hold else {
            return;
        };
        let now = std::time::Instant::now();
        if now < next_at {
            return;
        }
        self.nudge_hold = Some((
            key,
            advance_repeat_deadline(
                now,
                next_at,
                std::time::Duration::from_millis(NUDGE_REPEAT_INTERVAL_MS),
            ),
        ));
        self.nudge_screenshot(key);
    }

    /// How long the event loop may block before the next nudge repeat is due:
    /// `Some(remaining)` while a nudge key is held (zero if already due),
    /// `None` (block indefinitely) when idle. The loop passes this to
    /// [`dispatch_with_timeout`], so it wakes on the repeat deadline even
    /// when the compositor delivers no events at all.
    fn repeat_poll_timeout(&self) -> Option<std::time::Duration> {
        let (_, next_at) = self.nudge_hold?;
        let now = std::time::Instant::now();
        Some(if now >= next_at {
            std::time::Duration::ZERO
        } else {
            next_at - now
        })
    }

    /// After a timed wait, redraw if a nudge repeat came due while the loop
    /// was blocked with no events to wake the normal dispatch path.
    /// `draw_frame` fires the due repeat(s) at its top.
    fn draw_frame_if_repeat_due(&mut self, qh: &QueueHandle<Self>) {
        let Some((_, next_at)) = self.nudge_hold else {
            return;
        };
        if std::time::Instant::now() >= next_at {
            self.draw_frame(qh);
        }
    }

    /// Redraw in response to pointer motion, capped at
    /// [`MOTION_REDRAW_INTERVAL`]: the first event of a burst draws
    /// immediately (lowest latency), and any motion arriving within the
    /// interval is coalesced into a single scheduled draw (fired by
    /// [`MagnifierWindow::draw_frame_if_motion_pending`] when the loop wakes
    /// on the deadline). State updates in the motion handler stay per-event
    /// (cheap); only the full-frame draw + present is throttled.
    fn request_motion_redraw(&mut self, qh: &QueueHandle<Self>) {
        let now = std::time::Instant::now();
        if motion_redraw_due(self.last_draw_at, now) {
            self.redraw_pending = false;
            self.last_draw_at = Some(now);
            self.draw_frame(qh);
        } else {
            self.redraw_pending = true;
        }
    }

    /// After a timed wait, redraw if a motion redraw was throttled while the
    /// loop was blocked with no events to wake the normal dispatch path.
    fn draw_frame_if_motion_pending(&mut self, qh: &QueueHandle<Self>) {
        if !self.redraw_pending {
            return;
        }
        let now = std::time::Instant::now();
        if motion_redraw_due(self.last_draw_at, now) {
            self.redraw_pending = false;
            self.last_draw_at = Some(now);
            self.draw_frame(qh);
        }
    }

    /// The poll bound for the event loop: the earliest of the nudge-repeat
    /// deadline and the pending motion-redraw deadline. Returns `None` when
    /// neither is pending, so the loop blocks indefinitely (idle behaviour
    /// is unchanged).
    fn poll_timeout(&self) -> Option<std::time::Duration> {
        let mut best = self.repeat_poll_timeout();
        if self.redraw_pending {
            let wait = match self.last_draw_at {
                None => std::time::Duration::ZERO,
                Some(last) => {
                    let elapsed = std::time::Instant::now().duration_since(last);
                    MOTION_REDRAW_INTERVAL.saturating_sub(elapsed)
                }
            };
            best = Some(match best {
                None => wait,
                Some(b) => b.min(wait),
            });
        }
        best
    }

    /// Nudge the selection border closest to the magnified cursor by 1 real
    /// capture pixel in the WASD direction. In Screenshot Mode the cursor
    /// sprite follows the live pointer (the view stays put), so the anchor is
    /// the pointer's capture position — the user moves the cursor next to a
    /// border and nudges that one.
    fn nudge_screenshot(&mut self, key: char) {
        let Some(rect) = self.screenshot_rect else {
            return;
        };
        let Some(bounds) = self
            .captured
            .as_ref()
            .map(|c| (c.buffer.width as f64, c.buffer.height as f64))
        else {
            return;
        };
        // The nudge anchor is where the magnified cursor *visually* sits
        // (pointer + mode-entry offset, clamped to the quad) — detection is
        // based on what the user sees, not the raw hand position.
        let border = active_screenshot_border(rect, self.screenshot_capture_position());
        self.screenshot_rect = Some(nudge_screenshot_border(rect, border, key, bounds));
    }

    /// Whether the transient screenshot notice (e.g. "Saved …") is still
    /// fresh enough to show (used to force the OSD legend on briefly after a
    /// screenshot, even when the legend is otherwise toggled off).
    fn screenshot_notice_fresh(&self) -> bool {
        matches!(
            &self.screenshot_notice,
            Some((_, at)) if at.elapsed() < std::time::Duration::from_secs(SCREENSHOT_NOTICE_SECS)
        )
    }

    /// Crop the frozen frame to `rect` (capture px) and save it as a PNG at
    /// the configured path/filename pattern, then leave Screenshot Mode and
    /// show a transient "Saved …" notice in the OSD.
    fn capture_screenshot(&mut self, rect: (f64, f64, f64, f64)) {
        let region = ScreenshotRegion {
            x: rect.0.round() as i32,
            y: rect.1.round() as i32,
            width: ((rect.2 - rect.0).round() as u32).max(1),
            height: ((rect.3 - rect.1).round() as u32).max(1),
        };
        match self.save_screenshot(Some(&region)) {
            Ok(path) => {
                self.screenshot_notice = Some((
                    format!("Saved {}", path.display()),
                    std::time::Instant::now(),
                ));
            }
            Err(e) => {
                self.screenshot_notice =
                    Some((format!("Screenshot failed: {e}"), std::time::Instant::now()));
            }
        }
        self.exit_screenshot_mode();
        self.screenshot_rect = None;
    }

    /// Handle a key press while Screenshot Mode is active: `F` selects the
    /// whole screen, Return saves the current selection, Esc/Q cancel, and
    /// WASD nudges the border closest to the cursor. All other keys are
    /// suspended (no zoom, no pan, no quit).
    fn handle_screenshot_key(&mut self, keysym: Keysym, keysym_str: &str) {
        let config_key = &self.state.config.keybindings;
        if keysym_str == config_key.screenshot_fullscreen {
            if let Some(c) = &self.captured {
                self.screenshot_rect =
                    Some((0.0, 0.0, c.buffer.width as f64, c.buffer.height as f64));
            }
        } else if keysym == Keysym::Return || keysym == Keysym::KP_Enter {
            if let Some(rect) = self.screenshot_rect {
                self.capture_screenshot(rect);
            }
        } else if keysym == Keysym::Escape || keysym == Keysym::q || keysym == Keysym::Q {
            self.exit_screenshot_mode();
        } else if keysym_str == config_key.screenshot_scale_toggle {
            // Flip the effective save scale (real <-> magnified); the legend
            // shows the current one and the next save honors it.
            self.effective_screenshot_scale =
                Some(toggle_screenshot_scale(self.effective_screenshot_scale()));
        } else if matches!(keysym_str, "w" | "W" | "a" | "A" | "s" | "S" | "d" | "D")
            && let Some(key) = keysym_str.to_ascii_lowercase().chars().next()
        {
            self.nudge_screenshot(key);
            // Arm app-side key repeat: the event loop wakes on the repeat
            // deadline (`dispatch_with_timeout`) and `draw_frame` fires the
            // nudges (compositor-independent — niri does not deliver
            // `wl_keyboard` repeated-key events to this manual-loop client).
            // Only armed once a selection exists, so holding WASD before
            // drawing one cannot spin pointless wake-ups.
            if self.screenshot_rect.is_some() {
                self.nudge_hold = Some((
                    key,
                    std::time::Instant::now()
                        + std::time::Duration::from_millis(NUDGE_REPEAT_DELAY_MS),
                ));
            }
        }
    }

    /// Handle a pointer event while Screenshot Mode is active: LMB drag draws
    /// the selection, RMB cancels the mode, motion updates the drag (and the
    /// view never pans in this mode).
    fn handle_screenshot_pointer(&mut self, event: &PointerEvent, qh: &QueueHandle<Self>) {
        match event.kind {
            PointerEventKind::Motion { .. } => {
                let position = event.position;
                self.pointer_position_f = position;
                self.state.pointer_position = (position.0 as i32, position.1 as i32);
                if self.screenshot_dragging
                    && let Some(start) = self.screenshot_drag_start
                    && let Some(c) = &self.captured
                {
                    let bounds = (c.buffer.width as f64, c.buffer.height as f64);
                    // The drag tracks the magnified cursor *sprite* (pointer
                    // + the mode-entry offset), not the raw physical pointer:
                    // the user aims and draws with the visible cursor, so the
                    // rectangle must follow it, never diverge from it. The
                    // live corner is snapped to the nearest whole capture
                    // pixel, so the rectangle stays aligned to the magnified
                    // pixel grid in capture space while dragging (the anchor
                    // was snapped on press) and the saved crop is exact.
                    let cur = snap_capture_px(self.screenshot_capture_position());
                    self.screenshot_rect = Some(normalize_screenshot_rect(start, cur, bounds));
                }
                self.request_motion_redraw(qh);
            }
            PointerEventKind::Press { button, .. } => {
                if button == BTN_LEFT {
                    // Snap the drag anchor to the nearest whole capture pixel
                    // so the drawn rectangle always aligns with the magnified
                    // pixel grid (and the saved crop matches exactly what the
                    // user sees — see [`snap_capture_px`]).
                    let cap = snap_capture_px(self.screenshot_capture_position());
                    self.screenshot_drag_start = Some(cap);
                    self.screenshot_dragging = true;
                    if let Some(c) = &self.captured {
                        let bounds = (c.buffer.width as f64, c.buffer.height as f64);
                        self.screenshot_rect = Some(normalize_screenshot_rect(cap, cap, bounds));
                    }
                } else if button == BTN_RIGHT {
                    self.exit_screenshot_mode();
                }
                self.draw_frame(qh);
            }
            PointerEventKind::Release { button, .. } => {
                if button == BTN_LEFT {
                    self.screenshot_dragging = false;
                    self.screenshot_drag_start = None;
                }
                self.draw_frame(qh);
            }
            _ => {}
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

    /// Snap the view center exactly onto a capture edge when the user pushes
    /// into it (see [`EdgeReach`]). Evaluated on every `draw_frame` call —
    /// the single choke point after any state change — using the **stored
    /// direction of the last motion event** (see
    /// [`MagnifierWindow::last_motion_delta`]) rather than the current
    /// event's delta, so the reach is applied consistently with whatever the
    /// render sees. It never fires before the first motion event, so the
    /// launch centering on the pointer's position is preserved even when that
    /// position sits near an edge. While hold-to-zoom is active the reach is
    /// applied to the **x axis only**: the view y is locked to the anchor
    /// content under the cursor (the y-lock invariant), so it must never be
    /// snapped to a wall by the reach.
    ///
    /// The reach margin scales with the pointer's recent **peak** travel:
    /// fast flicks (whose delivered position can lag the hand's true stop)
    /// get a margin large enough to bridge the gap, while a slow crawl keeps
    /// a small margin so the edge is never magnetic. The view must already be
    /// within the (scaled + slack) margin of the wall, so the snap is always
    /// the size of a delivery gap, never a teleport across a large residual.
    /// `center` must already be clamped to the capture.
    fn apply_edge_reach(&self, center: (f64, f64)) -> (f64, f64) {
        let Some((lx, ly)) = self.last_motion_delta else {
            return center;
        };
        let Some(captured) = &self.captured else {
            return center;
        };
        let bounds = (captured.buffer.width as f64, captured.buffer.height as f64);
        let (sx, sy) = self.capture_scale();
        let (peak_x, peak_y) = self.reach_travel.peak();
        let margin_x = reach_margin(peak_x);
        let margin_y = reach_margin(peak_y);
        let reach_x = EdgeReach::new(self.width as f64, bounds.0, sx);
        let reach_y = EdgeReach::new(self.height as f64, bounds.1, sy);
        let result = (
            reach_x.apply(center.0, lx, self.pointer_position_f.0, margin_x),
            if self.hold_to_zoom_active {
                // The y-lock owns the view y during hold-to-zoom: never snap
                // it to a wall (the anchor content under the cursor must stay
                // glued while the hand travels to zoom).
                center.1
            } else {
                reach_y.apply(center.1, ly, self.pointer_position_f.1, margin_y)
            },
        );
        // Diagnostic (run with `RUST_LOG=maggie=debug`): near the surface
        // edges, log the raw geometry every draw so the wall-reach behaviour
        // can be verified against the compositor's delivered pointer
        // positions. `view_before` vs `view_after` shows whether the reach
        // fired; `hand_content` and the margins discriminate a delivery-gap
        // shortfall (view tracks the hand, pointer short of the surface
        // edge) from a residual shortfall (view lags the hand content).
        if tracing::enabled!(tracing::Level::DEBUG) {
            let (px, py) = self.pointer_position_f;
            let near_edge = px < 200.0
                || px > self.width as f64 - 200.0
                || py < 200.0
                || py > self.height as f64 - 200.0;
            if near_edge {
                let hand_content = (px * sx, py * sy);
                tracing::debug!(
                    pointer = ?self.pointer_position_f,
                    surface = ?(self.width, self.height),
                    view_before = ?center,
                    view_after = ?result,
                    hand_content = ?hand_content,
                    last_delta = ?self.last_motion_delta,
                    peak_travel = ?(peak_x, peak_y),
                    reach_margin = ?(margin_x, margin_y),
                    zoom = self.state.zoom,
                    "near-edge draw"
                ); // A greppable marker for the exact failure under
                // investigation: pushing toward an edge with the pointer
                // already inside the (speed-scaled) reach margin while the
                // view is still short of the wall after the reach — i.e. the
                // delivery gap or residual exceeded what the margin could
                // bridge (a normal approach never trips it: the reach either
                // fires or the pointer is still outside the margin).
                let short_x =
                    (lx > 0.0 && px > self.width as f64 - margin_x && result.0 < bounds.0 - 0.5)
                        || (lx < 0.0 && px < margin_x && result.0 > 0.5);
                let short_y =
                    (ly > 0.0 && py > self.height as f64 - margin_y && result.1 < bounds.1 - 0.5)
                        || (ly < 0.0 && py < margin_y && result.1 > 0.5);
                if short_x || short_y {
                    tracing::debug!(
                        pointer = ?self.pointer_position_f,
                        surface = ?(self.width, self.height),
                        view = ?result,
                        hand_content = ?hand_content,
                        peak_travel = ?(peak_x, peak_y),
                        reach_margin = ?(margin_x, margin_y),
                        "SETTLED SHORTFALL: view short of a wall while pushing into the edge zone"
                    );
                }
            }
        }
        result
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
        // Screenshot Mode shows its own instruction legend.
        if self.screenshot_active {
            let config_key = &self.state.config.keybindings;
            return vec![
                "maggie  screenshot mode".to_string(),
                "drag  select".to_string(),
                format!("{}  fullscreen", config_key.screenshot_fullscreen),
                "Return  save".to_string(),
                "Esc  cancel".to_string(),
                "WASD  nudge border".to_string(),
                format!(
                    "{}  scale: {}",
                    config_key.screenshot_scale_toggle,
                    self.effective_screenshot_scale().name()
                ),
            ];
        }
        let config_key = &self.state.config.keybindings;
        let mut lines = vec![
            // At the fully-zoomed-out view the readout shows "0 %" (see
            // [`zoom_readout`]); otherwise the zoom factor.
            format!(
                "maggie  zoom {}",
                zoom_readout(self.state.zoom, self.fit_zoom())
            ),
            // Plain "0-9 zoom level": the built-in bitmap font has no glyphs
            // for parentheses, so a fancier "(0 = 0%)" suffix used to render
            // as garbage on screen.
            "0-9  zoom level".to_string(),
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
        ];
        // A transient notice (e.g. the path of the last saved screenshot) is
        // shown for a few seconds after the event.
        if let Some((message, at)) = &self.screenshot_notice
            && at.elapsed() < std::time::Duration::from_secs(SCREENSHOT_NOTICE_SECS)
        {
            lines.insert(1, message.clone());
        }
        lines
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
        self.reach_travel = TravelHistory::new();
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
        // Held WASD nudge keys repeat here (on the frame-callback cadence);
        // this runs before drawing so the selection reflects the latest
        // nudge. No-op unless a repeat deadline is due.
        self.fire_repeat_nudges();

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
        // The edge reach runs on every draw (see
        // [`MagnifierWindow::apply_edge_reach`]), landing the view exactly on
        // a wall when the user pushes into it, using the stored last-motion
        // direction. The result is written back so the next motion event
        // pans from the reached position.
        let (center_x, center_y) = self.apply_edge_reach((center_x, center_y));
        self.view_center = Some((center_x, center_y));
        let src_x = center_x - view_w / 2.0;
        let src_y = center_y - view_h / 2.0;

        let lines = self.osd_lines();

        // The magnified cursor is drawn at the exact center of the viewport
        // (the center of the magnified quad; the quad fills the screen at
        // zoom >= 1) — except in Screenshot Mode, where it follows the live
        // pointer instead (the view stays put, so the user can aim the cursor
        // at a selection border to nudge it). Clamped to the quad so it never
        // floats in the letterbox black bars at fit zoom. At 0 % zoom the
        // sprite would be a degenerate 0x0 texture (and invisible anyway), so
        // it is skipped entirely.
        let cursor_logical = if self.pointer_seen
            && zoom > 0.0
            && self.state.cursor_visible
            && self.magnified_cursor.is_some()
        {
            if self.screenshot_active {
                // In Screenshot Mode the sprite follows the live pointer plus
                // the mode-entry offset (anchored so entering the mode never
                // jumps it), clamped to the quad.
                self.screenshot_cursor_logical()
            } else {
                Some((
                    off_x as f64 + dest_w as f64 / 2.0,
                    off_y as f64 + dest_h as f64 / 2.0,
                ))
            }
        } else {
            None
        };

        // The OSD ring marks the magnified cursor (always at the viewport
        // center); fall back to the hand position when no magnified cursor is
        // drawn.
        let osd_ring = cursor_logical
            .map(|(cx, cy)| (cx as i32, cy as i32))
            .unwrap_or(self.state.pointer_position);
        // In Screenshot Mode the legend moves to a fixed top-left corner so
        // it never overlaps the area being selected; otherwise it follows the
        // cursor ring as usual.
        let osd_anchor = if self.screenshot_active {
            (16, 16)
        } else {
            osd_ring
        };
        // The screenshot selection rectangle mapped into logical (viewport)
        // px, and the configured border color — computed here (before the
        // GPU/CPU branches) because those branches borrow `self` mutably and
        // cannot call `&self` methods.
        let screenshot_rect_logical = if self.screenshot_active {
            self.screenshot_rect.map(|r| {
                let a = self.capture_to_logical((r.0, r.1));
                let b = self.capture_to_logical((r.2, r.3));
                (a.0, a.1, b.0, b.1)
            })
        } else {
            None
        };
        let screenshot_color = self.state.config.screenshot_selection_color;
        // A fresh screenshot notice forces the legend on briefly, so the
        // "Saved …" heads-up is always visible even with the legend toggled
        // off. Computed before the GPU branch (which borrows `self.gpu`).
        let notice_fresh = self.screenshot_notice_fresh();
        // The active selection border — always the edge closest to the
        // cursor, wherever it is (no proximity requirement) — highlighted in
        // the overlay so it is obvious which edge WASD will nudge.
        let screenshot_active_border = if self.screenshot_active {
            self.screenshot_rect.map(|r| {
                // Detection is based on where the magnified cursor *visibly*
                // sits (pointer + entry offset), not the raw hand position.
                active_screenshot_border(r, self.screenshot_capture_position())
            })
        } else {
            None
        };

        if let Some(gpu) = &mut self.gpu {
            let osd = if self.state.osd_visible || notice_fresh {
                crate::osd::build_osd_sprite(
                    &lines,
                    (
                        osd_anchor.0 * crate::gpu::RENDER_SCALE,
                        osd_anchor.1 * crate::gpu::RENDER_SCALE,
                    ),
                    self.width as i32 * crate::gpu::RENDER_SCALE,
                    self.height as i32 * crate::gpu::RENDER_SCALE,
                )
            } else {
                None
            };
            // The screenshot selection overlay (scrim + colored border) at
            // the RENDER_SCALE buffer resolution, uploaded as a fullscreen
            // sprite on the GPU. It is **cached**: only rebuilt (and only
            // re-uploaded) when the mode/selection changed — plain pointer
            // motion reuses the existing texture, which is what keeps the
            // mouse snappy in Screenshot Mode.
            let lw = self.width as i32 * crate::gpu::RENDER_SCALE;
            let lh = self.height as i32 * crate::gpu::RENDER_SCALE;
            let overlay_state = (
                self.screenshot_active,
                self.screenshot_rect,
                screenshot_active_border,
            );
            let size_mismatch = self
                .screenshot_overlay
                .as_ref()
                .is_none_or(|b| b.width != lw || b.height != lh);
            let rebuild = self.screenshot_overlay_state != Some(overlay_state) || size_mismatch;
            if rebuild {
                let mut buf = self
                    .screenshot_overlay
                    .take()
                    .unwrap_or_else(|| RgbaBuffer {
                        width: lw,
                        height: lh,
                        data: Vec::new(),
                    });
                buf.width = lw;
                buf.height = lh;
                buf.data.resize((lw as usize) * (lh as usize) * 4, 0);
                if self.screenshot_active {
                    let rect_px = screenshot_rect_logical.map(|(x0, y0, x1, y1)| {
                        (
                            x0 * crate::gpu::RENDER_SCALE as f64,
                            y0 * crate::gpu::RENDER_SCALE as f64,
                            x1 * crate::gpu::RENDER_SCALE as f64,
                            y1 * crate::gpu::RENDER_SCALE as f64,
                        )
                    });
                    fill_screenshot_overlay(
                        &mut buf.data,
                        lw,
                        lh,
                        rect_px,
                        screenshot_color,
                        2 * crate::gpu::RENDER_SCALE,
                        screenshot_active_border,
                    );
                } else {
                    buf.data.fill(0);
                }
                self.screenshot_overlay = Some(buf);
                self.screenshot_overlay_state = Some(overlay_state);
            }
            let overlay = self.screenshot_overlay.as_ref();
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
            if zoom > 0.0 {
                let uv = (
                    src_x / source_w as f64,
                    src_y / source_h as f64,
                    view_w.min(source_w as f64) / source_w as f64,
                    view_h.min(source_h as f64) / source_h as f64,
                );
                gpu.draw(Some(uv), osd.as_ref(), cursor.as_ref(), overlay, rebuild);
            } else {
                // 0 % zoom: the magnified view collapses to nothing — draw a
                // plain black view (src = None clears the buffer) while the
                // magnified cursor and OSD legend stay visible so the user
                // can still navigate back in.
                gpu.draw(None, osd.as_ref(), cursor.as_ref(), overlay, rebuild);
            }
            if self.animating {
                self.request_frame_callback(qh);
            }
            return;
        }

        let scaled =
            self.state
                .renderer
                .render_bilinear(&captured.buffer, (src_x, src_y), dest_w, dest_h);

        // Same forced-on behavior as the GPU path: a fresh screenshot notice
        // always shows the legend briefly.
        let show_osd = self.state.osd_visible || notice_fresh;
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
        // The screenshot overlay at logical resolution, blended into the
        // canvas inside the render closure (which cannot borrow `self`).
        // Cached like the GPU path: rebuilt only when the mode/selection
        // changed.
        let (ow, oh) = (self.width as i32, self.height as i32);
        let overlay_state = (
            self.screenshot_active,
            self.screenshot_rect,
            screenshot_active_border,
        );
        let size_mismatch = self
            .screenshot_overlay
            .as_ref()
            .is_none_or(|b| b.width != ow || b.height != oh);
        if self.screenshot_overlay_state != Some(overlay_state) || size_mismatch {
            let mut buf = self
                .screenshot_overlay
                .take()
                .unwrap_or_else(|| RgbaBuffer {
                    width: ow,
                    height: oh,
                    data: Vec::new(),
                });
            buf.width = ow;
            buf.height = oh;
            buf.data.resize((ow as usize) * (oh as usize) * 4, 0);
            if self.screenshot_active {
                fill_screenshot_overlay(
                    &mut buf.data,
                    ow,
                    oh,
                    screenshot_rect_logical,
                    screenshot_color,
                    2,
                    screenshot_active_border,
                );
            } else {
                buf.data.fill(0);
            }
            self.screenshot_overlay = Some(buf);
            self.screenshot_overlay_state = Some(overlay_state);
        }
        let screenshot_overlay_cpu: Option<(Vec<u8>, i32, i32)> = self
            .screenshot_overlay
            .as_ref()
            .map(|b| (b.data.clone(), b.width, b.height));

        self.render_frame(qh, |canvas, width, height, stride| {
            canvas.fill(0);
            for y in 0..dest_h {
                let src_row = &scaled.data[(y as usize) * (scaled.width as usize) * 4..];
                let dest_row = &mut canvas
                    [((y + off_y) as usize) * (stride as usize) + (off_x as usize) * 4..];
                dest_row[..(dest_w as usize) * 4]
                    .copy_from_slice(&src_row[..(dest_w as usize) * 4]);
            }
            if let Some((ref overlay, ow, oh)) = screenshot_overlay_cpu {
                blend_overlay_into(canvas, stride, overlay, ow, oh);
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
            gpu.draw(None, None, None, None, false);
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

    /// Save the frozen frame (cropped to `region` when given, otherwise the
    /// whole frame) as a PNG at the configured path/pattern, returning the
    /// written path.
    fn save_screenshot(&mut self, region: Option<&ScreenshotRegion>) -> anyhow::Result<PathBuf> {
        // Crop the frozen frame first (inside the borrow of `self.captured`),
        // then release it before touching `self.capture_manager`.
        let (mut crop, mut w, mut h) = {
            let Some(captured) = &self.captured else {
                return Err(anyhow::anyhow!("No captured frame yet"));
            };
            let buffer = &captured.buffer;
            let (x, y, w, h) = match region {
                Some(r) => {
                    let x = r.x.clamp(0, buffer.width - 1);
                    let y = r.y.clamp(0, buffer.height - 1);
                    let w = (r.width as i32).clamp(1, buffer.width - x) as u32;
                    let h = (r.height as i32).clamp(1, buffer.height - y) as u32;
                    (x, y, w, h)
                }
                None => (0, 0, buffer.width as u32, buffer.height as u32),
            };
            let mut crop = Vec::with_capacity((w as usize) * (h as usize) * 4);
            for row in y..y + h as i32 {
                let start = (row as usize * buffer.width as usize + x as usize) * 4;
                crop.extend_from_slice(&buffer.data[start..start + (w as usize) * 4]);
            }
            (crop, w, h)
        };
        // When the user chose the magnified scale, upscale the crop to the
        // current zoom (nearest neighbor, matching the crisp magnifier look;
        // clamped to real size when zoom is below 1x).
        if self.effective_screenshot_scale() == crate::config::ScreenshotScale::Magnified {
            let scale = self.state.zoom.max(1.0);
            if scale > 1.0 {
                let (scaled, nw, nh) = upscale_nearest(&crop, w, h, scale);
                crop = scaled;
                w = nw;
                h = nh;
            }
        }
        let path = self.capture_manager.generate_screenshot_path()?;
        let file = std::fs::File::create(&path)?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&crop)?;
        tracing::info!("Screenshot saved to {}", path.display());
        Ok(path)
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
    // The layer surface is fully opaque everywhere (the magnified frame, the
    // black bars beyond it, and every overlay/cursor/OSD blend into opaque
    // pixels), so mark it opaque to the compositor. Compositors use this for
    // occlusion culling: everything fully covered by this surface stops being
    // composited, which makes Maggie's redraw cost independent of whatever
    // app happens to be underneath it (a constantly repainting browser no
    // longer adds compositor load while the magnifier is up). The region is
    // only consulted at commit time, so it can be destroyed right after.
    let opaque_region = compositor.wl_compositor().create_region(&qh, ());
    opaque_region.add(0, 0, i32::MAX, i32::MAX);
    layer.wl_surface().set_opaque_region(Some(&opaque_region));
    layer.commit();
    opaque_region.destroy();

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
        redraw_pending: false,
        last_draw_at: None,
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
        screenshot_active: false,
        screenshot_dragging: false,
        screenshot_drag_start: None,
        screenshot_rect: None,
        effective_screenshot_scale: None,
        nudge_hold: None,
        screenshot_cursor_offset: None,
        screenshot_notice: None,
        screenshot_overlay: None,
        screenshot_overlay_state: None,
        config_window: None,
        last_pointer_serial: None,
        hold_to_zoom_active: false,
        hold_zoom_last_y: 0.0,
        reach_travel: TravelHistory::new(),
        last_motion_delta: None,
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
        // capture Ready, frame callbacks) — but never longer than until the
        // next held-key nudge repeat is due (`None` blocks indefinitely, so
        // idle behaviour is unchanged). The first capture was already
        // requested at first configure, so the frame appears with no delay.
        let timeout = window.poll_timeout();
        dispatch_with_timeout(&conn, &mut event_queue, &mut window, timeout)?;
        // A nudge repeat may have come due while the loop was blocked with no
        // events to wake it — fire it now (compositor-independent repeat).
        window.draw_frame_if_repeat_due(&qh);
        // Likewise, a throttled motion redraw may have come due: fire it now
        // so panning stays smooth at a capped rate without flooding the
        // compositor with per-event commits.
        window.draw_frame_if_motion_pending(&qh);
    }

    Ok(())
}

/// Block for Wayland events like `EventQueue::blocking_dispatch`, but never
/// longer than `timeout` — the blocking poll is bounded so the event loop
/// wakes in time to fire a pending nudge repeat even when the compositor
/// delivers no events at all. A `None` timeout blocks indefinitely, exactly
/// like `blocking_dispatch`.
fn dispatch_with_timeout(
    conn: &Connection,
    event_queue: &mut wayland_client::EventQueue<MagnifierWindow>,
    window: &mut MagnifierWindow,
    timeout: Option<std::time::Duration>,
) -> Result<usize, wayland_client::DispatchError> {
    let dispatched = event_queue.dispatch_pending(window)?;
    if dispatched > 0 {
        return Ok(dispatched);
    }
    conn.flush()?;
    if let Some(guard) = event_queue.prepare_read() {
        let timeout_ts = timeout.map(|d| rustix::event::Timespec {
            tv_sec: d.as_secs() as i64,
            tv_nsec: d.subsec_nanos() as i64,
        });
        // Poll the socket, bounded by the timeout. The poll borrows the
        // guard's fd, so it runs in an inner scope that ends before the read.
        let ready = {
            let fd = guard.connection_fd();
            let mut fds = [rustix::event::PollFd::new(
                &fd,
                rustix::event::PollFlags::IN | rustix::event::PollFlags::ERR,
            )];
            loop {
                match rustix::event::poll(&mut fds, timeout_ts.as_ref()) {
                    Ok(_) => break,
                    // A signal restarts the wait from the full timeout; at the
                    // ~30 Hz repeat cadence the worst-case overshoot is one
                    // interval, which is acceptable.
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(e) => {
                        return Err(wayland_client::backend::WaylandError::Io(e.into()).into());
                    }
                }
            }
            fds[0].revents() & (rustix::event::PollFlags::IN | rustix::event::PollFlags::ERR)
        };
        // Only read when the socket is actually ready; when the timeout
        // simply expired with no events, dropping the guard cancels the read
        // and the caller retries on the next iteration. A WouldBlock read is
        // treated as "no events" (mirroring `blocking_read`).
        if !ready.is_empty() {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    event_queue.dispatch_pending(window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_redraw_due_caps_the_cadence() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        // First motion after any draw is always due (lowest latency).
        assert!(motion_redraw_due(None, t0));
        // Motion right after a draw is throttled (coalesced).
        assert!(!motion_redraw_due(Some(t0), t0 + Duration::from_millis(1)));
        // Just before the interval it is still throttled...
        assert!(!motion_redraw_due(
            Some(t0),
            t0 + MOTION_REDRAW_INTERVAL - Duration::from_micros(1)
        ));
        // ...and at the interval boundary it is due again.
        assert!(motion_redraw_due(Some(t0), t0 + MOTION_REDRAW_INTERVAL));
        assert!(motion_redraw_due(Some(t0), t0 + Duration::from_millis(50)));
    }

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
        // Simulate the full pipeline (pan + hard clamp + wall-aware offset
        // correction in the motion handler, then the hand-edge reach applied
        // on the draw stage with the stored per-event direction) with a
        // leftover view-vs-hand offset AND a pointer whose delivered position
        // stops short of the surface edge (edge clamping / fast-stop lag).
        // Repeated full left-right panning must always land the view
        // *exactly* on both edges — the wall wins — and the offset must decay
        // away during free motion.
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
                    view = reach.apply(corrected, step, hand, reach_margin(step));
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
        // The reach margin is proportional to the pointer's recent peak
        // travel: a fast flick whose delivered position stops well short of
        // the surface edge still lands the view exactly on the wall, while a
        // slow push keeps a small margin so the edge is never magnetic.
        let scale = 1.5;
        let surface = 2133.0;
        let bounds = 3200.0;
        let reach = EdgeReach::new(surface, bounds, scale);
        // Fast flick: 60 logical px short of the edge, peak travel 40 px
        // (margin = 68 px) -> the gap is bridged, view lands on the wall.
        assert_eq!(
            reach.apply(3150.0, 40.0, surface - 60.0, reach_margin(40.0)),
            bounds
        );
        // The same shortfall at slow speed (travel 1 px, margin ~9.5 px)
        // does not bridge it: the view stays put (no magnetic wall).
        assert_eq!(
            reach.apply(3150.0, 1.0, surface - 60.0, reach_margin(1.0)),
            3150.0
        );
        // Pushing away from the edge never triggers, however large the
        // travel (here: pushing left while the pointer sits near the right
        // edge).
        assert_eq!(
            reach.apply(3150.0, -40.0, surface - 10.0, reach_margin(40.0)),
            3150.0
        );
        // A view far from the wall never triggers (no teleports).
        assert_eq!(
            reach.apply(2500.0, 40.0, surface - 60.0, reach_margin(40.0)),
            2500.0
        );
    }

    #[test]
    fn reach_margin_is_capped() {
        // The margin is capped (REACH_MAX_LOGICAL) so an extreme motion
        // burst cannot create a huge magnetic zone: a pointer far short of
        // the edge never triggers even with extreme travel.
        let reach = EdgeReach::new(2133.0, 3200.0, 1.5);
        assert_eq!(
            reach.apply(3100.0, 500.0, 2133.0 - 250.0, reach_margin(500.0)),
            3100.0
        );
        // Within the cap (margin = 200 px), the reach still fires.
        assert_eq!(
            reach.apply(3150.0, 1000.0, 2133.0 - 100.0, reach_margin(1000.0)),
            3200.0
        );
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
        // edge (delivered position within the reach margin of the surface
        // edge) must land the view exactly on the wall.
        assert_eq!(reach.apply(3180.0, 5.0, 2120.0, reach_margin(13.0)), bounds);
        // Pushing left into the left edge lands on 0.
        assert_eq!(reach.apply(20.0, -5.0, 5.0, reach_margin(0.0)), 0.0);
        // Pushing away from an edge never triggers.
        assert_eq!(
            reach.apply(3180.0, -5.0, 2120.0, reach_margin(100.0)),
            3180.0
        );
        // Hand mid-screen never triggers.
        assert_eq!(
            reach.apply(3180.0, 5.0, 1000.0, reach_margin(100.0)),
            3180.0
        );
        // A view too far from the wall never triggers (no teleports).
        assert_eq!(
            reach.apply(2800.0, 5.0, 2120.0, reach_margin(100.0)),
            2800.0
        );
        // Gliding (no movement this event) never triggers.
        assert_eq!(reach.apply(3180.0, 0.0, 2120.0, reach_margin(0.0)), 3180.0);
    }

    #[test]
    fn reach_wall_edge_is_speed_and_direction_safe() {
        let scale = 1.5;
        let surface = 2133.0;
        let bounds = 3200.0;
        let reach = EdgeReach::new(surface, bounds, scale);
        // Even a tiny push while the hand is jammed at the edge lands on the
        // wall (this is what slow crawling needed before).
        assert_eq!(reach.apply(3199.0, 0.1, 2130.0, reach_margin(0.0)), bounds);
        // The hand within the margin but not pushing: untouched.
        assert_eq!(
            reach.apply(3190.0, 0.0, 2120.0, reach_margin(100.0)),
            3190.0
        );
        // Pushing toward the edge with the hand just outside the margin:
        // untouched (the margin bounds the magnetic feel).
        assert_eq!(reach.apply(3190.0, 5.0, 2100.0, reach_margin(5.0)), 3190.0);
    }

    #[test]
    fn reach_fires_from_stored_direction_after_motion_stops() {
        // The staged design: the motion handler pans/clamps/corrects the
        // view and stores the last per-axis travel; the edge reach is applied
        // on every draw (frame callbacks included) with that *stored*
        // direction. So even if the pointer stops short of the physical edge
        // and no further motion events are delivered, the next draw still
        // lands the view exactly on the wall — the failure mode where a
        // per-event evaluation could miss the final shortfall.
        let scale = 1.5;
        let surface = 2133.0;
        let bounds = 3200.0;
        let reach = EdgeReach::new(surface, bounds, scale);
        // Fast flick right: the last delivered position is 40 logical px
        // short of the edge, but the view must land on the wall on the next
        // draw, driven by the stored (rightward) delta.
        assert_eq!(
            reach.apply(3150.0, 25.0, surface - 40.0, reach_margin(40.0)),
            bounds
        );
        // The stored direction is the *last* one: if the pointer then moved
        // away (leftward stored delta), the same geometry must NOT snap.
        assert_eq!(
            reach.apply(3150.0, -25.0, surface - 40.0, reach_margin(40.0)),
            3150.0
        );
        // Before any motion the stored delta is None (mapped to 0.0 here):
        // no snap, so launch centering on a near-edge pointer position is
        // preserved.
        assert_eq!(
            reach.apply(3150.0, 0.0, surface - 40.0, reach_margin(40.0)),
            3150.0
        );
    }

    #[test]
    fn travel_history_peak_keeps_the_margin_large_through_a_decelerating_flick() {
        // A fast flick to the right edge decelerates: the last delivered
        // events have small travel, but the delivery gap was accumulated at
        // the peak speed frames earlier. The margin must stay large through
        // the whole approach, so it is sized from the recent *peak* travel,
        // not the current event's travel.
        let scale = 1.5;
        let surface = 2133.0;
        let bounds = 3200.0;
        let reach = EdgeReach::new(surface, bounds, scale);
        // The flick's recent history: fast (100 px/event), then decelerating
        // to 2 px/event. The delivered position stops 90 logical px short of
        // the edge.
        let mut hist = TravelHistory::new();
        for _ in 0..2 {
            hist.push((100.0, 0.0));
        }
        for _ in 0..2 {
            hist.push((2.0, 0.0));
        }
        let (peak_x, _) = hist.peak();
        assert_eq!(peak_x, 100.0);
        // Sized from the peak, the margin (158 px) bridges the 90 px gap...
        let margin = reach_margin(peak_x);
        assert!(margin > 90.0, "margin {margin} must cover the 90 px gap");
        assert_eq!(reach.apply(3150.0, 2.0, surface - 90.0, margin), bounds);
        // ...whereas the old per-event margin (11 px) could not.
        let old_margin = reach_margin(2.0);
        assert!(old_margin < 90.0, "old margin {old_margin} too small");
        assert_eq!(reach.apply(3150.0, 2.0, surface - 90.0, old_margin), 3150.0);
    }

    #[test]
    fn travel_history_current_event_always_in_window_covers_long_deceleration() {
        // A long deceleration (more events than the window) flushes the peak,
        // but the gap shrinks with the hand's speed: the delivery gap is at
        // most the travel of the final frame, which IS the current event's
        // travel — always present in the window. So a long deceleration still
        // lands the wall: the margin covers the current event's travel, and
        // the fast-phase peak only adds headroom for multi-frame staleness.
        let scale = 1.5;
        let surface = 2133.0;
        let bounds = 3200.0;
        let reach = EdgeReach::new(surface, bounds, scale);
        // Fast phase, then 10 slow events (the fast phase has left the
        // window; the peak is now the current 2 px/event travel).
        let mut hist = TravelHistory::new();
        for _ in 0..REACH_HISTORY_LEN {
            hist.push((100.0, 0.0));
        }
        for _ in 0..10 {
            hist.push((2.0, 0.0));
        }
        let (peak_x, _) = hist.peak();
        assert_eq!(peak_x, 2.0);
        // The gap the decelerated hand can leave (<= its own final travel,
        // 2 px, margin 11) is still bridged: the current event is in the
        // window, so the margin always covers at least the current speed.
        assert_eq!(
            reach.apply(3194.0, 2.0, surface - 8.0, reach_margin(peak_x)),
            bounds
        );
        // A stale gap larger than the current speed can no longer be bridged
        // once the fast phase flushed — that is the documented limit of the
        // window (the fast phase must be recent for multi-frame staleness).
        assert_eq!(
            reach.apply(3150.0, 2.0, surface - 90.0, reach_margin(peak_x)),
            3150.0
        );
    }

    #[test]
    fn travel_history_peak_decays_once_the_fast_phase_passes() {
        // After the fast phase leaves the window, the peak drops and the
        // edge stops being magnetic: a slow crawl far from the edge is
        // untouched even while pushing toward it.
        let mut hist = TravelHistory::new();
        for _ in 0..REACH_HISTORY_LEN {
            hist.push((100.0, 0.0));
        }
        for _ in 0..REACH_HISTORY_LEN {
            hist.push((1.0, 0.0));
        }
        let (peak_x, _) = hist.peak();
        assert_eq!(peak_x, 1.0);
        let reach = EdgeReach::new(2133.0, 3200.0, 1.5);
        // Crawl 60 logical px short of the edge with a tiny margin: untouched.
        assert_eq!(
            reach.apply(3150.0, 1.0, 2133.0 - 60.0, reach_margin(peak_x)),
            3150.0
        );
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
            view = reach.apply(corrected, step, hand, reach_margin(step));
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

    #[test]
    fn wheel_levels_reach_the_configured_minimum() {
        // The regression the user reported: max zoom 12 with a 1x minimum —
        // the wheel used to bottom out at max/9 = 1.33x and could never
        // reach the 1x minimum. Walking down repeatedly must land exactly on
        // 1x (level 0).
        let mut z = 12.0;
        for _ in 0..20 {
            let next = wheel_levels_next(z, 1.0, 12.0, -1.0);
            if next >= z {
                break;
            }
            z = next;
        }
        assert_eq!(z, 1.0, "wheel must reach the 1x minimum, got {z}");
        // And it stays there (never goes below the minimum).
        assert_eq!(wheel_levels_next(z, 1.0, 12.0, -1.0), 1.0);
        // Zooming back in from the minimum steps onto the first key level.
        assert_eq!(wheel_levels_next(1.0, 1.0, 12.0, 1.0), 12.0 / 9.0);
        // The key levels themselves are unchanged (level 9 = max).
        assert_eq!(wheel_levels_next(12.0, 1.0, 12.0, -1.0), 12.0 * 8.0 / 9.0);
    }

    #[test]
    fn wheel_levels_reach_zero_when_allowed() {
        // With 0 % zoom allowed the wheel walks all the way down to 0.
        let mut z = 12.0;
        for _ in 0..20 {
            let next = wheel_levels_next(z, 0.0, 12.0, -1.0);
            if next >= z {
                break;
            }
            z = next;
        }
        assert_eq!(z, 0.0, "wheel must reach 0% when allowed, got {z}");
        // Zooming in from 0 steps onto the first key level.
        assert_eq!(wheel_levels_next(0.0, 0.0, 12.0, 1.0), 12.0 / 9.0);
    }

    #[test]
    fn wheel_levels_below_min_zoom_out_stays_put() {
        // The 0 key can put the zoom below the wheel's floor when 0 % is not
        // allowed (e.g. fit 0.667 with a 1x wheel minimum). Scrolling out
        // from there must stay put — not snap back up to 1x (which would
        // make scroll-out zoom in); scrolling in returns to the floor.
        let below = 0.667;
        assert_eq!(wheel_levels_next(below, 1.0, 12.0, -1.0), below);
        assert_eq!(wheel_levels_next(below, 1.0, 12.0, 1.0), 1.0);
    }

    #[test]
    fn wheel_levels_never_below_min_and_never_above_max() {
        // Off a level with the wheel, snapping is bounded by the level range.
        assert_eq!(wheel_levels_next(11.0, 1.0, 12.0, 1.0), 12.0); // snaps up to max
        assert_eq!(wheel_levels_next(1.2, 1.0, 12.0, -1.0), 1.0); // snaps down to min
        // A zoom beyond the current max snaps to the top level, not backwards.
        assert_eq!(wheel_levels_next(13.0, 1.0, 12.0, -1.0), 12.0 * 8.0 / 9.0);
    }

    #[test]
    fn fit_zoom_fills_the_viewport_with_the_whole_capture() {
        // A clean 2x scale: the whole screen fits the viewport at 1/2.
        let fit = fit_zoom((3200.0, 2000.0), (1600.0, 1000.0));
        assert!((fit - 0.5).abs() < 1e-9, "fit = {fit}");
        // Never above 1x (a capture smaller than the viewport still fits at
        // 1x — the screen is never shown smaller than the viewport).
        assert_eq!(fit_zoom((50.0, 50.0), (100.0, 100.0)), 1.0);
        // A mismatched aspect ratio: the limiting axis decides, so the whole
        // capture is visible edge-to-edge on that axis.
        let mixed = fit_zoom((3000.0, 1000.0), (2000.0, 1000.0));
        assert!((mixed - 2.0 / 3.0).abs() < 1e-9, "mixed = {mixed}");
    }

    #[test]
    fn zoom_readout_shows_0x_at_fit_and_factor_otherwise() {
        let fit = 2.0 / 3.0;
        // Exactly at the fully-zoomed-out view: reads as 0x.
        assert_eq!(zoom_readout(fit, fit), "0x");
        // A hair above fit still reads as the factor (no premature 0x).
        assert_eq!(zoom_readout(fit + 0.01, fit), "0.68x");
        // Normal magnified zooms read as the factor.
        assert_eq!(zoom_readout(3.0, fit), "3.00x");
        assert_eq!(zoom_readout(1.0, fit), "1.00x");
        // When the whole screen already fills the viewport at 1x (fit == 1),
        // there is no real zoom-out headroom: 1x reads as the factor, not 0x.
        assert_eq!(zoom_readout(1.0, 1.0), "1.00x");
    }

    #[test]
    fn screenshot_rect_normalization_flips_and_clamps() {
        // Dragging up/left yields a normalized rect regardless of drag order.
        let rect = normalize_screenshot_rect((10.0, 20.0), (4.0, 6.0), (320.0, 200.0));
        assert_eq!(rect, (4.0, 6.0, 10.0, 20.0));
        // Dragging outside the capture clamps to the bounds.
        let rect = normalize_screenshot_rect((-5.0, 0.0), (500.0, 300.0), (320.0, 200.0));
        assert_eq!(rect, (0.0, 0.0, 320.0, 200.0));
        // A plain click (no drag) still yields a valid 1px rectangle.
        let rect = normalize_screenshot_rect((100.0, 100.0), (100.0, 100.0), (320.0, 200.0));
        assert_eq!(rect, (100.0, 100.0, 101.0, 101.0));
        // An edge click cannot push the rect out of bounds.
        let rect = normalize_screenshot_rect((319.5, 199.5), (319.5, 199.5), (320.0, 200.0));
        assert_eq!(rect, (319.0, 199.0, 320.0, 200.0));
    }

    #[test]
    fn snap_capture_px_snaps_to_the_magnified_pixel_grid() {
        // Fractional capture positions snap to the nearest whole capture
        // pixel — the grid the user sees as magnified blocks — so a drag
        // always produces an integer-aligned selection.
        assert_eq!(snap_capture_px((10.4, 20.6)), (10.0, 21.0));
        assert_eq!(snap_capture_px((123.7, 55.2)), (124.0, 55.0));
        // Negative values round toward +inf (nearest, not floor): a pointer
        // slightly outside the capture anchors on the edge pixel.
        assert_eq!(snap_capture_px((-0.6, 319.5)), (-1.0, 320.0));
    }

    #[test]
    fn snapped_drag_yields_an_integer_rect() {
        // The drag pipeline the runtime uses: snap both corners, then
        // normalize. The result is integer-valued and clamped to the
        // capture, so the saved crop matches the visually selected region
        // exactly (the save path rounds — here it is a no-op).
        let bounds = (320.0, 200.0);
        let start = snap_capture_px((101.3, 41.7));
        let cur = snap_capture_px((267.9, 168.2));
        let rect = normalize_screenshot_rect(start, cur, bounds);
        assert_eq!(rect, (101.0, 42.0, 268.0, 168.0));
        assert!(rect.0.fract() == 0.0 && rect.1.fract() == 0.0);
        assert!(rect.2.fract() == 0.0 && rect.3.fract() == 0.0);
        // The save region derived from it is exact: no independent rounding
        // drift between position and size.
        let region_x = rect.0.round() as i32;
        let region_y = rect.1.round() as i32;
        let region_w = (rect.2 - rect.0).round() as u32;
        let region_h = (rect.3 - rect.1).round() as u32;
        assert_eq!(
            (region_x, region_y, region_w, region_h),
            (101, 42, 167, 126)
        );
    }

    #[test]
    fn active_screenshot_border_picks_closest_edge() {
        let rect = (0.0, 0.0, 100.0, 100.0);
        // Near the middle: ties break left < right < top < bottom, so the
        // exact center selects the left border.
        assert_eq!(
            active_screenshot_border(rect, (50.0, 50.0)),
            ScreenshotBorder::Left
        );
        assert_eq!(
            active_screenshot_border(rect, (99.0, 50.0)),
            ScreenshotBorder::Right
        );
        assert_eq!(
            active_screenshot_border(rect, (50.0, 1.0)),
            ScreenshotBorder::Top
        );
        assert_eq!(
            active_screenshot_border(rect, (50.0, 98.0)),
            ScreenshotBorder::Bottom
        );
        // A corner-ish point: left edge wins over top when horizontally closer.
        assert_eq!(
            active_screenshot_border(rect, (2.0, 20.0)),
            ScreenshotBorder::Left
        );
        // A wide rectangle: a cursor far to the right of the rect must pick
        // the short right edge (visible segment), NOT the long top/bottom
        // edges — the distance to a segment is what counts, not the distance
        // to the infinite lines through the top/bottom edges.
        let wide = (0.0, 0.0, 2000.0, 100.0);
        assert_eq!(
            active_screenshot_border(wide, (2500.0, 50.0)),
            ScreenshotBorder::Right
        );
        assert_eq!(
            active_screenshot_border(wide, (-500.0, 50.0)),
            ScreenshotBorder::Left
        );
        // Far above a wide rect: the top edge is closest.
        assert_eq!(
            active_screenshot_border(wide, (1000.0, -500.0)),
            ScreenshotBorder::Top
        );
        // Inside a wide rect near its left edge: left wins.
        assert_eq!(
            active_screenshot_border(wide, (10.0, 50.0)),
            ScreenshotBorder::Left
        );
    }
    #[test]
    fn nudge_screenshot_border_moves_active_edge_1px_and_clamps() {
        let bounds = (100.0, 100.0);
        // Top border: W moves the top edge up, S moves it down.
        let r = (10.0, 20.0, 90.0, 80.0);
        assert_eq!(
            nudge_screenshot_border(r, ScreenshotBorder::Top, 'w', bounds),
            (10.0, 19.0, 90.0, 80.0)
        );
        assert_eq!(
            nudge_screenshot_border(r, ScreenshotBorder::Top, 's', bounds),
            (10.0, 21.0, 90.0, 80.0)
        );
        // Bottom border: W shrinks up (y1 decreases), S extends down.
        assert_eq!(
            nudge_screenshot_border(r, ScreenshotBorder::Bottom, 'w', bounds),
            (10.0, 20.0, 90.0, 79.0)
        );
        assert_eq!(
            nudge_screenshot_border(r, ScreenshotBorder::Bottom, 's', bounds),
            (10.0, 20.0, 90.0, 81.0)
        );
        // Left border: A extends left, D shrinks right.
        assert_eq!(
            nudge_screenshot_border(r, ScreenshotBorder::Left, 'a', bounds),
            (9.0, 20.0, 90.0, 80.0)
        );
        assert_eq!(
            nudge_screenshot_border(r, ScreenshotBorder::Left, 'd', bounds),
            (11.0, 20.0, 90.0, 80.0)
        );
        // Right border: A shrinks left, D extends right.
        assert_eq!(
            nudge_screenshot_border(r, ScreenshotBorder::Right, 'a', bounds),
            (10.0, 20.0, 89.0, 80.0)
        );
        assert_eq!(
            nudge_screenshot_border(r, ScreenshotBorder::Right, 'd', bounds),
            (10.0, 20.0, 91.0, 80.0)
        );
        // Clamps: a border can never move past its opposite edge (min 1px) or
        // outside the capture.
        assert_eq!(
            nudge_screenshot_border((0.0, 0.0, 1.0, 1.0), ScreenshotBorder::Left, 'a', bounds),
            (0.0, 0.0, 1.0, 1.0)
        );
        assert_eq!(
            nudge_screenshot_border((0.0, 0.0, 1.0, 1.0), ScreenshotBorder::Left, 'd', bounds),
            (0.0, 0.0, 1.0, 1.0)
        );
        assert_eq!(
            nudge_screenshot_border(
                (99.0, 99.0, 100.0, 100.0),
                ScreenshotBorder::Bottom,
                's',
                bounds
            ),
            (99.0, 99.0, 100.0, 100.0)
        );
    }

    #[test]
    fn nudge_off_axis_translates_whole_rectangle() {
        let bounds = (100.0, 100.0);
        let r = (10.0, 20.0, 90.0, 80.0);
        // A/D on a horizontal (top) border translate the whole rect
        // horizontally, preserving its size.
        assert_eq!(
            nudge_screenshot_border(r, ScreenshotBorder::Top, 'a', bounds),
            (9.0, 20.0, 89.0, 80.0)
        );
        assert_eq!(
            nudge_screenshot_border(r, ScreenshotBorder::Top, 'd', bounds),
            (11.0, 20.0, 91.0, 80.0)
        );
        // W/S on a vertical (left) border translate it vertically.
        assert_eq!(
            nudge_screenshot_border(r, ScreenshotBorder::Left, 'w', bounds),
            (10.0, 19.0, 90.0, 79.0)
        );
        assert_eq!(
            nudge_screenshot_border(r, ScreenshotBorder::Left, 's', bounds),
            (10.0, 21.0, 90.0, 81.0)
        );
        // At the capture edge a translate simply does not move (never shrinks).
        assert_eq!(
            nudge_screenshot_border((0.0, 20.0, 80.0, 80.0), ScreenshotBorder::Top, 'a', bounds),
            (0.0, 20.0, 80.0, 80.0)
        );
        assert_eq!(
            nudge_screenshot_border(
                (20.0, 20.0, 100.0, 80.0),
                ScreenshotBorder::Bottom,
                'd',
                bounds
            ),
            (20.0, 20.0, 100.0, 80.0)
        );
    }

    #[test]
    fn screenshot_overlay_dims_outside_and_draws_border() {
        let color = [255, 153, 0];
        // 8x8 overlay, 2px border around rect (2,2)-(8,8) -> transparent
        // interior (4,4)-(6,6).
        let mut ov = vec![0u8; 8 * 8 * 4];
        fill_screenshot_overlay(&mut ov, 8, 8, Some((2.0, 2.0, 8.0, 8.0)), color, 2, None);
        let px = |x: usize, y: usize| {
            let i = (y * 8 + x) * 4;
            [ov[i], ov[i + 1], ov[i + 2], ov[i + 3]]
        };
        // Inside the selection (away from the border): fully transparent.
        assert_eq!(px(4, 4), [0, 0, 0, 0]);
        // On the border: opaque orange.
        assert_eq!(px(2, 4), [color[0], color[1], color[2], 255]);
        assert_eq!(px(7, 3), [color[0], color[1], color[2], 255]);
        // Outside the selection: fully transparent (no dimming — the
        // magnified screen stays opaque and undimmed).
        assert_eq!(px(0, 0), [0, 0, 0, 0]);
        // No rect: the whole overlay stays fully transparent.
        let mut ov2 = vec![0u8; 8 * 8 * 4];
        fill_screenshot_overlay(&mut ov2, 8, 8, None, color, 2, None);
        assert!(ov2.iter().all(|&b| b == 0));
    }

    #[test]
    fn screenshot_overlay_highlights_active_border() {
        let color = [255, 153, 0];
        // 12x12 overlay, 2px border around rect (2,2)-(10,10); the top edge
        // is active, so its band is 4px tall and lightened toward white.
        let mut ov = vec![0u8; 12 * 12 * 4];
        fill_screenshot_overlay(
            &mut ov,
            12,
            12,
            Some((2.0, 2.0, 10.0, 10.0)),
            color,
            2,
            Some(ScreenshotBorder::Top),
        );
        let px = |x: usize, y: usize| {
            let i = (y * 12 + x) * 4;
            [ov[i], ov[i + 1], ov[i + 2], ov[i + 3]]
        };
        // Active band is 4px (2*2): rows 2..6 at x=3 are the lightened color.
        let active = [
            255u8,
            (153u16 + (255 - 153) * 3 / 5) as u8,
            (255 * 3 / 5) as u8,
            255,
        ];
        assert_eq!(px(3, 2), active);
        assert_eq!(px(3, 5), active);
        // Row 6 is past the active band: normal border color.
        assert_eq!(px(3, 6), [color[0], color[1], color[2], 255]);
        // The inactive right border stays 2px and normal-colored (row 7 is
        // below the active top band).
        assert_eq!(px(9, 7), [color[0], color[1], color[2], 255]);
        // Interior next to it stays transparent.
        assert_eq!(px(7, 7), [0, 0, 0, 0]);
    }

    #[test]
    fn screenshot_overlay_narrow_rect_does_not_panic() {
        // Regression: a selection narrower than its own combined border
        // widths (or a 1px click rect) used to make the interior fill
        // invert and panic on a slice index, and full-width border bands
        // drew lines across the whole screen. It must render a small
        // outlined box instead.
        let color = [255, 153, 0];
        // 1px-wide, 6px-tall rect at (3,1): the horizontal bands would
        // previously span the whole 8px canvas width.
        let mut ov = vec![0u8; 8 * 8 * 4];
        fill_screenshot_overlay(&mut ov, 8, 8, Some((3.0, 1.0, 4.0, 7.0)), color, 2, None);
        let px = |x: usize, y: usize| {
            let i = (y * 8 + x) * 4;
            [ov[i], ov[i + 1], ov[i + 2], ov[i + 3]]
        };
        // A border pixel: orange.
        assert_eq!(px(3, 1), [color[0], color[1], color[2], 255]);
        assert_eq!(px(3, 6), [color[0], color[1], color[2], 255]);
        // A row far from the rect's x-range is transparent (no scrim), NOT a
        // full-width horizontal border line.
        assert_eq!(px(7, 1), [0, 0, 0, 0]);
        assert_eq!(px(7, 6), [0, 0, 0, 0]);
        // A column far from the rect's y-range is transparent, NOT a
        // full-height vertical border line.
        assert_eq!(px(3, 0), [0, 0, 0, 0]);
        assert_eq!(px(3, 7), [0, 0, 0, 0]);
    }

    #[test]
    fn toggle_screenshot_scale_flips_between_real_and_magnified() {
        use crate::config::ScreenshotScale;
        assert_eq!(
            toggle_screenshot_scale(ScreenshotScale::Real),
            ScreenshotScale::Magnified
        );
        assert_eq!(
            toggle_screenshot_scale(ScreenshotScale::Magnified),
            ScreenshotScale::Real
        );
    }

    #[test]
    fn advance_repeat_deadline_steps_whole_intervals() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        // 10 ms late: steps one whole 33 ms interval.
        let d1 = advance_repeat_deadline(
            t0 + Duration::from_millis(10),
            t0,
            Duration::from_millis(33),
        );
        assert_eq!(d1.duration_since(t0), Duration::from_millis(33));
        // 100 ms late (3+ intervals): advances past now, not one behind.
        let late = t0 + Duration::from_millis(100);
        let d2 = advance_repeat_deadline(late, t0, Duration::from_millis(33));
        assert!(d2 > late);
        assert!(d2.duration_since(late) <= Duration::from_millis(33));
        // Not yet due: unchanged.
        assert_eq!(
            advance_repeat_deadline(
                t0,
                t0 + Duration::from_millis(50),
                Duration::from_millis(33)
            ),
            t0 + Duration::from_millis(50)
        );
    }

    #[test]
    fn upscale_nearest_scales_rgba_correctly() {
        // 2x1 image: red pixel (255,0,0,255) then blue (0,0,255,255).
        let src: Vec<u8> = vec![255, 0, 0, 255, 0, 0, 255, 255];
        let (out, w, h) = upscale_nearest(&src, 2, 1, 3.0);
        assert_eq!((w, h), (6, 3));
        // Row 0: three red then three blue.
        let px = |x: usize, y: usize| {
            let i = (y * 6 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };
        assert_eq!(px(0, 0), [255, 0, 0, 255]);
        assert_eq!(px(2, 0), [255, 0, 0, 255]);
        assert_eq!(px(3, 0), [0, 0, 255, 255]);
        assert_eq!(px(5, 0), [0, 0, 255, 255]);
        // All rows are the same (vertical nearest neighbor).
        assert_eq!(px(1, 2), [255, 0, 0, 255]);
        assert_eq!(px(4, 2), [0, 0, 255, 255]);
        // Scale below 1 is clamped: the output is never smaller than 1 px.
        let (out2, w2, h2) = upscale_nearest(&src, 2, 1, 0.5);
        assert_eq!((w2, h2), (1, 1));
        assert_eq!(out2, vec![255, 0, 0, 255]);
        // Non-integer scale (1.5) rounds the output size up.
        let (out3, w3, h3) = upscale_nearest(&src, 2, 1, 1.5);
        assert_eq!((w3, h3), (3, 2));
        let px3 = |x: usize, y: usize| {
            let i = (y * 3 + x) * 4;
            [out3[i], out3[i + 1], out3[i + 2], out3[i + 3]]
        };
        assert_eq!(px3(0, 0), [255, 0, 0, 255]);
        assert_eq!(px3(2, 0), [0, 0, 255, 255]);
    }

    #[test]
    fn screenshot_overlay_blends_into_canvas() {
        let mut canvas = vec![255u8; 4 * 4 * 4]; // opaque white canvas
        let mut ov = vec![0u8; 4 * 4 * 4];
        // Opaque orange border pixel at (1,1) and a translucent black pixel
        // at (0,0) of a 4-wide RGBA overlay.
        let i_border = 20; // (1 * 4 + 1) * 4
        ov[i_border..i_border + 4].copy_from_slice(&[255, 153, 0, 255]);
        let i_scrim = 0; // pixel (0,0)
        ov[i_scrim..i_scrim + 4].copy_from_slice(&[0, 0, 0, 128]);
        blend_overlay_into(&mut canvas, 4, &ov, 4, 4);
        let border_px = &canvas[i_border..i_border + 4];
        assert_eq!(border_px, [255, 153, 0, 255]);
        let scrim_px = &canvas[0..4];
        // White * 0.5 + black * 0.5 = ~128 gray.
        assert!(
            scrim_px[0] > 100 && scrim_px[0] < 160,
            "gray {}",
            scrim_px[0]
        );
    }

    #[test]
    fn zero_key_always_selects_zero_zoom() {
        // State-level: the 0 key selects the minimum (0, which the engine
        // maps to the fully-zoomed-out view at runtime) regardless of the
        // allow-zero setting.
        let config = MagnifierConfig {
            allow_zero_zoom: false,
            ..MagnifierConfig::default()
        };
        let mut state = MagnifierState::new(config, Some(3.0));
        state.handle_zoom_key(0);
        assert_eq!(state.zoom, 0.0, "key 0 must always select the minimum");
    }

    #[test]
    fn allow_zero_lets_key_levels_go_below_1x() {
        // With 0 % zoom allowed the 1-9 keys may go sub-1x (each key is a
        // fraction of max), instead of being clamped to 1x.
        let config = MagnifierConfig {
            max_zoom: 4.0,
            allow_zero_zoom: true,
            ..MagnifierConfig::default()
        };
        let mut state = MagnifierState::new(config, Some(3.0));
        state.handle_zoom_key(1);
        assert!(
            (state.zoom - 4.0 / 9.0).abs() < 1e-9,
            "key 1 = max/9 below 1x when allowed, got {}",
            state.zoom
        );
        state.handle_zoom_key(9);
        assert_eq!(state.zoom, 4.0);
    }

    #[test]
    fn state_launches_at_zero_default_when_allowed() {
        // A default zoom of 0 % (with the allow-zero setting enabled by the
        // config normalization) starts the state at 0; once the capture
        // arrives the engine clamps it up to the fully-zoomed-out view.
        let config = MagnifierConfig {
            default_zoom: Some(0.0),
            allow_zero_zoom: true,
            ..MagnifierConfig::default()
        };
        let state = MagnifierState::new(config, None);
        assert_eq!(state.zoom, 0.0);
    }

    /// Regression test for the magnified-cursor blit: `draw_cursor_at` must
    /// draw the reticle (white ring + black center) at the requested position
    /// without mirroring, and never write outside the canvas.
    fn canvas_at(canvas: &[u8], stride: i32, x: i32, y: i32) -> Option<[u8; 4]> {
        if x < 0 || y < 0 || x >= stride / 4 || y >= canvas.len() as i32 / stride {
            return None;
        }
        let i = y as usize * stride as usize + x as usize * 4;
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
