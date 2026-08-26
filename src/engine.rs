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
use smithay_client_toolkit::seat::pointer_constraints::PointerConstraintsHandler;
use smithay_client_toolkit::seat::pointer_constraints::PointerConstraintsState;
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

use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_confined_pointer_v1::ZwpConfinedPointerV1, zwp_pointer_constraints_v1::Lifetime,
};
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
use crate::osd::OsdSprite;
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

/// Dead zone (logical px) for hold-to-zoom zoom-in from the floor: while the
/// zoom sits exactly on the runtime minimum (the fully-zoomed-out "0 %"
/// view), tiny alternating pointer jitter must not flap the zoom between the
/// floor and one step above it. Upward motion is ignored until it has
/// accumulated past this many px, then zoom-in proceeds normally.
const HTZ_FLOOR_DEADZONE: f64 = 2.0;

/// The hold-to-zoom zoom step at/around the runtime minimum. While the zoom
/// sits exactly on the floor (the fully-zoomed-out "0 %" view), zoom-out
/// holds the floor rock-solid and zoom-in is ignored until the upward motion
/// has accumulated past [`HTZ_FLOOR_DEADZONE`] px (then it proceeds
/// normally) — a period-2 jitter (one px down, one px up, …) at the floor
/// used to flap the zoom between the floor and one step above it. Below the
/// floor (the `0` key when 0 % is not allowed) zoom-out stays put and
/// zoom-in returns to the floor. Returns `(new_zoom, new_dead_travel)`.
fn htz_floor_zoom(
    zoom: f64,
    min: f64,
    max: f64,
    dy: f64,
    speed: f64,
    dead_travel: f64,
) -> (f64, f64) {
    let at_floor = (zoom - min).abs() <= 1e-9;
    if at_floor && dy > 0.0 {
        // Zooming out at the floor: hold it exactly (and re-arm the dead
        // zone — any upward jitter since the last down-tick is discarded).
        (min, 0.0)
    } else if at_floor && dy < 0.0 {
        // Zooming in from the floor: swallow tremor up to the dead zone.
        let travel = dead_travel + (-dy);
        if travel >= HTZ_FLOOR_DEADZONE {
            ((zoom - dy * speed).clamp(min, max), 0.0)
        } else {
            (min, travel)
        }
    } else if zoom < min && dy > 0.0 {
        // Below the floor (the 0 key with 0 % not allowed): zooming out
        // stays put.
        (zoom, 0.0)
    } else {
        ((zoom - dy * speed).clamp(min, max), 0.0)
    }
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

/// How close (capture px) the view must already be to a wall for the
/// wall-hold to engage, and how close (logical px) the pointer must be to
/// the corresponding physical screen edge. Both are deliberately tiny and
/// **constant** (not speed-scaled), so the hold only fires while the user
/// is physically parked at the very edge — it never grabs the view from a
/// distance, unlike the removed speed-scaled magnetic reach. It suppresses
/// the sub-pixel shiver at the **right/bottom** edges: there the resting
/// delivered pointer position oscillates *below* the surface edge, which
/// (scaled up by the capture scale) made the quantized integer view center
/// flip between `wall` and `wall−1`, jittering the magnified content and
/// the cursor/OSD position while the user keeps pushing into the edge.
const WALL_HOLD_EPS: f64 = 2.0;
const WALL_HOLD_MARGIN_LOGICAL: f64 = 4.0;

/// Non-magnetic edge-hold with **hysteresis** for one axis. It pins the view
/// to the *capture edge* (0 on the low edge, `capture` on the high edge)
/// while the pointer is parked there and the view is already within
/// [`WALL_HOLD_EPS`] of it. Pinning to the capture edge (not `capture − 1`)
/// makes the content's edge — the beyond-capture boundary, which renders
/// exactly at the viewport center — land flush on the magnified cursor's
/// apex when pushed to the limit, so the screen edge and the cursor tip are
/// perfectly aligned at every wall. Crucially the hold is **latched**
/// via `held` (the side currently latched, or `None`): once engaged it stays
/// put until the pointer actually moves more than
/// [`WALL_HOLD_MARGIN_LOGICAL`] px *away* from that edge. Without the latch,
/// a parked pointer's micro-wobble pans the view from the pinned position by
/// a fraction of a pixel, which the integer quantization turns into a hop
/// from the last real pixel to `capture − 2` — just outside the epsilon
/// band — so the hold disengages and the view flip-flops (the bottom/right
/// shiver). The latch prevents exactly that. Releasing (moving the pointer
/// off the edge) returns it to `None` so normal panning resumes; it never
/// grabs the view from a distance (engage still requires the view to already
/// be within [`WALL_HOLD_EPS`] of the edge).
fn edge_hold_axis(
    view: f64,
    pointer: f64,
    surface: f64,
    capture: f64,
    held: Option<bool>,
) -> (f64, Option<bool>) {
    let at_high = (pointer - surface).abs() <= WALL_HOLD_MARGIN_LOGICAL;
    let at_low = pointer.abs() <= WALL_HOLD_MARGIN_LOGICAL;
    match held {
        // Latched high: keep pinning until the pointer leaves the high edge;
        // leaving releases (Option -> None) so normal panning resumes.
        Some(true) => {
            if at_high {
                (capture, Some(true))
            } else {
                (view, None)
            }
        }
        // Latched low: symmetric to the high edge.
        Some(false) => {
            if at_low {
                (0.0, Some(false))
            } else {
                (view, None)
            }
        }
        // Not latched: engage when the pointer is parked at an edge AND the
        // view is already near it (never a grab-from-a-distance).
        None => {
            if at_high && (view - capture).abs() <= WALL_HOLD_EPS {
                (capture, Some(true))
            } else if at_low && view.abs() <= WALL_HOLD_EPS {
                (0.0, Some(false))
            } else {
                (view, None)
            }
        }
    }
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
/// is reached exactly, at any speed. Moving away from the hand content
/// does **not** heal at all: the view pans 1:1 with the hand, so a huge
/// residual after a deep hold-to-zoom drag can never stick the view or
/// drive it against the drag direction (it used to reverse the pan —
/// mouse-down moved the view up, the same direction as mouse-up — and a
/// later bounded heal left it momentarily stuck). The residual is erased
/// only by the toward-content catch-up; both walls stay reachable
/// regardless (the plain pan clamps the view to the capture at the far
/// edge). Returns the remaining offset.
fn offset_correction_step(
    offset: (f64, f64),
    dt: f64,
    travel: (f64, f64),
    scale: (f64, f64),
) -> (f64, f64) {
    let f = offset_correction_factor(dt);
    let heal = |o: f64, t: f64, s: f64| {
        if o * t >= 0.0 {
            // Moving away from the hand content (or gliding): no correction
            // — the view pans 1:1 with the hand, so a large residual can
            // never stick the view or reverse its direction.
            return 0.0;
        }
        // Pushing toward the hand content: catch up at least as fast as the
        // hand travels (capped at 2× the hand's speed, never overshooting
        // the hand content), so the far wall is reached exactly, at any
        // speed, and the residual is erased during the push itself.
        let lim = t.abs() * s * 2.0;
        let mut corr = (o * f).clamp(-lim, lim);
        let catch = t.abs() * s;
        corr = if corr >= 0.0 {
            corr.max(catch)
        } else {
            corr.min(-catch)
        };
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

/// How long the pointer must be still before the frozen screen's sampling
/// origin settles onto the cursor's lattice (see
/// [`snap_src_to_cursor_lattice`] and `draw_frame`). While the pointer
/// moves, the rendered origin uses the fine physical-pixel snap so panning
/// stays smooth and the cursor sits exactly at the dead center (it never
/// moves on its own); once the pointer rests for this long, the sampling
/// origin is shifted by at most half a capture pixel so the magnified
/// screen's texel grid coincides with the cursor's fixed grid — the screen's
/// blocks and the cursor's blocks share one crisp lattice exactly when the
/// user is inspecting, with the cursor still at the dead center. The settle
/// is a single sub-block nudge of the *frozen image* per stop, never a
/// movement of the cursor and never an animation.
const CURSOR_SETTLE_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// Snap a view origin (capture px) so the capture-texel lattice coincides
/// with a fixed cursor lattice (whose sprite origin is `cursor_origin` in
/// render-buffer px): texel boundary `i` starts at the same buffer-pixel
/// boundary as cursor block `(cursor_origin + (i - i0) * px_per_texel)` for
/// some `i0`, i.e. `(i - src) * px_per_texel - 0.5 ≡ cursor_origin
/// (mod px_per_texel)` — the screen's blocks and the cursor's blocks are
/// flush, like two layers in a bitmap editor, while the cursor itself stays
/// at the dead center. The origin moves by at most half a capture pixel
/// (sub-block), so the frozen content shifts imperceptibly when the settle
/// engages. This is the "align on launch" idea applied continuously: the
/// alignment holds whenever the pointer is still.
fn snap_src_to_cursor_lattice(src: f64, cursor_origin: f64, px_per_texel: f64) -> f64 {
    let target_frac = ((-cursor_origin - 0.5) / px_per_texel).rem_euclid(1.0);
    let raw_frac = src.rem_euclid(1.0);
    let mut delta = target_frac - raw_frac;
    if delta > 0.5 {
        delta -= 1.0;
    } else if delta < -0.5 {
        delta += 1.0;
    }
    src + delta
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
    let mut next = if on_level {
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
    // When max/LEVELS equals min_zoom (e.g. max=9, min=1 → level 0 and
    // level 1 are both 1×), stepping from level 0 to level 1 produces the
    // same zoom and the "no-change" guard blocks the wheel. Skip levels
    // that don't actually change the zoom until one does (capped at LEVELS).
    if steps > 0.0 {
        let mut candidate = next;
        while candidate <= LEVELS && (max_zoom * candidate / LEVELS - zoom).abs() < 1e-9 {
            candidate += 1.0;
        }
        next = candidate.min(LEVELS);
    }
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

/// Snap a view origin (capture px) so the magnified screen's texel grid lands
/// exactly on the render buffer's pixel grid. `px_per_texel` is the number of
/// buffer px one capture texel spans: `RENDER_SCALE * zoom` on the GPU path
/// (the buffer is RENDER_SCALE times the logical size), `zoom` on the CPU
/// path. The sampling origin has an arbitrary fractional part because the
/// view pans continuously, and an unsnapped origin puts texel boundaries at
/// fractional buffer-pixel positions — the magnified pixel blocks render at
/// uneven, shifting widths and never line up with the cursor sprite (or the
/// display's physical pixels). Locking the phase to half a texel places every
/// texel boundary on an exact buffer-pixel boundary (when `px_per_texel` is
/// integral), so the magnified grid and the cursor sprite share one crisp,
/// stationary lattice. The snap never moves the origin by more than half a
/// texel, and it only affects the *rendered* grid — the view center (cursor
/// content, readout, panning math) stays untouched.
fn snap_render_origin(origin: f64, px_per_texel: f64) -> f64 {
    if !px_per_texel.is_finite() || px_per_texel <= 0.0 {
        return origin;
    }
    let t = origin * px_per_texel;
    ((t - 0.5).round() + 0.5) / px_per_texel
}

/// Quantize the view center (capture px) to the capture's pixel grid and
/// clamp it inside the capture. This is the "snap at launch and that's it"
/// pixel-grid lock: rounding the center to the nearest *integer capture
/// pixel* makes the capture pixel under the magnified cursor an exact
/// integer, so the cursor's texels coincide with the screen's texels (both
/// lattices share the viewport center as a common point and have the same
/// period `RENDER_SCALE × zoom`) — permanently, at every zoom, with the
/// cursor fixed at the dead center and nothing ever adjusting itself
/// afterwards. The cost is that the content can only slide in whole-texel
/// steps (one capture px = one magnified block), which is exactly what makes
/// the alignment stable: sub-texel panning is what kept breaking the phase.
/// Clamping after the round keeps the "cursor can always reach the exact
/// edge and never leaves the capture" invariant (see `clamp_to_capture`).
fn quantize_center_to_pixel_grid(center: (f64, f64), capture: (f64, f64)) -> (f64, f64) {
    (
        center.0.round().clamp(0.0, capture.0),
        center.1.round().clamp(0.0, capture.1),
    )
}

/// The pan-tuning gain applied to pointer-motion deltas: `zoom^-tuning`
/// (`1.0` when tuning is 0 or the zoom is degenerate). At high zoom the
/// gain is small — you move the mouse further to pan from one magnified
/// pixel (texel) to the next — and below 1× the gain exceeds 1, so a short
/// nudge travels further when zoomed out (the "vice versa" the user asked
/// for). See `pan_tuning` in the config.
fn pan_tuning_gain(zoom: f64, tuning: f64) -> f64 {
    if tuning <= 0.0 || zoom <= 0.0 {
        1.0
    } else {
        zoom.powf(-tuning)
    }
}

/// The minimap rectangle in logical viewport px: a small overview of the
/// whole viewport, including the black space beyond the captured frame, pinned
/// to the configured corner. The rectangle follows the viewport aspect ratio;
/// the capture is fitted inside it and letterboxed with black space.
/// Half-size (capture px) of the region scrubbed around the launch pointer
/// position when building the minimap base: the frozen frame can contain the
/// launching app's *own* cursor graphic (XWayland / software cursors are
/// rendered into the app's surface and cannot be excluded by the screencopy's
/// `overlay_cursor = 0`), and scrubbing it before the downscale keeps the
/// minimap free of a stray miniature cursor next to the marker dot. The
/// scrub only affects the minimap base, never the magnified view itself.
const CURSOR_BAKE_HALF: i32 = 24;
/// Minimum width/height (minimap px) for the amber visible-region outline to
/// be drawn: below this it would hide behind the marker dot (whose outer
/// diameter is ~8 px), leaving only stray corner pixels poking out — the
/// "single pixel following the cursor" artifact at deep zoom.
const MINIMAP_MARKER_MIN_EDGE: i32 = 12;
/// Length (minimap px) of each leg of the amber corner brackets marking the
/// visible region. The brackets are short L-shapes at the four corners of
/// the region (like camera viewfinder brackets), not full solid edges —
/// less obtrusive. On very small regions they shrink to half the edge.
const MINIMAP_CORNER_TICK: i32 = 7;
/// Supersampling grid used to rasterize the rounded stroke evenly at corners.
/// 8×8 = 64 sub-samples per pixel — enough for buttery-smooth corner arcs.
const MINIMAP_MASK_SAMPLES: i32 = 8;
const MINIMAP_PULSE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
const MINIMAP_CORNER_RADIUS: i32 = 8;

fn minimap_layout(
    viewport: (f64, f64),
    capture: (f64, f64),
    margin: f64,
    corner: crate::osd::Corner,
) -> (f64, f64, f64, f64) {
    let w = (viewport.0 * 0.22).round().clamp(140.0, 360.0);
    let h = (w * capture.1 / capture.0.max(1.0)).round().max(40.0);
    let (x, y) = corner.position(
        viewport.0 as i32,
        viewport.1 as i32,
        w as i32,
        h as i32,
        margin as i32,
    );
    (x as f64, y as f64, w, h)
}

/// Seconds elapsed since program launch, used by all outline animation
/// schemes so they share a common clock.
fn outline_elapsed() -> f32 {
    use std::sync::OnceLock;
    static LAUNCH_TIME: OnceLock<std::time::Instant> = OnceLock::new();
    let start = LAUNCH_TIME.get_or_init(std::time::Instant::now);
    start.elapsed().as_secs_f32()
}

/// The visible magnified region in capture px (the source rect of the view,
/// which may extend past the capture edges near the screen — the marker is
/// clamped to the capture in the drawing code). `None` when the zoom is
/// degenerate (0 or negative), so no outline is drawn.
fn minimap_outline_color(speed: f64) -> [u8; 3] {
    let t = outline_elapsed() * speed as f32;
    // A fully opaque, continuously moving RGB gradient. The pulse changes
    // the brightness without changing alpha, so the outline never becomes
    // translucent or disappears.
    let pulse = 0.65 + 0.35 * (t * std::f32::consts::TAU / 1.6).sin();
    let phase = t * std::f32::consts::TAU / 3.2;
    [
        (255.0 * pulse * (0.5 + 0.5 * phase.sin())) as u8,
        (255.0 * pulse * (0.5 + 0.5 * (phase + 2.094).sin())) as u8,
        (255.0 * pulse * (0.5 + 0.5 * (phase + 4.188).sin())) as u8,
    ]
}

/// Per-pixel color for the angular gradient scheme: a 45-degree angle
/// gradient that slides along the outline over time. The hue rotates with
/// the projection of the pixel position onto the 45° axis, offset by time.
fn minimap_angular_gradient_color(x: i32, y: i32, w: f64, h: f64, speed: f64) -> [u8; 3] {
    let t = outline_elapsed() * speed as f32;
    let cos45 = std::f32::consts::FRAC_1_SQRT_2;
    let sin45 = cos45;
    // Project pixel onto the 45° axis, normalized to [0, 1] across the
    // diagonal of the minimap, then offset by time to make it slide.
    let proj = (x as f32 * cos45 + y as f32 * sin45)
        / ((w as f32 + h as f32) * cos45).max(1.0);
    let phase = (proj + t * 0.15) * std::f32::consts::TAU;
    [
        (127.5 + 127.5 * phase.sin()) as u8,
        (127.5 + 127.5 * (phase + 2.094).sin()) as u8,
        (127.5 + 127.5 * (phase + 4.188).sin()) as u8,
    ]
}

/// Perimeter parameter (0.0..1.0) for a point near a rounded rectangle's
/// boundary. The parameter increases clockwise and is linear in arc/edge
/// length, so equal increments correspond to equal distances along the
/// perimeter — unlike angle-from-center which compresses corners.
fn perimeter_param(x: f64, y: f64, w: f64, h: f64, r: f64) -> f64 {
    let straight_h = (w - 2.0 * r).max(0.0);
    let straight_v = (h - 2.0 * r).max(0.0);
    let arc = std::f64::consts::FRAC_PI_2 * r;
    let total = 2.0 * straight_h + 2.0 * straight_v + 4.0 * arc;
    if total <= 0.0 {
        return 0.0;
    }
    let mut best_dist = f64::MAX;
    let mut best_param = 0.0f64;
    // --- straight edges ---
    // Top edge: (r, 0) → (w-r, 0)
    {
        let nx = x.clamp(r, w - r);
        let d = (x - nx).hypot(y);
        if d < best_dist {
            best_dist = d;
            best_param = nx - r;
        }
    }
    // Right edge: (w, r) → (w, h-r)
    {
        let ny = y.clamp(r, h - r);
        let d = (x - w).hypot(y - ny);
        if d < best_dist {
            best_dist = d;
            best_param = straight_h + arc + (ny - r);
        }
    }
    // Bottom edge: (w-r, h) → (r, h)
    {
        let nx = x.clamp(r, w - r);
        let d = (x - nx).hypot(y - h);
        if d < best_dist {
            best_dist = d;
            best_param = straight_h + arc + straight_v + arc + (w - r - nx);
        }
    }
    // Left edge: (0, h-r) → (0, r)
    {
        let ny = y.clamp(r, h - r);
        let d = x.hypot(y - ny);
        if d < best_dist {
            best_dist = d;
            best_param = 2.0 * straight_h + 3.0 * arc + straight_v + (h - r - ny);
        }
    }
    // --- corner arcs (center, entry_angle, cumulative distance to entry) ---
    let corners: [(f64, f64, f64, f64); 4] = [
        (w - r, r, -std::f64::consts::FRAC_PI_2, straight_h),
        (w - r, h - r, 0.0, straight_h + arc + straight_v),
        (r, h - r, std::f64::consts::FRAC_PI_2, 2.0 * straight_h + 2.0 * arc + straight_v),
        (r, r, std::f64::consts::PI, 2.0 * straight_h + 3.0 * arc + 2.0 * straight_v),
    ];
    for &(ccx, ccy, entry_angle, base_param) in &corners {
        let dx = x - ccx;
        let dy = y - ccy;
        let dist = (dx * dx + dy * dy).sqrt();
        if dist >= r + 2.0 {
            continue;
        }
        let angle = dy.atan2(dx);
        // Normalise both angles into [0, 2π) so arc distance wraps correctly.
        let norm_a = if angle < 0.0 { angle + std::f64::consts::TAU } else { angle };
        let norm_e = if entry_angle < 0.0 {
            entry_angle + std::f64::consts::TAU
        } else {
            entry_angle
        };
        let mut arc_dist = (norm_a - norm_e) * r;
        if arc_dist < -0.1 {
            arc_dist += std::f64::consts::TAU * r;
        }
        if arc_dist >= -0.1 && arc_dist <= arc + 0.1 {
            let d = (dist - r).abs();
            if d < best_dist {
                best_dist = d;
                best_param = base_param + arc_dist.max(0.0);
            }
        }
    }
    (best_param / total).min(1.0)
}

/// Whether a pixel is on a dash (light) or a gap (dark) in the marching
/// ants scheme.  Alternating segments travel around the outline at a
/// fixed speed; dash length is shorter than Photoshop-style ants.  The
/// dashes are equal in perimeter distance thanks to [`perimeter_param`].
fn marching_ants_on_dash(x: i32, y: i32, w: f64, h: f64, speed: f64) -> bool {
    let t = outline_elapsed() as f64 * speed;
    let r = MINIMAP_CORNER_RADIUS as f64;
    let param = perimeter_param(x as f64, y as f64, w, h, r);
    let straight_h = (w - 2.0 * r).max(0.0);
    let straight_v = (h - 2.0 * r).max(0.0);
    let perimeter = 2.0 * straight_h + 2.0 * straight_v + std::f64::consts::TAU * r;
    let target_dash_px = 20.0;
    let num_half = ((perimeter / target_dash_px).round() as i32).max(4) & !1;
    let half = 1.0 / num_half as f64;
    let phase = (param + t * 0.0175).rem_euclid(1.0);
    let slot = (phase / half).floor() as i32;
    slot % 2 == 0
}

fn fit_capture_thumb(capture: (f64, f64), max_w: f64, max_h: f64) -> (f64, f64) {
    let (cw, ch) = (capture.0.max(1.0), capture.1.max(1.0));
    let scale = (max_w / cw).min(max_h / ch).min(1.0);
    ((cw * scale).max(1.0), (ch * scale).max(1.0))
}

fn rounded_rect_sdf(x: f64, y: f64, w: f64, h: f64, r: f64) -> f64 {
    let qx = (x - w / 2.0).abs() - (w / 2.0 - r);
    let qy = (y - h / 2.0).abs() - (h / 2.0 - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// Return `(rounded-rect coverage, outline-stroke coverage)` for one pixel.
/// The stroke is measured **inside the boundary only** (`-outline_width ≤ d ≤ 0`)
/// so the visual thickness is exactly `outline_width` everywhere, including
/// corners.  8×8 supersampling gives smooth coverage gradients at the
/// inner edge of the stroke — the coverage value is used directly as the
/// outline's alpha for buttery-smooth anti-aliasing instead of a jagged
/// binary threshold.
fn minimap_pixel_coverages(x: i32, y: i32, w: f64, h: f64, outline_width: f64) -> (u8, u8) {
    let r = MINIMAP_CORNER_RADIUS as f64;
    let mut inside = 0i32;
    let mut stroke = 0i32;
    let samples = MINIMAP_MASK_SAMPLES * MINIMAP_MASK_SAMPLES;
    for sy in 0..MINIMAP_MASK_SAMPLES {
        for sx in 0..MINIMAP_MASK_SAMPLES {
            let px = x as f64 + (sx as f64 + 0.5) / MINIMAP_MASK_SAMPLES as f64;
            let py = y as f64 + (sy as f64 + 0.5) / MINIMAP_MASK_SAMPLES as f64;
            let d = rounded_rect_sdf(px, py, w, h, r);
            if d <= 0.0 {
                inside += 1;
                if d >= -outline_width {
                    stroke += 1;
                }
            }
        }
    }
    (
        ((inside * 255) / samples) as u8,
        ((stroke * 255) / samples) as u8,
    )
}

fn apply_minimap_mask(buf: &mut RgbaBuffer, outline_width: f64) {
    let (w, h) = (buf.width as f64, buf.height as f64);
    for y in 0..buf.height {
        for x in 0..buf.width {
            let (inside, stroke) = minimap_pixel_coverages(x, y, w, h, outline_width);
            // The outline and the clipped-away outside are transparent in the
            // content sprite. This exposes the magnified screen underneath
            // the outline and lets the rounded corners cut only black space.
            if inside < 255 || stroke > 0 {
                let i = (y as usize * buf.width as usize + x as usize) * 4;
                buf.data[i..i + 4].copy_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
}

fn build_minimap_outline(
    buf_w: i32,
    buf_h: i32,
    scheme: crate::config::MinimapOutlineScheme,
    speed: f64,
    outline_width: f64,
) -> RgbaBuffer {
    use crate::config::MinimapOutlineScheme;
    let mut out = RgbaBuffer::new(buf_w, buf_h);
    let (w, h) = (buf_w as f64, buf_h as f64);
    for y in 0..buf_h {
        for x in 0..buf_w {
            let (_, stroke) = minimap_pixel_coverages(x, y, w, h, outline_width);
            if stroke == 0 {
                continue;
            }
            let i = (y as usize * buf_w as usize + x as usize) * 4;
            // Use the supersampled coverage directly as the outline alpha
            // so corners anti-alias smoothly instead of snapping to a
            // jagged binary edge.
            match scheme {
                MinimapOutlineScheme::Gradient => {
                    let c = minimap_outline_color(speed);
                    out.data[i..i + 4]
                        .copy_from_slice(&[c[0], c[1], c[2], stroke]);
                }
                MinimapOutlineScheme::AngularGradient => {
                    let c =
                        minimap_angular_gradient_color(x, y, w, h, speed);
                    out.data[i..i + 4]
                        .copy_from_slice(&[c[0], c[1], c[2], stroke]);
                }
                MinimapOutlineScheme::MarchingAnts => {
                    // Alternate between light grey dashes and dark grey
                    // gaps — both are solid and opaque, covering the
                    // magnified screen underneath.
                    let on_dash = marching_ants_on_dash(x, y, w, h, speed);
                    let (lr, lg, lb) = (192u8, 192u8, 192u8); // light grey
                    let (dr, dg, db) = (48u8, 48u8, 48u8);     // dark grey
                    let (cr, cg, cb) = if on_dash {
                        (lr, lg, lb)
                    } else {
                        (dr, dg, db)
                    };
                    out.data[i..i + 4]
                        .copy_from_slice(&[cr, cg, cb, stroke]);
                }
            }
        }
    }
    out
}

fn blend_outline_into(canvas: &mut [u8], stride: i32, outline: &[u8], width: i32, height: i32, ox: i32, oy: i32) {
    for y in 0..height {
        for x in 0..width {
            let si = (y as usize * width as usize + x as usize) * 4;
            let alpha = outline[si + 3] as u16;
            if alpha == 0 { continue; }
            let di = (((y + oy) as usize) * stride as usize + (x + ox) as usize) * 4;
            let a = 255 - alpha;
            canvas[di] = ((outline[si] as u16 * alpha + canvas[di] as u16 * a) / 255) as u8;
            canvas[di + 1] = ((outline[si + 1] as u16 * alpha + canvas[di + 1] as u16 * a) / 255) as u8;
            canvas[di + 2] = ((outline[si + 2] as u16 * alpha + canvas[di + 2] as u16 * a) / 255) as u8;
        }
    }
}

fn minimap_marker_rect(
    center: (f64, f64),
    zoom: f64,
    viewport: (f64, f64),
) -> Option<(f64, f64, f64, f64)> {
    if zoom <= 0.0 {
        return None;
    }
    let vw = viewport.0 / zoom;
    let vh = viewport.1 / zoom;
    Some((
        center.0 - vw / 2.0,
        center.1 - vh / 2.0,
        center.0 + vw / 2.0,
        center.1 + vh / 2.0,
    ))
}

/// Box-average downscale of a capture into a `dst_w x dst_h` buffer, each
/// channel dimmed by `dim` (0.0..1.0) — the "toned down" minimap base.
fn downscale_dimmed(src: &RgbaBuffer, dst_w: i32, dst_h: i32, dim: f32) -> RgbaBuffer {
    let mut out = RgbaBuffer::new(dst_w, dst_h);
    let (sw, sh) = (src.width as f64, src.height as f64);
    let dim = dim as f64;
    for y in 0..dst_h {
        let sy0 = (y as f64 * sh / dst_h as f64) as usize;
        let sy1 = (((y + 1) as f64 * sh / dst_h as f64).ceil() as usize).min(src.height as usize);
        for x in 0..dst_w {
            let sx0 = (x as f64 * sw / dst_w as f64) as usize;
            let sx1 =
                (((x + 1) as f64 * sw / dst_w as f64).ceil() as usize).min(src.width as usize);
            let mut acc = [0u64; 3];
            let mut n = 0u64;
            for sy in sy0..sy1 {
                let row = sy * src.width as usize;
                for sx in sx0..sx1 {
                    let i = (row + sx) * 4;
                    acc[0] += src.data[i] as u64;
                    acc[1] += src.data[i + 1] as u64;
                    acc[2] += src.data[i + 2] as u64;
                    n += 1;
                }
            }
            let i = (y as usize * dst_w as usize + x as usize) * 4;
            out.data[i] = ((acc[0] / n.max(1)) as f64 * dim) as u8;
            out.data[i + 1] = ((acc[1] / n.max(1)) as f64 * dim) as u8;
            out.data[i + 2] = ((acc[2] / n.max(1)) as f64 * dim) as u8;
            out.data[i + 3] = 255;
        }
    }
    out
}

/// Fill the square region of `capture` centered on `(cx, cy)` (capture px,
/// half-size [`CURSOR_BAKE_HALF`]) with the average color of the 1 px ring
/// just outside it, returning the scrubbed copy. Used to remove the
/// launching app's own baked-in cursor graphic from the minimap overview
/// before downscaling — the ring-average fill blends into the surrounding
/// content, and the marker dot is drawn on top of it anyway.
fn inpaint_cursor_region(capture: &RgbaBuffer, cx: f64, cy: f64, half: i32) -> RgbaBuffer {
    let mut out = capture.clone();
    let (w, h) = (capture.width, capture.height);
    let (x0, y0) = (cx as i32 - half, cy as i32 - half);
    let (x1, y1) = (cx as i32 + half, cy as i32 + half);
    // Average the ring just outside the region (clamped to the frame).
    let mut acc = [0u64; 3];
    let mut n = 0u64;
    for y in (y0 - 1).max(0)..(y1 + 1).min(h) {
        for x in (x0 - 1).max(0)..(x1 + 1).min(w) {
            if x >= x0 && x < x1 && y >= y0 && y < y1 {
                continue;
            }
            let i = (y as usize * w as usize + x as usize) * 4;
            acc[0] += out.data[i] as u64;
            acc[1] += out.data[i + 1] as u64;
            acc[2] += out.data[i + 2] as u64;
            n += 1;
        }
    }
    if n == 0 {
        return out;
    }
    let col = [(acc[0] / n) as u8, (acc[1] / n) as u8, (acc[2] / n) as u8];
    for y in y0.max(0)..y1.min(h) {
        for x in x0.max(0)..x1.min(w) {
            let i = (y as usize * w as usize + x as usize) * 4;
            out.data[i] = col[0];
            out.data[i + 1] = col[1];
            out.data[i + 2] = col[2];
        }
    }
    out
}

/// Build the minimap sprite for one frame.
///
/// Returns `(sprite, new_base)`: the sprite to draw (`None` when the minimap
/// is hidden) and the (possibly rebuilt) cached base buffer the caller must
/// store back. The base — chrome + dimmed downscale, no marker — is reused
/// across frames; only the marker (view outline + cursor dot) is redrawn
/// each frame into a clone, keeping per-frame cost to a small buffer copy.
/// `scale` multiplies the sprite rect (RENDER_SCALE on the GPU path so the
/// rect spans the scaled surface while the buffer stays at logical
/// resolution; 1.0 on the CPU path); the base buffer itself is
/// scale-independent and shared between the paths.
#[allow(clippy::too_many_arguments)]
/// Build the minimap sprite (dimmed overview + animated outline + marker).
///
/// Since the frozen frame never changes, the expensive SDF-based outline
/// geometry and content mask are **cached** and only recomputed when the
/// minimap dimensions or outline thickness change. Each frame, only the
/// animated outline colors and the per-frame marker/cursor-dot are drawn.
#[allow(clippy::too_many_arguments)]
fn build_minimap_sprite(
    capture: &RgbaBuffer,
    view_center: (f64, f64),
    zoom: f64,
    viewport: (f64, f64),
    scale: f64,
    corner: crate::osd::Corner,
    bake_pos: Option<(f64, f64)>,
    base: Option<RgbaBuffer>,
    scheme: crate::config::MinimapOutlineScheme,
    speed: f64,
    outline_thickness: f64,
    outline_zoom_scale: f64,
    max_zoom: f64,
    outline_coverage_cache: &mut Option<(i32, i32, f64, Vec<u8>)>,
    masked_base_cache: &mut Option<(i32, i32, f64, RgbaBuffer)>,
) -> (Option<OsdSprite>, Option<RgbaBuffer>) {
    // Effective outline width: base thickness scaled by zoom level.
    let zoom_t = if max_zoom > 1.0 {
        ((zoom - 1.0).max(0.0) / (max_zoom - 1.0)).min(1.0)
    } else {
        0.0
    };
    let outline_width = outline_thickness * (1.0 + outline_zoom_scale * zoom_t);
    let (mm_x, mm_y, mm_w, mm_h) = minimap_layout(
        viewport,
        (capture.width as f64, capture.height as f64),
        14.0,
        corner,
    );
    let buf_w = mm_w.round() as i32;
    let buf_h = mm_h.round() as i32;
    let inset = outline_width.ceil() as i32;
    let content_w = (buf_w - inset * 2).max(1);
    let content_h = (buf_h - inset * 2).max(1);
    let (tw, th) = fit_capture_thumb(
        (capture.width as f64, capture.height as f64),
        content_w as f64,
        content_h as f64,
    );
    let tw_i = tw.round().clamp(1.0, content_w as f64) as i32;
    let th_i = th.round().clamp(1.0, content_h as f64) as i32;
    let ox_i = inset + (content_w - tw_i) / 2;
    let oy_i = inset + (content_h - th_i) / 2;

    // Rebuild the base (dimmed downscale) when missing or size changed.
    let base = match base {
        Some(b) if b.width == buf_w && b.height == buf_h => b,
        _ => {
            let mut b = RgbaBuffer::new(buf_w, buf_h);
            for px in b.data.chunks_exact_mut(4) {
                px.copy_from_slice(&[0, 0, 0, 255]);
            }
            let src = match bake_pos {
                Some((cx, cy)) => inpaint_cursor_region(capture, cx, cy, CURSOR_BAKE_HALF),
                None => capture.clone(),
            };
            let img = downscale_dimmed(&src, tw_i, th_i, 0.45);
            for y in 0..th_i {
                let s = y as usize * tw_i as usize * 4;
                let d = ((y + oy_i) as usize * buf_w as usize + ox_i as usize) * 4;
                b.data[d..d + tw_i as usize * 4]
                    .copy_from_slice(&img.data[s..s + tw_i as usize * 4]);
            }
            // Invalidate outline/mask caches when the base changes.
            *outline_coverage_cache = None;
            *masked_base_cache = None;
            b
        }
    };

    // --- Outline geometry cache (SDF + supersampling) ---
    // The outline coverage (per-pixel stroke alpha) depends only on the
    // minimap dimensions and outline width — both constant during panning.
    // Recompute only when the cache key changes.
    let coverage_valid = outline_coverage_cache
        .as_ref()
        .is_some_and(|(w, h, ow, _)| *w == buf_w && *h == buf_h && (*ow - outline_width).abs() < 1e-9);
    if !coverage_valid {
        let coverage = compute_outline_coverage(buf_w, buf_h, outline_width);
        *outline_coverage_cache = Some((buf_w, buf_h, outline_width, coverage));
    }
    let coverage = &outline_coverage_cache.as_ref().unwrap().3;

    // --- Pre-masked base cache ---
    // The mask clears pixels in the content where the outline sits. This
    // geometry is also constant — apply once and cache.
    let mask_valid = masked_base_cache
        .as_ref()
        .is_some_and(|(w, h, ow, _)| *w == buf_w && *h == buf_h && (*ow - outline_width).abs() < 1e-9);
    if !mask_valid {
        let mut masked = base.clone();
        apply_minimap_mask(&mut masked, outline_width);
        *masked_base_cache = Some((buf_w, buf_h, outline_width, masked));
    }
    let masked_base = &masked_base_cache.as_ref().unwrap().3;

    // --- Per-frame work (cheap) ---
    // Clone the pre-masked base, draw the marker + cursor dot, then
    // apply animated colors from the cached coverage.
    let mut frame = masked_base.clone();
    let (cw, ch) = (capture.width as f64, capture.height as f64);
    let to_mm_x = |px: f64, total: f64| ox_i as f64 + (px / total.max(1.0)) * tw_i as f64;
    let to_mm_y = |py: f64, total: f64| oy_i as f64 + (py / total.max(1.0)) * th_i as f64;

    // Marker: visible-region corner brackets.
    if let Some((rx0, ry0, rx1, ry1)) = minimap_marker_rect(view_center, zoom, viewport) {
        let x0 = to_mm_x(rx0.clamp(0.0, cw), cw).round() as i32;
        let x1 = to_mm_x(rx1.clamp(0.0, cw), cw).round() as i32;
        let y0 = to_mm_y(ry0.clamp(0.0, ch), ch).round() as i32;
        let y1 = to_mm_y(ry1.clamp(0.0, ch), ch).round() as i32;
        let (x0, x1) = (x0.min(x1 - 1), x1.max(x0 + 1));
        let (y0, y1) = (y0.min(y1 - 1), y1.max(y0 + 1));
        if (x1 - x0) >= MINIMAP_MARKER_MIN_EDGE && (y1 - y0) >= MINIMAP_MARKER_MIN_EDGE {
            let tick = MINIMAP_CORNER_TICK
                .min((x1 - x0) / 2)
                .min((y1 - y0) / 2)
                .max(1);
            let bracket = [255, 200, 70, 255];
            fill_px(&mut frame.data, buf_w, buf_h, x0, x0 + tick, y0, y0 + 1, bracket);
            fill_px(&mut frame.data, buf_w, buf_h, x0, x0 + 1, y0, y0 + tick, bracket);
            fill_px(&mut frame.data, buf_w, buf_h, x1 - tick, x1, y0, y0 + 1, bracket);
            fill_px(&mut frame.data, buf_w, buf_h, x1 - 1, x1, y0, y0 + tick, bracket);
            fill_px(&mut frame.data, buf_w, buf_h, x0, x0 + tick, y1 - 1, y1, bracket);
            fill_px(&mut frame.data, buf_w, buf_h, x0, x0 + 1, y1 - tick, y1, bracket);
            fill_px(&mut frame.data, buf_w, buf_h, x1 - tick, x1, y1 - 1, y1, bracket);
            fill_px(&mut frame.data, buf_w, buf_h, x1 - 1, x1, y1 - tick, y1, bracket);
        }
    }

    // Cursor marker: filled red circle with black outline.
    let dx = to_mm_x(view_center.0.clamp(0.0, cw), cw).round() as i32;
    let dy = to_mm_y(view_center.1.clamp(0.0, ch), ch).round() as i32;
    let (r_in, r_out) = (2.5f64, 3.5f64);
    let (red, black) = ([255, 60, 60, 255], [0, 0, 0, 255]);
    for py in (dy - r_out as i32)..=(dy + r_out as i32) {
        let dyr = (py as f64 - dy as f64).abs();
        let half = ((r_out * r_out - dyr * dyr).max(0.0)).sqrt();
        if half < 0.5 {
            continue;
        }
        let x_lo = (dx as f64 - half).floor() as i32;
        let x_hi = (dx as f64 + half).ceil() as i32;
        fill_px(&mut frame.data, buf_w, buf_h, x_lo, x_hi + 1, py, py + 1, black);
        if dyr <= r_in {
            let half_in = ((r_in * r_in - dyr * dyr).max(0.0)).sqrt();
            let xi_lo = (dx as f64 - half_in).ceil() as i32;
            let xi_hi = (dx as f64 + half_in).floor() as i32;
            fill_px(&mut frame.data, buf_w, buf_h, xi_lo, xi_hi + 1, py, py + 1, red);
        }
    }

    // Apply animated outline colors from cached coverage — no SDF needed.
    let outline = apply_outline_colors(coverage, buf_w, buf_h, scheme, speed);
    let sprite = OsdSprite {
        buffer: frame,
        outline: Some(outline),
        x: (mm_x * scale).round() as i32,
        y: (mm_y * scale).round() as i32,
        width: (mm_w * scale).round() as i32,
        height: (mm_h * scale).round() as i32,
    };
    (Some(sprite), Some(base))
}

/// Pre-compute the outline stroke coverage for every pixel. The result is
/// cached and reused across frames — only the animated colors change.
fn compute_outline_coverage(buf_w: i32, buf_h: i32, outline_width: f64) -> Vec<u8> {
    let mut coverage = Vec::with_capacity((buf_w * buf_h) as usize);
    let (w, h) = (buf_w as f64, buf_h as f64);
    for y in 0..buf_h {
        for x in 0..buf_w {
            let (_, stroke) = minimap_pixel_coverages(x, y, w, h, outline_width);
            coverage.push(stroke);
        }
    }
    coverage
}

/// Apply animated outline colors from pre-computed coverage. Per-pixel
/// color lookup only — no SDF, no supersampling.
fn apply_outline_colors(
    coverage: &[u8],
    buf_w: i32,
    buf_h: i32,
    scheme: crate::config::MinimapOutlineScheme,
    speed: f64,
) -> RgbaBuffer {
    use crate::config::MinimapOutlineScheme;
    let mut out = RgbaBuffer::new(buf_w, buf_h);
    for (i, &stroke) in coverage.iter().enumerate() {
        if stroke == 0 {
            continue;
        }
        let pi = i * 4;
        let x = (i as i32) % buf_w;
        let y = (i as i32) / buf_w;
        match scheme {
            MinimapOutlineScheme::Gradient => {
                let c = minimap_outline_color(speed);
                out.data[pi..pi + 4]
                    .copy_from_slice(&[c[0], c[1], c[2], stroke]);
            }
            MinimapOutlineScheme::AngularGradient => {
                let c = minimap_angular_gradient_color(x, y, buf_w as f64, buf_h as f64, speed);
                out.data[pi..pi + 4]
                    .copy_from_slice(&[c[0], c[1], c[2], stroke]);
            }
            MinimapOutlineScheme::MarchingAnts => {
                let on_dash = marching_ants_on_dash(x, y, buf_w as f64, buf_h as f64, speed);
                let (lr, lg, lb) = (192u8, 192u8, 192u8);
                let (dr, dg, db) = (48u8, 48u8, 48u8);
                let (cr, cg, cb) = if on_dash {
                    (lr, lg, lb)
                } else {
                    (dr, dg, db)
                };
                out.data[pi..pi + 4]
                    .copy_from_slice(&[cr, cg, cb, stroke]);
            }
        }
    }
    out
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

/// Alpha-blend an overlay buffer (tightly packed `width*4` rows) into an RGBA
/// canvas in place at offset `(ox, oy)` (the CPU fallback path; on the GPU
/// path overlays are uploaded as textures and the sprite shader blends
/// them). Used for the fullscreen screenshot overlay (offset 0) and the
/// corner minimap sprite.
fn blend_overlay_into(
    canvas: &mut [u8],
    stride: i32,
    overlay: &[u8],
    width: i32,
    height: i32,
    ox: i32,
    oy: i32,
) {
    for y in 0..height {
        for x in 0..width {
            let di = (((y + oy) as usize) * (stride as usize) + (x + ox) as usize) * 4;
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

/// Linux input event code for the left mouse button (draws the screenshot
/// selection rectangle in Screenshot Mode).
const BTN_LEFT: u32 = 0x110;
/// How long the "Saved …" screenshot heads-up stays in the OSD legend.
const SCREENSHOT_NOTICE_SECS: u64 = 4;
/// Linux input event code for the right mouse button.
const BTN_RIGHT: u32 = 0x111;
/// Linux input event code for the middle mouse button (the default
/// hold-to-zoom key; with a non-MMB hold-to-zoom binding it resets the zoom).
const BTN_MIDDLE: u32 = 0x112;
/// The binding-name string for the middle mouse button, matched against
/// `keybindings.hold_to_zoom`. When it is the configured hold-to-zoom key,
/// MMB press arms hold-to-zoom (and MMB release disarms it) instead of
/// resetting the zoom.
const MMB_HTZ: &str = "MMB";

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
    /// Deadline after which the launch hint ("K: help") disappears.
    pub launch_hint_deadline: Option<std::time::Instant>,
}

impl MagnifierState {
    pub fn new(config: MagnifierConfig, initial_zoom: Option<f64>) -> Self {
        let max_zoom = config.max_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let min_zoom = config.min_zoom();
        let zoom = initial_zoom
            .unwrap_or_else(|| config.default_zoom.unwrap_or(1.0))
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
            launch_hint_deadline: Some(
                std::time::Instant::now() + std::time::Duration::from_secs(4),
            ),
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
            .unwrap_or(1.0)
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
    /// The launch pointer position in capture px — where the launching app's
    /// own cursor graphic (XWayland / software cursors, which `overlay_cursor
    /// = 0` cannot exclude) is baked into the frozen frame. The minimap base
    /// scrubs this region so no stray miniature cursor appears next to its
    /// marker dot.
    cursor_bake_capture_pos: Option<(f64, f64)>,
    view_center: Option<(f64, f64)>,
    /// Wall-clock time of the last pointer-motion event, driving the offset
    /// correction's time constant (only real motion corrects — never
    /// self-animated).
    last_motion_at: Option<std::time::Instant>,
    /// A motion-driven redraw was throttled and is waiting for the next
    /// [`MOTION_REDRAW_INTERVAL`] deadline (see [`MagnifierWindow::request_motion_redraw`]).
    redraw_pending: bool,
    /// A settle redraw is pending: armed on pointer motion, fired once the
    /// pointer has been still for [`CURSOR_SETTLE_DELAY`], so the frozen
    /// screen's sampling origin snaps onto the cursor's lattice (see
    /// [`MagnifierWindow::draw_frame_if_settle_due`]). The cursor itself
    /// never moves.
    settle_pending: bool,
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
    /// The `zwp_pointer_constraints_v1` global (when the compositor provides
    /// it). Used to confine the pointer to the layer surface so it can never
    /// leave into other surfaces (shell hot corners, panels) — the compositor
    /// then always keeps the blank cursor in effect and the OS cursor never
    /// shows, and the delivered position at the screen edges is clamped
    /// instead of oscillating sub-pixel-wise.
    pointer_constraints: PointerConstraintsState,
    /// The active pointer confinement on the layer surface (persistent
    /// lifetime, whole-surface region), kept alive for the app's lifetime.
    confinement: Option<ZwpConfinedPointerV1>,
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
    /// Accumulated upward travel (logical px) while the zoom sits exactly on
    /// the runtime minimum (the fully-zoomed-out view): tiny alternating
    /// pointer jitter must not flap the zoom between the floor and one step
    /// above it, so zoom-in from the floor is ignored until this passes
    /// [`HTZ_FLOOR_DEADZONE`] (see [`htz_floor_zoom`]).
    hold_floor_dead_travel: f64,
    /// Non-magnetic edge-hold latch per axis (`(x, y)`): which side (high /
    /// low) of each axis is currently held onto its capture edge, or `None`.
    /// See [`edge_hold_axis`] — the latch gives the edge-hold hysteresis so
    /// a parked pointer's micro-wobble can't make the quantized view hop
    /// between the edge and `capture − 2`.
    edge_hold: (Option<bool>, Option<bool>),
    /// Whether the Shift key is currently held. Used to slow down pointer
    /// motion (panning) by the configured factor.
    shift_held: bool,
    /// Sub-pixel accumulator for smooth shift-slowed panning: tracks the
    /// fractional capture-pixel movement from scaled deltas, advancing
    /// the view center only when the accumulator crosses 0.5 px. This
    /// eliminates the quantization artifacts ("square" motion) that a
    /// simple per-event multiplier creates at high zoom.
    pan_accum: (f64, f64),
    /// Whether the minimap overlay (a dimmed overview of the frozen screen
    /// with the visible-region marker) is shown in the viewport corner
    /// (toggled with the `minimap` key, default `M`).
    minimap_visible: bool,
    /// Cached minimap base buffer (dimmed downscale of the capture + chrome),
    /// without the per-frame view marker. Rebuilt only when the capture or
    /// the minimap size changes; each frame the marker is drawn into a clone
    /// of this.
    minimap_base: Option<RgbaBuffer>,
    /// Cached minimap outline coverage (per-pixel stroke alpha from the
    /// SDF supersampling). Keyed on `(buf_w, buf_h, outline_width)`. Only
    /// recomputed when the minimap dimensions or outline thickness change.
    minimap_outline_coverage: Option<(i32, i32, f64, Vec<u8>)>,
    /// Cached minimap base with the outline mask already applied. Keyed
    /// on `(buf_w, buf_h, outline_width)`. Avoids re-running the SDF-
    /// based mask on every frame.
    minimap_masked_base: Option<(i32, i32, f64, RgbaBuffer)>,
    /// Last zoom level at which the cursor texture was uploaded to the GPU.
    /// When the zoom hasn't changed, theTexImage2D upload is skipped.
    cursor_upload_zoom: f64,
    /// Whether the cursor was visible on the previous frame. When
    /// visibility toggles, the texture is re-uploaded.
    cursor_was_visible: bool,
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
    // Direct index writes into pre-allocated capacity — avoids per-element
    // bounds checks from repeated `push` calls.
    let base = out.len();
    let n = width.min(row.len() / 4);
    out.resize(base + n * 4, 0);
    for (i, px) in row.chunks_exact(4).take(n).enumerate() {
        let o = base + i * 4;
        out[o] = px[2];
        out[o + 1] = px[1];
        out[o + 2] = px[0];
        out[o + 3] = 255;
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

impl PointerConstraintsHandler for MagnifierWindow {
    // The confinement is persistent, so nothing needs to be done on
    // activation or deactivation — the compositor re-engages it on every
    // pointer enter into the layer surface.
    fn confined(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpConfinedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }

    fn unconfined(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpConfinedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }

    fn locked(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wayland_protocols::wp::pointer_constraints::zv1::client::
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }

    fn unlocked(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wayland_protocols::wp::pointer_constraints::zv1::client::
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
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
                        let raw_dx = position.0 - self.pointer_position_f.0;
                        let raw_dy = position.1 - self.pointer_position_f.1;
                        self.pointer_position_f = position;
                        self.state.pointer_position = (position.0 as i32, position.1 as i32);
                        // Shift modifier slows down panning for precision.
                        let shift_factor = if self.shift_held {
                            self.state.config.shift_slow_factor
                        } else {
                            1.0
                        };
                        let dx = raw_dx * shift_factor;
                        let dy = raw_dy * shift_factor;
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
                            let speed = self.state.config.hold_to_zoom_speed;
                            let max_zoom = self.state.config.max_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
                            let min_zoom = self.runtime_min_zoom();
                            // The floor is held rock-solid against tiny
                            // alternating pointer jitter (no flap between the
                            // fully-zoomed-out view and one step above it).
                            let (new_zoom, floor_dead) = htz_floor_zoom(
                                self.state.zoom,
                                min_zoom,
                                max_zoom,
                                dy_zoom,
                                speed,
                                self.hold_floor_dead_travel,
                            );
                            self.hold_floor_dead_travel = floor_dead;
                            if (new_zoom - self.state.zoom).abs() > 1e-9 {
                                self.set_zoom(new_zoom, min_zoom);
                            }
                            if let Some((cx, cy)) = self.view_center {
                                // Pan-tuning applies to the horizontal pan
                                // during hold-to-zoom too, so the feel is
                                // consistent.
                                let tuning = self.state.config.pan_tuning.clamp(0.0, 1.0);
                                let gain = pan_tuning_gain(self.state.zoom, tuning);
                                if self.shift_held {
                                    let scaled = dx * sx * gain;
                                    self.pan_accum.0 += scaled;
                                    if self.pan_accum.0.abs() >= 0.5 {
                                        let step = self.pan_accum.0.round();
                                        self.pan_accum.0 -= step;
                                        let nx = self.clamp_to_capture((cx + step, cy)).0;
                                        self.view_center = Some((nx, cy));
                                    }
                                } else {
                                    let nx = self.clamp_to_capture((cx + dx * sx * gain, cy)).0;
                                    self.view_center = Some((nx, cy));
                                }
                            }
                        } else if let Some((cx, cy)) = self.view_center {
                            // The view pans with the hand's *movement*
                            // (relative deltas) and is hard-clamped to the
                            // capture: the magnified cursor sits at the
                            // viewport center, so pushing against a screen
                            // edge always lands the view *exactly* on the
                            // capture edge (never in the black beyond-capture
                            // fill), and every captured pixel stays reachable.
                            // Pan-tuning scales the pan distance per mouse
                            // pixel with the zoom (see `pan_tuning`); while it
                            // is active the view intentionally lags the hand's
                            // content, so the offset correction below is
                            // suspended — it would read the intentional lag as
                            // a residual and erase it on the next toward-motion.
                            let tuning = self.state.config.pan_tuning.clamp(0.0, 1.0);
                            let gain = pan_tuning_gain(self.state.zoom, tuning);
                            let (nx, ny) = if self.shift_held {
                                // Accumulate scaled deltas; advance the view
                                // center only when the accumulator crosses 0.5
                                // capture px. This eliminates the quantization
                                // artifacts ("square" motion) that a simple
                                // per-event multiplier creates at high zoom.
                                let (scaled_x, scaled_y) = (dx * sx * gain, dy * sy * gain);
                                self.pan_accum.0 += scaled_x;
                                self.pan_accum.1 += scaled_y;
                                let step_x = if self.pan_accum.0.abs() >= 0.5 {
                                    let s = self.pan_accum.0.round();
                                    self.pan_accum.0 -= s;
                                    s
                                } else {
                                    0.0
                                };
                                let step_y = if self.pan_accum.1.abs() >= 0.5 {
                                    let s = self.pan_accum.1.round();
                                    self.pan_accum.1 -= s;
                                    s
                                } else {
                                    0.0
                                };
                                self.clamp_to_capture((cx + step_x, cy + step_y))
                            } else {
                                self.clamp_to_capture((cx + dx * sx * gain, cy + dy * sy * gain))
                            };
                            let (fx, fy) = if tuning > 0.0 {
                                (nx, ny)
                            } else {
                                // The view-vs-hand offset (view minus hand
                                // content; hold-to-zoom locks the view y while
                                // the hand travels to zoom, and a launch quirk
                                // or resize can leave a residual). Left alone
                                // it shifts the reachable pan range and
                                // creates invisible limits, so it is corrected
                                // by real pointer motion only, every event:
                                // each motion pulls the view a small fraction
                                // of the remaining offset toward the hand
                                // content, so navigation is always fully
                                // restored without any jump or self-animation.
                                // The correction never fights a wall: an axis
                                // already pinned to a capture edge is left
                                // untouched, so the view always reaches — and
                                // glides along — the exact edges, regardless
                                // of pointer speed. In steady state the offset
                                // is zero (the view pans 1:1 with the hand),
                                // so this is dormant.
                                let hand = (
                                    self.pointer_position_f.0 * sx,
                                    self.pointer_position_f.1 * sy,
                                );
                                let offset = (nx - hand.0, ny - hand.1);
                                if offset.0.hypot(offset.1) > 0.5 {
                                    let (rox, roy) =
                                        offset_correction_step(offset, dt, (dx, dy), (sx, sy));
                                    correct_toward_hand(
                                        (nx, ny),
                                        (hand.0 + rox, hand.1 + roy),
                                        bounds,
                                    )
                                } else {
                                    (nx, ny)
                                }
                            };
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
                    if button == BTN_MIDDLE {
                        // The middle mouse button is the default hold-to-zoom
                        // key: while it is held, vertical motion zooms (see
                        // the Motion handler), so MMB press arms the feature
                        // instead of resetting the zoom. With any other
                        // hold-to-zoom binding configured, MMB keeps its
                        // legacy reset-to-default role (the `reset_zoom` key
                        // always resets either way).
                        if self.state.config.keybindings.hold_to_zoom == MMB_HTZ {
                            self.hold_to_zoom_active = true;
                            self.hold_zoom_last_y = self.pointer_position_f.1;
                            self.hold_floor_dead_travel = 0.0;
                            // Ensure the view center is initialized so the
                            // vertical lock engages immediately (in case no
                            // motion/draw happened before the press) —
                            // mirrors the keyboard arm in `press_key`.
                            if self.view_center.is_none() {
                                let (sx, sy) = self.capture_scale();
                                self.view_center = Some(self.clamp_to_capture((
                                    self.pointer_position_f.0 * sx,
                                    self.pointer_position_f.1 * sy,
                                )));
                            }
                        } else {
                            // Middle mouse button resets the zoom to the
                            // default; the view stays put (zoom scales around
                            // the center). The runtime minimum applies, so a
                            // default of 0 % lands on the fully-zoomed-out
                            // view.
                            let default_zoom = self.state.config.default_zoom.unwrap_or(1.0);
                            self.set_zoom(default_zoom, self.runtime_min_zoom());
                            self.draw_frame(qh);
                        }
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    // Releasing the hold-to-zoom key stops smooth zooming —
                    // the mouse-button counterpart of `release_key`. The view
                    // stays exactly where it is (no jump, no self-animation);
                    // the Motion handler corrects any residual offset with
                    // real pointer motion.
                    if button == BTN_MIDDLE && self.state.config.keybindings.hold_to_zoom == MMB_HTZ
                    {
                        let was_active = self.hold_to_zoom_active;
                        self.hold_to_zoom_active = false;
                        if was_active {
                            // Fresh baseline so the first correction step
                            // after the release never dumps the whole offset
                            // (a pause before the next motion would otherwise
                            // make dt huge).
                            self.last_motion_at = Some(std::time::Instant::now());
                            self.draw_frame(qh);
                        }
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
            } else if keysym_str == self.state.config.keybindings.minimap {
                // The minimap stays toggleable in Screenshot Mode too.
                self.minimap_visible = !self.minimap_visible;
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
            self.hold_floor_dead_travel = 0.0;
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
        } else if keysym_str == config_key.minimap {
            self.minimap_visible = !self.minimap_visible;
            self.draw_frame(qh);
            tracing::info!("Minimap visible: {}", self.minimap_visible);
        } else if keysym_str == config_key.reset_zoom {
            let default_zoom = self.state.config.default_zoom.unwrap_or(1.0);
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
        if self.shift_held && !modifiers.shift {
            self.pan_accum = (0.0, 0.0);
        }
        self.shift_held = modifiers.shift;
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
            // Confine the pointer to the layer surface (best effort — the
            // compositor may not provide pointer constraints).
            self.ensure_confinement(qh);
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
            if let Some(confinement) = self.confinement.take() {
                confinement.destroy();
            }
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
    /// Confine the pointer to the layer surface via `zwp_pointer_constraints_v1`
    /// (persistent lifetime, NULL region = the whole surface input region), so
    /// it can never leave the magnifier into other surfaces. The compositor
    /// then always keeps the blank cursor in effect (the OS cursor never shows
    /// at hot corners or edges) and clamps the delivered position at the
    /// screen edges instead of letting it oscillate sub-pixel-wise. No-op when
    /// the compositor does not provide the protocol.
    fn ensure_confinement(&mut self, qh: &QueueHandle<Self>) {
        if self.confinement.is_some() {
            return;
        }
        let Some(pointer) = &self.pointer else {
            return;
        };
        let surface = self.layer.wl_surface().clone();
        // NULL region = the whole surface input region (the fullscreen
        // overlay). Persistent lifetime re-engages the confinement on every
        // pointer re-enter. Best effort: compositors without the protocol
        // (or without a pointer) simply leave the pointer unconfined.
        let Ok(confinement) = self.pointer_constraints.confine_pointer(
            &surface,
            pointer,
            None,
            Lifetime::Persistent,
            qh,
        ) else {
            return;
        };
        tracing::info!("Pointer confined to the layer surface");
        self.confinement = Some(confinement);
    }

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

    /// After a timed wait, redraw if the pointer has been still long enough
    /// for the settle (armed by motion, fired once [`CURSOR_SETTLE_DELAY`]
    /// of stillness has passed). A single redraw applies the
    /// cursor-lattice-aligned sampling origin; further motion re-arms it.
    fn draw_frame_if_settle_due(&mut self, qh: &QueueHandle<Self>) {
        if !self.settle_pending {
            return;
        }
        let due = match self.last_motion_at {
            None => true,
            Some(last) => last.elapsed() >= CURSOR_SETTLE_DELAY,
        };
        if due {
            self.settle_pending = false;
            self.draw_frame(qh);
        }
    }

    /// The poll bound for the event loop: the earliest of the nudge-repeat
    /// deadline and the pending motion-redraw deadline. Returns `None` when
    /// neither is pending, so the loop blocks indefinitely (idle behaviour
    /// is unchanged).
    fn poll_timeout(&self) -> Option<std::time::Duration> {
        let mut best = self.repeat_poll_timeout();
        // The outline pulse must continue while the minimap is visible even
        // when the pointer is idle. A modest cadence keeps animation smooth
        // without busy-spinning the event loop.
        if self.minimap_visible {
            best = Some(best.map_or(MINIMAP_PULSE_INTERVAL, |b| b.min(MINIMAP_PULSE_INTERVAL)));
        }
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
        if self.settle_pending {
            let wait = match self.last_motion_at {
                None => CURSOR_SETTLE_DELAY,
                Some(last) => CURSOR_SETTLE_DELAY
                    .saturating_sub(std::time::Instant::now().duration_since(last)),
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

    /// Center the view on the launch pointer's content once, using the real
    /// capture scale. Requires both the launch position (first enter) and the
    /// capture; the enter can arrive before the screencopy completes.
    fn apply_launch_centering(&mut self) {
        if self.launch_centered || self.captured.is_none() {
            return;
        }
        if let Some(position) = self.launch_position.take() {
            let (sx, sy) = self.capture_scale();
            let center = self.clamp_to_capture((position.0 * sx, position.1 * sy));
            self.view_center = Some(center);
            // The baked cursor graphic sits at the same spot the view centers
            // on; scrub it out of the (possibly already built) minimap base.
            self.cursor_bake_capture_pos = Some(center);
            self.minimap_base = None;
            self.launch_centered = true;
        }
    }

    fn osd_lines(&self) -> Vec<String> {
        // Screenshot Mode shows its own instruction legend.
        if self.screenshot_active {
            let config_key = &self.state.config.keybindings;
            return vec![
                format!("maggie v{}  screenshot mode", env!("CARGO_PKG_VERSION")),
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
                format!("{}  toggle minimap", config_key.minimap),
            ];
        }
        let config_key = &self.state.config.keybindings;
        let mut lines = vec![
            // At the fully-zoomed-out view the readout shows "0 %" (see
            // [`zoom_readout`]); otherwise the zoom factor.
            format!(
                "maggie v{}  zoom {}",
                env!("CARGO_PKG_VERSION"),
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
            format!("{}  toggle minimap", config_key.minimap),
            format!("hold {} + move  smooth zoom", config_key.hold_to_zoom),
            // When MMB is the hold-to-zoom key it no longer resets the zoom
            // (the `reset_zoom` key does); with any other hold-to-zoom binding
            // MMB keeps the legacy reset role.
            if config_key.hold_to_zoom == MMB_HTZ {
                format!("{}  reset zoom", config_key.reset_zoom)
            } else {
                format!("MMB / {}  reset zoom", config_key.reset_zoom)
            },
            "Q / Esc / RMB  quit".to_string(),
        ];
        // The magnified cursor always sits at the viewport center, and the
        // view center in capture px is exactly what is under it. The frozen
        // frame is the output at its native resolution, so capture px are the
        // real physical screen pixels under the cursor (no scaling or
        // logical-coordinate fudging). Only shown once the first capture
        // (and its launch centering) has settled the view center — never a
        // placeholder.
        if let Some((cx, cy)) = self.view_center {
            // When the edge-hold is latched on an axis (meaning the user has
            // pushed to the very edge), display the capture dimension — the
            // user expects to see the full width/height at the edge.
            let display_x = match self.edge_hold.0 {
                Some(true) => self.captured.as_ref().map_or(cx, |c| c.buffer.width as f64),
                _ => cx,
            };
            let display_y = match self.edge_hold.1 {
                Some(true) => self
                    .captured
                    .as_ref()
                    .map_or(cy, |c| c.buffer.height as f64),
                _ => cy,
            };
            lines.insert(
                1,
                format!(
                    "pos {}x{}",
                    display_x.round() as i64,
                    display_y.round() as i64
                ),
            );
        }
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
        // Pixel-grid lock: quantize the view center to the nearest integer
        // capture pixel and keep it there forever (see
        // [`quantize_center_to_pixel_grid`]). The magnified cursor sits at
        // the dead center on a fixed lattice; rounding the center — the
        // capture pixel under the cursor — to an exact integer makes the
        // cursor's texels and the screen's texels coincide permanently: the
        // launch snap the user asked for, applied once and then held for
        // every subsequent pan and zoom. Nothing moves on its own; the
        // cursor never leaves the center. Configurable: with
        // `pixel_locked_panning` off the center stays continuous and the
        // render paths fall back to their per-origin crispness snap (the
        // smooth-panning trade-off — see the GPU/CPU branches).
        let (center_x, center_y) = if self.state.config.pixel_locked_panning {
            quantize_center_to_pixel_grid((center_x, center_y), (source_w as f64, source_h as f64))
        } else {
            (center_x, center_y)
        };
        // Edge-hold with hysteresis (see [`edge_hold_axis`]): pin each axis
        // to the capture edge (0 on the left/top, `capture` on the
        // right/bottom) while the pointer is parked at that edge, latched so
        // a parked pointer's micro-wobble can't make the quantized view hop
        // between the edge and capture − 2 (the bottom/right shiver).
        // Pinning to the capture edge (not `capture − 1`) makes the
        // beyond-capture boundary render exactly at the viewport center —
        // flush on the magnified cursor's apex — so the screen edge and the
        // cursor tip align perfectly at every wall (the earlier `capture − 1`
        // target left the boundary a full magnified texel past the apex,
        // which the user saw as the edge and cursor not lining up).
        let (px, py) = self.pointer_position_f;
        let (center_x, held_x) = edge_hold_axis(
            center_x,
            px,
            self.width as f64,
            source_w as f64,
            self.edge_hold.0,
        );
        let (center_y, held_y) = edge_hold_axis(
            center_y,
            py,
            self.height as f64,
            source_h as f64,
            self.edge_hold.1,
        );
        self.edge_hold = (held_x, held_y);
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

        // OSD placement: configured corner (default top-left), or always
        // top-left in Screenshot Mode so it never overlaps the selection.
        let osd_corner = if self.screenshot_active {
            crate::osd::Corner::TopLeft
        } else {
            self.state.config.osd_corner
        };
        // Minimap placement: configured corner (default bottom-right). If it
        // shares a corner with the OSD, push it to the opposite corner so
        // the two never overlap.
        let minimap_corner = {
            let desired = self.state.config.minimap_corner;
            if desired == osd_corner {
                match desired {
                    crate::osd::Corner::TopLeft => crate::osd::Corner::BottomRight,
                    crate::osd::Corner::TopRight => crate::osd::Corner::BottomLeft,
                    crate::osd::Corner::BottomLeft => crate::osd::Corner::TopRight,
                    crate::osd::Corner::BottomRight => crate::osd::Corner::TopLeft,
                }
            } else {
                desired
            }
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
                    osd_corner,
                    self.width as i32 * crate::gpu::RENDER_SCALE,
                    self.height as i32 * crate::gpu::RENDER_SCALE,
                )
            } else {
                None
            };
            // Shift slow-down indicator or launch hint — both are small,
            // transient sprites drawn at the OSD corner.
            let hint = if self.shift_held && self.state.config.show_shift_osd {
                crate::osd::build_hint_sprite(
                    &["Shift: slow".to_string()],
                    osd_corner,
                    self.width as i32 * crate::gpu::RENDER_SCALE,
                    self.height as i32 * crate::gpu::RENDER_SCALE,
                    [0xCC, 0xCC, 0xCC],
                )
            } else if !self.state.osd_visible
                && self
                    .state
                    .launch_hint_deadline
                    .is_some_and(|d| std::time::Instant::now() < d)
            {
                let key = &self.state.config.keybindings.toggle_osd;
                crate::osd::build_hint_sprite(
                    &[format!("{key}: help")],
                    osd_corner,
                    self.width as i32 * crate::gpu::RENDER_SCALE,
                    self.height as i32 * crate::gpu::RENDER_SCALE,
                    [0x80, 0x80, 0x80],
                )
            } else {
                // Clear the deadline once expired so we stop checking.
                self.state.launch_hint_deadline = None;
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
            // The GPU buffer is RENDER_SCALE x the logical size, so the
            // cursor sprite origin is scaled to match. The cursor never
            // moves on its own: its origin is always the dead-center hotspot
            // position (viewport center minus the scaled hotspot offset),
            // rounded to the surface pixel grid so the sprite's own texel
            // edges are crisp. In Screenshot Mode the cursor tracks the
            // live pointer (the aim position) instead.
            let cursor = cursor_logical.map(|(cx, cy)| {
                let (buf, (hx, hy)) = self
                    .magnified_cursor
                    .as_mut()
                    .expect("magnified cursor present")
                    .sprite(crate::gpu::RENDER_SCALE as f64);
                let origin_x = (cx * crate::gpu::RENDER_SCALE as f64 - hx).round() as i32;
                let origin_y = (cy * crate::gpu::RENDER_SCALE as f64 - hy).round() as i32;
                ((origin_x, origin_y), buf, (hx, hy))
            });
            // The minimap overlay (dimmed overview + view marker), rebuilt
            // per frame from the cached base while visible; the sprite rect
            // is in RENDER_SCALE surface coordinates, the buffer stays at
            // logical resolution (LINEAR-upscaled by the GPU).
            let minimap = if self.minimap_visible {
                let (sprite, base) = build_minimap_sprite(
                    &captured.buffer,
                    (center_x, center_y),
                    zoom,
                    (self.width as f64, self.height as f64),
                    crate::gpu::RENDER_SCALE as f64,
                    minimap_corner,
                    self.cursor_bake_capture_pos,
                    self.minimap_base.take(),
                    self.state.config.minimap_outline_scheme,
                    self.state.config.minimap_outline_speed,
                    self.state.config.minimap_outline_thickness as f64,
                    self.state.config.minimap_outline_zoom_scale,
                    self.state.config.max_zoom,
                    &mut self.minimap_outline_coverage,
                    &mut self.minimap_masked_base,
                );
                self.minimap_base = base;
                sprite
            } else {
                None
            };
            if zoom > 0.0 {
                // Pixel-locked panning: the view center is quantized to
                // integer capture px (see `draw_frame`), so the sampling
                // origin already keeps every texel boundary on an exact
                // pixel boundary — capture pixel `C == center` starts
                // exactly at the viewport center, where the cursor hotspot
                // sits, and the cursor's and the screen's blocks share one
                // lattice. No separate origin snap is needed (it would
                // shift the phase and break the lock). Smooth panning:
                // keep each texel individually crisp on the buffer's pixel
                // grid instead (`snap_render_origin`), accepting that the
                // phase drifts with the continuous pan.
                let (src_x, src_y) = if self.state.config.pixel_locked_panning {
                    (src_x, src_y)
                } else {
                    let factor = zoom * crate::gpu::RENDER_SCALE as f64;
                    (
                        snap_render_origin(src_x, factor),
                        snap_render_origin(src_y, factor),
                    )
                };
                let uv = (
                    src_x / source_w as f64,
                    src_y / source_h as f64,
                    view_w.min(source_w as f64) / source_w as f64,
                    view_h.min(source_h as f64) / source_h as f64,
                );
                let cursor_changed = self.state.cursor_visible
                    && ((self.cursor_upload_zoom - zoom).abs() > 1e-9
                        || self.state.cursor_visible != self.cursor_was_visible);
                gpu.draw(
                    Some(uv),
                    osd.as_ref(),
                    hint.as_ref(),
                    cursor.as_ref(),
                    overlay,
                    rebuild,
                    cursor_changed,
                    minimap.as_ref(),
                );
            } else {
                // 0 % zoom: the magnified view collapses to nothing — draw a
                // plain black view (src = None clears the buffer) while the
                // magnified cursor and OSD legend stay visible so the user
                // can still navigate back in.
                let cursor_changed = self.state.cursor_visible
                    && ((self.cursor_upload_zoom - zoom).abs() > 1e-9
                        || self.state.cursor_visible != self.cursor_was_visible);
                gpu.draw(
                    None,
                    osd.as_ref(),
                    hint.as_ref(),
                    cursor.as_ref(),
                    overlay,
                    rebuild,
                    cursor_changed,
                    minimap.as_ref(),
                );
            }
            // Track cursor state for next frame's upload skip.
            // Only update the zoom tracker when the cursor was actually
            // present this frame — otherwise the first frame (before any
            // pointer event) would mark the zoom as "uploaded" and skip
            // the texture upload on the second frame when the cursor
            // first appears.
            if cursor.is_some() && self.state.cursor_visible {
                self.cursor_upload_zoom = zoom;
            }
            self.cursor_was_visible = self.state.cursor_visible;
            if self.animating {
                self.request_frame_callback(qh);
            }
            return;
        }

        // CPU fallback: same texel-grid lock as the GPU branch, but the
        // canvas is at logical resolution, so one capture texel spans `zoom`
        // canvas px. The cursor sprite is built first so its origin can serve
        // as the settle anchor for the sampling origin below (mirroring the
        // GPU settle: while the pointer rests, the origin snaps onto the
        // cursor's lattice so the screen's blocks and the cursor's blocks
        // coincide with the cursor at the dead center).
        let cursor_buf = cursor_logical.map(|(cx, cy)| {
            let (buf, (hx, hy)) = self
                .magnified_cursor
                .as_mut()
                .expect("magnified cursor present")
                .sprite(1.0);
            // Dead center origin (or the live pointer in Screenshot Mode), on
            // the canvas pixel grid. The cursor never moves on its own.
            let origin_x = (cx - hx).round() as i32;
            let origin_y = (cy - hy).round() as i32;
            (
                (origin_x, origin_y),
                buf,
                (hx.round() as i32, hy.round() as i32),
            )
        });
        // Same pixel-grid behavior as the GPU branch: with pixel-locked
        // panning the quantized center keeps the texel grid on the cursor's
        // lattice at logical resolution; with smooth panning the origin is
        // snapped so each texel stays crisp on the canvas pixel grid.
        let (src_x, src_y) = if self.state.config.pixel_locked_panning {
            (src_x, src_y)
        } else {
            (
                snap_render_origin(src_x, zoom),
                snap_render_origin(src_y, zoom),
            )
        };
        let scaled =
            self.state
                .renderer
                .render_bilinear(&captured.buffer, (src_x, src_y), dest_w, dest_h);

        // Same forced-on behavior as the GPU path: a fresh screenshot notice
        // always shows the legend briefly.
        let show_osd = self.state.osd_visible || notice_fresh;
        let osd_lines = self.osd_lines();
        // Launch hint (CPU path).
        let hint_lines: Vec<String> = if self.shift_held && self.state.config.show_shift_osd {
            vec!["Shift: slow".to_string()]
        } else if !self.state.osd_visible
            && self
                .state
                .launch_hint_deadline
                .is_some_and(|d| std::time::Instant::now() < d)
        {
            self.state.launch_hint_deadline = None;
            vec![format!(
                "{}: help",
                self.state.config.keybindings.toggle_osd
            )]
        } else {
            self.state.launch_hint_deadline = None;
            vec![]
        };
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
        // The minimap for the CPU path: the same base-cached sprite, at
        // logical resolution, blended into the canvas at its corner position
        // inside the render closure (which cannot borrow `self`).
        let minimap_cpu: Option<(RgbaBuffer, i32, i32, Option<RgbaBuffer>)> = if self.minimap_visible {
            let (sprite, base) = build_minimap_sprite(
                &captured.buffer,
                (center_x, center_y),
                zoom,
                (self.width as f64, self.height as f64),
                1.0,
                minimap_corner,
                self.cursor_bake_capture_pos,
                self.minimap_base.take(),
                self.state.config.minimap_outline_scheme,
                self.state.config.minimap_outline_speed,
                self.state.config.minimap_outline_thickness as f64,
                self.state.config.minimap_outline_zoom_scale,
                self.state.config.max_zoom,
                &mut self.minimap_outline_coverage,
                &mut self.minimap_masked_base,
            );
            self.minimap_base = base;
            sprite.map(|s| (s.buffer, s.x, s.y, s.outline))
        } else {
            None
        };

        let shift_held = self.shift_held;
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
                blend_overlay_into(canvas, stride, overlay, ow, oh, 0, 0);
            }
            if let Some((cursor_pos, ref cursor_sprite, hotspot)) = cursor_buf {
                Self::draw_cursor_at(canvas, stride, cursor_pos, cursor_sprite, hotspot);
            }
            if let Some((ref mm, mm_x, mm_y, ref outline)) = minimap_cpu {
                blend_overlay_into(canvas, stride, &mm.data, mm.width, mm.height, mm_x, mm_y);
                if let Some(outline) = outline {
                    blend_outline_into(canvas, stride, &outline.data, outline.width, outline.height, mm_x, mm_y);
                }
            }
            if show_osd {
                crate::osd::draw_osd(canvas, width, height, &osd_lines, osd_corner);
            }
            if !hint_lines.is_empty() {
                let hint_color = if shift_held {
                    [0xCC, 0xCC, 0xCC]
                } else {
                    [0x80, 0x80, 0x80]
                };
                crate::osd::draw_osd_colored(
                    canvas,
                    width,
                    height,
                    &hint_lines,
                    osd_corner,
                    hint_color,
                );
            }
        });
    }

    /// Blit a magnified-cursor sprite at its sprite origin `pos` (top-left
    /// corner). The engine computes the origin so the cursor's hotspot pixel
    /// starts at the same canvas-pixel boundary as the screen texel at the
    /// viewport center — the two grids share one lattice, and the hotspot
    /// lands where the cursor tip should be. `stride` is the byte stride of a
    /// canvas row (width * 4); the usable pixel width of each row is
    /// stride / 4.
    fn draw_cursor_at(
        canvas: &mut [u8],
        stride: i32,
        pos: (i32, i32),
        cursor: &RgbaBuffer,
        _hotspot: (i32, i32),
    ) {
        let (cursor_w, cursor_h) = (cursor.width, cursor.height);
        let (pos_x, pos_y) = pos;
        let canvas_w = stride / 4;
        let canvas_h = canvas.len() as i32 / stride;

        for y in 0..cursor_h {
            let dest_y = pos_y + y;
            if dest_y < 0 || dest_y >= canvas_h {
                continue;
            }
            for x in 0..cursor_w {
                let dest_x = pos_x + x;
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
            gpu.draw(None, None, None, None, None, false, true, None);
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

    // Pointer constraints (zwp_pointer_constraints_v1): confine the pointer
    // to the layer surface so it can never leave into other surfaces (shell
    // hot corners etc.) and the blank cursor always stays in effect. Not all
    // compositors provide it — without it the app behaves exactly as before.
    let pointer_constraints = PointerConstraintsState::bind(&globals, &qh);

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
    let minimap_on_launch = config.minimap_visible;
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
        pointer_constraints,
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
        cursor_bake_capture_pos: None,
        view_center: None,
        last_motion_at: None,
        redraw_pending: false,
        settle_pending: false,
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
        confinement: None,
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
        hold_floor_dead_travel: 0.0,

        edge_hold: (None, None),
        shift_held: false,
        pan_accum: (0.0, 0.0),
        minimap_visible: minimap_on_launch,
        minimap_base: None,
        minimap_outline_coverage: None,
        minimap_masked_base: None,
        cursor_upload_zoom: -1.0,
        cursor_was_visible: false,
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
        // And a cursor settle may have come due while the pointer rested:
        // fire it so the cursor snaps into grid alignment without needing a
        // further event.
        window.draw_frame_if_settle_due(&qh);
        if window.minimap_visible {
            window.draw_frame(&qh);
        }
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
    fn pan_tuning_gain_scales_with_zoom() {
        // Disabled (0) or a degenerate zoom -> no effect.
        assert_eq!(pan_tuning_gain(12.0, 0.0), 1.0);
        assert_eq!(pan_tuning_gain(0.0, 0.5), 1.0);
        // Neutral at 1x; slower at high zoom (more mouse travel per texel);
        // faster below 1x (the "vice versa").
        assert!((pan_tuning_gain(1.0, 0.5) - 1.0).abs() < 1e-9);
        assert!((pan_tuning_gain(4.0, 0.5) - 0.5).abs() < 1e-9);
        assert!((pan_tuning_gain(12.0, 0.5) - 12.0_f64.powf(-0.5)).abs() < 1e-9);
        assert!(pan_tuning_gain(0.25, 0.5) > 1.0);
        // Stronger tuning -> even more mouse travel at high zoom.
        assert!(pan_tuning_gain(8.0, 1.0) < pan_tuning_gain(8.0, 0.5));
    }

    #[test]
    fn edge_hold_pins_the_edge_with_hysteresis_against_wobble() {
        let surf = 1829.0;
        let cap = 3200.0;
        // Not near the edge, pointer parked there: no grab-from-a-distance.
        assert_eq!(
            edge_hold_axis(3190.0, 1828.9, surf, cap, None),
            (3190.0, None)
        );
        // Engage at the high edge when the view is within EPS: pin to cap
        // (the capture edge), and the hold latches (Some(true)).
        let (v, held) = edge_hold_axis(3199.0, 1828.9, surf, cap, None);
        assert_eq!((v, held), (3200.0, Some(true)));
        // While latched, even a view that would quantize to cap-2 (a parked
        // pointer's micro-wobble panning the view) stays pinned to cap —
        // no 3198/3196 flip-flop.
        let (v, held) = edge_hold_axis(3197.0, 1828.4, surf, cap, Some(true));
        assert_eq!((v, held), (3200.0, Some(true)));
        assert_eq!(
            edge_hold_axis(3197.0, 1828.4, surf, cap, Some(true)),
            (3200.0, Some(true))
        );
        // Releasing: the pointer moves > margin away from the edge -> the
        // hold drops and the view pans freely.
        assert_eq!(
            edge_hold_axis(3199.0, 1820.0, surf, cap, Some(true)),
            (3199.0, None)
        );
        // Low edge engages to 0 and latches; pointer leaving low edge releases.
        assert_eq!(
            edge_hold_axis(1.0, 0.7, surf, cap, None),
            (0.0, Some(false))
        );
        assert_eq!(
            edge_hold_axis(2.5, 0.7, surf, cap, Some(false)),
            (0.0, Some(false))
        );
        assert_eq!(
            edge_hold_axis(1.0, 6.0, surf, cap, Some(false)),
            (1.0, None)
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
    fn htz_floor_zoom_holds_the_floor_against_jitter() {
        // The failure the user reported: at the fully-zoomed-out view (0x,
        // zoom == fit), continuing to drag made the zoom flap between the
        // floor and one step above it (0.57x <-> 0.59x) in a period-2 loop.
        // A 1 px down / 1 px up jitter must never leave the floor.
        let min = 0.5715;
        let max = 12.0;
        let speed = 0.02;
        let mut zoom = min;
        let mut dead = 0.0;
        for i in 0..200 {
            let dy = if i % 2 == 0 { 1.0 } else { -1.0 };
            (zoom, dead) = htz_floor_zoom(zoom, min, max, dy, speed, dead);
            assert_eq!(zoom, min, "jitter step {i} must hold the floor");
        }
        // A committed zoom-in leaves the floor: the first px is swallowed by
        // the dead zone, the second crosses it, and motion off the floor is
        // normal from there on.
        let (z, dead) = htz_floor_zoom(min, min, max, -1.0, speed, 0.0);
        assert_eq!(z, min);
        assert!(dead > 0.0, "dead zone accumulates");
        let (z, dead) = htz_floor_zoom(min, min, max, -1.0, speed, dead);
        assert!(z > min, "crossing the dead zone zooms in");
        assert_eq!(dead, 0.0);
        let (z, _) = htz_floor_zoom(z, min, max, -1.0, speed, 0.0);
        assert!(z > min + 0.01, "off the floor, zoom-in is normal");
        // Zooming out at the floor holds it exactly and re-arms the zone.
        let (z, dead) = htz_floor_zoom(min, min, max, 1.0, speed, 1.5);
        assert_eq!(z, min);
        assert_eq!(dead, 0.0);
    }

    #[test]
    fn htz_floor_zoom_below_floor_keeps_existing_behavior() {
        // The `0` key with 0 % not allowed leaves the zoom below the floor
        // (fit < 1): zooming out stays put, zooming in returns to the floor.
        let (z, _) = htz_floor_zoom(0.5715, 1.0, 12.0, 2.0, 0.02, 0.0);
        assert_eq!(z, 0.5715);
        let (z, _) = htz_floor_zoom(0.5715, 1.0, 12.0, -2.0, 0.02, 0.0);
        assert_eq!(z, 1.0);
        // Mid-range: plain clamp.
        let (z, _) = htz_floor_zoom(5.0, 1.0, 12.0, -2.0, 0.02, 0.0);
        assert_eq!(z, 5.04);
        let (z, _) = htz_floor_zoom(5.0, 1.0, 12.0, 2.0, 0.02, 0.0);
        assert_eq!(z, 4.96);
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
    fn away_motion_never_reverses_the_pan_direction() {
        // The exact failure the user reported: after a deep hold-to-zoom
        // zoom-in the view sits far *below* the hand content (huge positive
        // offset, hand way up). Dragging away from the content used to let
        // the time-based heal (capped at 2× the hand's travel) overpower the
        // plain pan, so mouse-down panned the view *up* — the same direction
        // as mouse-up. The away-motion correction must be bounded by the
        // hand's own travel: the view moves with the drag or stands still,
        // never backwards.
        let o = 1050.0; // view below the hand content by 1050 capture px
        let scale = (1.75, 1.75);
        // Dragging down (away, t > 0): corrected at most the hand's travel.
        let after = offset_correction_step((o, 0.0), 0.016, (10.0, 0.0), scale);
        let corrected = o - after.0;
        assert!(
            corrected <= 10.0 * scale.0 + 1e-9,
            "away correction {corrected} exceeds the hand's travel"
        );
        // Mirrored: dragging up (away) with the view above the hand content
        // must not move the view down.
        let after = offset_correction_step((-o, 0.0), 0.016, (-10.0, 0.0), scale);
        let corrected = (-o) - after.0;
        assert!(
            corrected >= -10.0 * scale.0 - 1e-9,
            "away correction {corrected} exceeds the hand's travel"
        );
        // The residual is erased by toward-motion (the catch-up boost);
        // continuous toward-motion converges to zero.
        let mut o2 = (o, 0.0);
        for _ in 0..200_000 {
            o2 = offset_correction_step(o2, 0.016, (-10.0, 0.0), scale);
            if o2.0.hypot(o2.1) < 0.5 {
                break;
            }
        }
        assert!(o2.0.hypot(o2.1) < 0.5, "offset {o2:?}");
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
        // Continuous motion still converges — away-motion never heals, so
        // use toward-motion travel (the catch-up boost erases the residual).
        let mut o2 = o;
        for _ in 0..100_000 {
            o2 = offset_correction_step(o2, 0.016, (-20.0, 20.0), scale);
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
        // Moving away from the hand content never heals: the view pans 1:1
        // with the hand, so the correction can never stick or reverse it.
        let away = offset_correction_step((300.0, 0.0), 0.016, (100.0, 0.0), (1.5, 1.5));
        assert_eq!(away.0, 300.0, "away-motion must not heal");
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
            view = corrected;
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
    fn wheel_levels_scroll_up_from_floor_when_max_equals_nine_times_min() {
        // Regression: when max=9 and min=1, level 0 and level 1 both map to
        // 1×. Scrolling up from level 0 used to produce 1× (same as current)
        // so the no-change guard blocked the wheel. The fix skips levels that
        // don't change the zoom.
        assert_eq!(wheel_levels_next(1.0, 1.0, 9.0, 1.0), 9.0 * 2.0 / 9.0);
        // Scrolling down from level 1 stays at the floor.
        assert_eq!(wheel_levels_next(1.0, 1.0, 9.0, -1.0), 1.0);
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
    fn snap_render_origin_locks_texel_boundaries_to_pixel_boundaries() {
        // One capture texel spans 4 buffer px (zoom 2 on the GPU path). With
        // the phase locked to half a texel, texel boundary i lands at buffer
        // px `(i - origin) * 4 - 0.5`, which must be an integer for every
        // texel — i.e. the magnified block edges sit exactly on physical
        // pixels.
        let f = 4.0;
        let origin = snap_render_origin(123.456, f);
        for i in 100..110 {
            let boundary = (i as f64 - origin) * f - 0.5;
            assert!(
                (boundary - boundary.round()).abs() < 1e-9,
                "texel {i} boundary {boundary} not on a pixel boundary"
            );
        }
        // An already-aligned origin is left untouched.
        assert_eq!(snap_render_origin(-149.875, f), -149.875);
        // The snap never moves the origin by more than half a texel.
        for probe in [0.0, 0.123, 77.7, -50.25, 1e6 + 0.999] {
            let snapped = snap_render_origin(probe, f);
            assert!(
                ((snapped - probe) * f).abs() <= 0.5 + 1e-9,
                "snap moved origin {probe} -> {snapped} by more than half a texel"
            );
        }
        // Degenerate factors fall through to the input unchanged.
        assert_eq!(snap_render_origin(10.0, 0.0), 10.0);
        assert_eq!(snap_render_origin(10.0, f64::NAN), 10.0);
    }

    #[test]
    fn snap_render_origin_keeps_phase_half_a_texel() {
        // The lock condition is `fract(origin * px_per_texel) == 0.5` for any
        // input, at any integral pixel-per-texel factor.
        for f in [1.0, 2.0, 4.0, 6.0, 10.0] {
            for probe in [-100.0, -3.7, 0.0, 0.25, 42.9, 999.99] {
                let snapped = snap_render_origin(probe, f);
                let phase = (snapped * f).fract();
                let phase = (phase - phase.round()).abs();
                assert!(
                    (phase - 0.5).abs() < 1e-9,
                    "phase of snap({probe}, {f}) = {phase}, want 0.5"
                );
            }
        }
    }

    #[test]
    fn snap_src_to_cursor_lattice_aligns_screen_blocks_with_cursor_blocks() {
        // The settle snaps the sampling origin so the capture-texel lattice
        // coincides with the cursor's fixed lattice (whose sprite origin is
        // `cursor_origin`) — like two layers in a bitmap editor — while the
        // cursor itself stays at the dead center. Verify: crisp integer texel
        // boundaries, texel start positions that lie exactly on the cursor's
        // block lattice, and a content shift of at most half a capture pixel.
        let factor: f64 = 6.0; // zoom 3 on the GPU path
        let center = 1600.5;
        let raw_src = center - 1829.0 / 3.0 / 2.0;
        // Dead-center cursor sprite origin: W = viewport center surface px,
        // hotspot offset hx = 5 * factor.
        let w: f64 = 1829.0;
        let cursor_origin = (w - 5.0 * factor).round();
        let src = snap_src_to_cursor_lattice(raw_src, cursor_origin, factor);

        // The settle never moves the content by more than half a capture px.
        assert!(
            (src - raw_src).abs() <= 0.5 + 1e-9,
            "settle shifted origin by {:.3} capture px",
            src - raw_src
        );
        // Texel boundaries are integers (crisp blocks).
        let b0 = -src * factor - 0.5;
        assert!(
            (b0 - b0.round()).abs() < 1e-9,
            "texel boundary {b0} not on a pixel boundary"
        );
        // The texel lattice lies on the cursor's block lattice: every texel
        // start is cursor_origin + k * factor for integer k.
        for j in 0..16 {
            let texel_start = (b0 + j as f64 * factor).round();
            let k = (texel_start - cursor_origin) / factor;
            assert!(
                (k - k.round()).abs() < 1e-9,
                "texel {j} start {texel_start} not on the cursor lattice"
            );
        }
    }

    #[test]
    fn snap_src_to_cursor_lattice_keeps_blocks_crisp_at_any_rest_position() {
        // At any arbitrary rest position the settle must produce the same
        // guarantees (integer texel boundaries on the cursor lattice), with
        // the shift bounded to half a capture px.
        let factor: f64 = 4.0; // zoom 2, GPU path
        let w: f64 = 1829.0;
        let cursor_origin = (w - 3.0 * factor).round();
        for center in [0.25, 33.7, 100.0, 777.777, 1600.5, 3199.999] {
            let raw_src = center - 1829.0 / 2.0 / 2.0;
            let src = snap_src_to_cursor_lattice(raw_src, cursor_origin, factor);
            assert!((src - raw_src).abs() <= 0.5 + 1e-9);
            let b0 = -src * factor - 0.5;
            assert!(
                (b0 - b0.round()).abs() < 1e-9,
                "center {center}: texel boundary {b0} not integer"
            );
            for j in 0..8 {
                let texel_start = (b0 + j as f64 * factor).round();
                let k = (texel_start - cursor_origin) / factor;
                assert!(
                    (k - k.round()).abs() < 1e-9,
                    "center {center}: texel {j} start {texel_start} off the cursor lattice"
                );
            }
        }
    }

    #[test]
    fn quantize_center_rounds_to_integer_capture_pixels_and_clamps() {
        // The launch snap: any center quantizes to a whole capture pixel
        // (never a fractional one), and stays inside the capture.
        assert_eq!(
            quantize_center_to_pixel_grid((123.4, 55.6), (3200.0, 2000.0)),
            (123.0, 56.0)
        );
        assert_eq!(
            quantize_center_to_pixel_grid((123.5, 55.5), (3200.0, 2000.0)),
            (124.0, 56.0)
        );
        // Walls: rounding can never push the cursor out of the capture, and
        // the exact edges stay reachable.
        assert_eq!(
            quantize_center_to_pixel_grid((-0.4, -0.6), (3200.0, 2000.0)),
            (0.0, 0.0)
        );
        assert_eq!(
            quantize_center_to_pixel_grid((3199.6, 1999.6), (3200.0, 2000.0)),
            (3200.0, 2000.0)
        );
        assert_eq!(
            quantize_center_to_pixel_grid((6400.0, 3.0), (3200.0, 2000.0)),
            (3200.0, 3.0)
        );
    }

    #[test]
    fn quantized_center_locks_cursor_texels_to_screen_texels() {
        // With the view center quantized to an integer capture pixel, the
        // capture texel under the magnified cursor starts exactly at the
        // viewport center — so the cursor's hotspot texel and that capture
        // texel occupy the same block, and the two lattices coincide for
        // every texel (both are `viewport_center + m * px_per_texel`).
        let w: f64 = 1829.0; // logical viewport width
        let rs: f64 = 2.0; // RENDER_SCALE
        let zoom: f64 = 3.0;
        let view_w = w / zoom; // capture px visible across the viewport
        let px_per_texel = zoom * rs;
        // Integer center: texel `C == center` starts at the viewport center.
        let center: f64 = 1234.0;
        let src = center - view_w / 2.0;
        let texel_start = (center - src) * px_per_texel;
        assert!(
            (texel_start - w * rs / 2.0).abs() < 1e-9,
            "texel start {texel_start}"
        );
        // Fractional center (what the quantization eliminates): the texel
        // under the cursor starts short of the viewport center, so the
        // cursor hotspot straddles two texels — misaligned.
        let center_f: f64 = 1234.4;
        let src_f = center_f - view_w / 2.0;
        let texel_start_f = (center_f.floor() - src_f) * px_per_texel;
        assert!(texel_start_f < w * rs / 2.0 - 1e-9);
        // The lattice property holds for every integer center and zoom.
        for zoom in [1.0_f64, 1.33, 2.0, 3.5, 8.0] {
            let view_w = w / zoom;
            for center in [0.0_f64, 1.0, 640.0, 1599.0] {
                let src = center - view_w / 2.0;
                let texel_start = (center - src) * (zoom * rs);
                assert!(
                    (texel_start - w * rs / 2.0).abs() < 1e-9,
                    "zoom {zoom} center {center}: texel start {texel_start}"
                );
            }
        }
    }

    #[test]
    fn minimap_layout_pins_configured_corner_with_capture_aspect() {
        let (x, y, w, h) = minimap_layout(
            (1400.0, 900.0),
            (3200.0, 2000.0),
            14.0,
            crate::osd::Corner::BottomRight,
        );
        // Width is ~22 % of the viewport (inside the 140..360 clamp range).
        assert!((w - 1400.0 * 0.22).abs() < 1.0, "w = {w}");
        // Height follows the capture aspect (16:10).
        assert!((h - w * 2000.0 / 3200.0).abs() < 1.0, "h = {h}");
        // Pinned to the bottom-right corner with the margin.
        assert!((x + w + 14.0 - 1400.0).abs() < 1.0, "x = {x}");
        assert!((y + h + 14.0 - 900.0).abs() < 1.0, "y = {y}");
        // Wide viewports clamp the width (never a huge minimap).
        let (x, y, w, _h) = minimap_layout(
            (4000.0, 2000.0),
            (3200.0, 2000.0),
            14.0,
            crate::osd::Corner::BottomRight,
        );
        assert_eq!(w, 360.0);
        assert!(x > 0.0 && y > 0.0);
        // Top-left placement.
        let (x, y, _w, _h) = minimap_layout(
            (1400.0, 900.0),
            (3200.0, 2000.0),
            14.0,
            crate::osd::Corner::TopLeft,
        );
        assert!((x - 14.0).abs() < 1.0, "x = {x}");
        assert!((y - 14.0).abs() < 1.0, "y = {y}");
    }

    #[test]
    fn minimap_marker_tracks_the_visible_region_and_center() {
        // A zoomed-in view centered on the capture: the visible region is the
        // viewport size divided by the zoom, centered on the cursor.
        let rect = minimap_marker_rect((1600.0, 1000.0), 2.0, (800.0, 500.0)).unwrap();
        assert_eq!(rect, (1400.0, 875.0, 1800.0, 1125.0));
        // At fit zoom the whole capture is visible.
        let rect = minimap_marker_rect((1600.0, 1000.0), 0.5, (800.0, 500.0)).unwrap();
        assert_eq!(rect, (800.0, 500.0, 2400.0, 1500.0));
        // Degenerate zoom draws no outline.
        assert_eq!(minimap_marker_rect((0.0, 0.0), 0.0, (800.0, 500.0)), None);
    }

    #[test]
    fn minimap_marker_has_no_stray_pixels_at_deep_zoom() {
        // At deep zoom the visible-region outline is smaller than the marker
        // dot and must be skipped entirely — otherwise its corner pixels poke
        // out from behind the dot (the "single pixel following the cursor"
        // artifact). The circle must also have no degenerate 1 px spikes.
        let mut cap = RgbaBuffer::new(3200, 2000);
        for px in cap.data.chunks_exact_mut(4) {
            px.copy_from_slice(&[100, 100, 100, 255]);
        }
        let (sprite, _base) = build_minimap_sprite(
            &cap,
            (1600.0, 1000.0),
            20.0,
            (1829.0, 1143.0),
            1.0,
            crate::osd::Corner::BottomRight,
            None,
            None,
            crate::config::MinimapOutlineScheme::Gradient,
            0.2,
            2.0,
            0.25,
            9.0,
            &mut None,
            &mut None,
        );
        let buf = sprite.unwrap().buffer;
        let mut amber = 0;
        let mut black_rows = std::collections::BTreeMap::<i32, i32>::new();
        for y in 0..buf.height {
            for x in 0..buf.width {
                let i = (y as usize * buf.width as usize + x as usize) * 4;
                let c = [buf.data[i], buf.data[i + 1], buf.data[i + 2]];
                if c == [255, 200, 70] {
                    amber += 1;
                } else if c == [0, 0, 0] {
                    *black_rows.entry(y).or_insert(0) += 1;
                }
            }
        }
        assert_eq!(
            amber, 0,
            "no visible-region outline may survive at deep zoom"
        );
        for (y, n) in &black_rows {
            assert!(
                *n >= 3,
                "black outline row {y} has only {n} px (degenerate spike)"
            );
        }
        // At a moderate zoom the outline IS drawn (it is useful and larger
        // than the dot).
        let (sprite, _base) = build_minimap_sprite(
            &cap,
            (1600.0, 1000.0),
            3.0,
            (1829.0, 1143.0),
            1.0,
            crate::osd::Corner::BottomRight,
            None,
            None,
            crate::config::MinimapOutlineScheme::Gradient,
            0.2,
            2.0,
            0.25,
            9.0,
            &mut None,
            &mut None,
        );
        let buf = sprite.unwrap().buffer;
        let amber = buf
            .data
            .chunks_exact(4)
            .filter(|px| px[0] == 255 && px[1] == 200 && px[2] == 70)
            .count();
        // Four corner brackets (7 px legs, one shared pixel each) — present,
        // but far fewer pixels than a full solid perimeter (~200), so the
        // outline is the unobtrusive bracket style, not solid lines.
        assert!(
            (40..=80).contains(&amber),
            "expected corner brackets at moderate zoom, got {amber} px"
        );
    }

    #[test]
    fn inpaint_cursor_region_scrubs_the_baked_cursor() {
        // A 100x100 frame with a bright region (the "baked cursor") at the
        // center and a flat gray surround.
        let mut buf = RgbaBuffer::new(100, 100);
        for px in buf.data.chunks_exact_mut(4) {
            px.copy_from_slice(&[128, 128, 128, 255]);
        }
        let (cx, cy) = (50.0, 50.0);
        for y in (cy as i32 - 8)..(cy as i32 + 8) {
            for x in (cx as i32 - 8)..(cx as i32 + 8) {
                let i = (y as usize * 100 + x as usize) * 4;
                buf.data[i..i + 3].copy_from_slice(&[255, 0, 0]);
            }
        }
        // A distant corner pixel keeps its own color: the fill is bounded.
        buf.data[4..7].copy_from_slice(&[0, 0, 255]);
        let scrubbed = inpaint_cursor_region(&buf, cx, cy, 10);
        // The region is gone (flat gray again)…
        let mid = (50usize * 100 + 50) * 4;
        assert_eq!(&scrubbed.data[mid..mid + 3], &[128, 128, 128]);
        // …the original buffer is untouched, and the corner keeps its color.
        assert_eq!(&buf.data[mid..mid + 3], &[255, 0, 0]);
        assert_eq!(&scrubbed.data[4..7], &[0, 0, 255]);
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
        blend_overlay_into(&mut canvas, 4, &ov, 4, 4, 0, 0);
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
    /// `draw_cursor_at` takes the sprite origin (top-left corner); this
    /// helper computes the origin that lands the sprite's hotspot on `target`.
    fn origin_for_hotspot_at(target: (i32, i32), hotspot: (i32, i32)) -> (i32, i32) {
        (target.0 - hotspot.0, target.1 - hotspot.1)
    }

    #[test]
    fn draw_cursor_at_blits_ring_and_center_unmirrored() {
        let mut canvas = vec![0u8; 64 * 64 * 4];
        let stride = 64 * 4;
        let (sprite, (hx, hy)) = crate::cursor::MagnifiedCursor::from_reticle(1.0).sprite(1.0);
        let hotspot = (hx.round() as i32, hy.round() as i32);

        // Place the origin so the reticle's center (its hotspot) lands on
        // (32, 32) — the same visual result as before.
        let origin = origin_for_hotspot_at((32, 32), hotspot);
        MagnifierWindow::draw_cursor_at(&mut canvas, stride, origin, &sprite, hotspot);

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

        let origin = origin_for_hotspot_at((40, 32), hotspot);
        MagnifierWindow::draw_cursor_at(&mut canvas, stride, origin, &sprite, hotspot);
        // Center moved +8 in x: the pixel at 40 must be the black center, and
        // the mirrored destination (24) must stay untouched.
        assert_eq!(canvas_at(&canvas, stride, 40, 32), Some([0, 0, 0, 255]));
        assert_eq!(canvas_at(&canvas, stride, 24, 32), Some([0, 0, 0, 0]));
    }

    #[test]
    fn draw_cursor_at_places_hotspot_at_target_origin() {
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

        // Origin = target - hotspot: the sprite is drawn with its top-left
        // corner at the origin, so the hotspot pixel lands exactly on target.
        let origin = origin_for_hotspot_at((50, 50), hotspot);
        assert_eq!(origin, (49, 49));
        MagnifierWindow::draw_cursor_at(&mut canvas, stride, origin, &sprite, hotspot);
        // The hotspot pixel of the sprite lands exactly on the target.
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
