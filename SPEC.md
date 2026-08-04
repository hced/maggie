# SPEC.md: Maggie – Technical & Functional Specification

## 1. Project Overview & Scope
**Maggie** is a native Wayland screen magnifier and utility tool written in **Rust (Edition 2024)**. It is designed to run seamlessly on minimal desktop environments and tiling window managers (such as **Niri**) without relying on legacy X11 layers or fallback architectures.

---

## 2. Core Architecture & Environment
* **Language:** Rust (Edition 2024)
* **Display Server Protocol:** Native Wayland exclusively.
* **Task Management:** Managed via a project root `Justfile` (e.g., `just build`, `just run`, `just check`, `just test`).
* **GPU Acceleration:** Rendering is GPU-accelerated via **EGL + OpenGL ES 2** (`src/gpu.rs`): the frozen frame and OSD legend are drawn as textured quads and presented through a `wl_egl_window` at a 2× buffer scale. If EGL/GLES initialization fails, the app **automatically falls back to the CPU rendering path** (`src/render.rs`), which remains fully functional; the choice is made once at startup.

---

## 3. Screen Capture & Modes — *Implemented*
* **Frozen-frame model:** The screen is captured exactly **once** at startup via `zwlr_screencopy` (`capture_output` on the current output). The SHM buffer (XRGB8888/ARGB8888) is converted to RGBA with a stride-aware row copy, honoring the `y_invert` flag.
* **Capture-before-content:** The fullscreen layer-shell overlay (`Layer::Overlay`) is committed at startup but presents **no image data until the first captured frame is ready**; the initial screencopy therefore never contains the overlay, avoiding the Droste-effect self-feedback that plagues live-capture magnifiers.
* **Frozen view:** After the first frame, the overlay displays a nearest-neighbor zoomed view of the frozen frame. Zoom keys `1`–`9` re-scale the same frozen frame; the screen is never re-captured at runtime.
* **Fullscreen viewport at launch:** The viewport covers the **entire physical screen** from the very first presented buffer — including any shell bars (e.g., the Noctalia shell top bar) in the magnified capture — with **no compositor window rules** required. The layer-shell overlay is created on the **Overlay layer** (topmost) with **all anchors** and an **exclusive zone of −1** ("dont care"): without it, niri/Smithay shrinks the overlay geometry around the reserved zones of bars/docks (e.g., the Noctalia top bar) regardless of layer, leaving the real bar covering the magnifier's top strip. The surface explicitly requests the **full output logical size** and **re-asserts it on every configure**, **redrawing immediately** whenever a configure changes the size. The **GPU (EGL) window is initialized lazily at the first configure**, when the real output logical size is known, so the very first presented buffer is already fullscreen: creating the `wl_egl_window` with a hardcoded size and calling `wl_egl_window_resize` before the first `eglSwapBuffers` has no effect on the first attached buffer, which previously left the viewport smaller than the screen until the first pointer motion triggered a second swap (regression fixed in commit `99d2dd2`).
* **Failure handling:** A failed capture is retried up to **3 times**; if all retries fail, the overlay renders black.
* **Zoom centering:** The viewport follows the **live cursor** over the frozen frame — the view is centered on the content under the cursor and clamped at the capture edges; if the cursor was never seen, it centers on the capture. Zoom keys `1`–`9` re-scale.
* **Cursor-following motion:** The view center is tracked at **sub-pixel precision** (fractional logical pointer position mapped into source space via per-axis source/viewport ratios `scale_x`/`scale_y`, so capture buffers whose aspect ratio differs from the output are handled correctly). The motion style is selected by the `cursor_follow` config option (§4): `snap` (**default** — instant linear motion, no easing or delay), `ease` (exponential smoothing toward the cursor, τ = 40 ms, driven by `wl_surface` frame callbacks until settled — the former default), or `inertia` (ease while moving plus a **momentum glide** after the cursor stops, with damping). Eased positions shift the magnified content continuously on the CPU fallback (bilinear); on the GPU path they are quantized to source texels by nearest-neighbor sampling.
* **Input responsiveness during animation:** Keyboard input (the app's own global keys — zoom/OSD/screenshot bindings) works even while the panning animation is running: the **EGL swap interval is set to 0** so frame redraws never block the event loop. The layer surface's keyboard interactivity is **`on-demand`** (not `exclusive`), so compositor-level global keybindings keep working.
* **Viewport rendering:** The viewport is rendered from the frozen frame as a textured quad. The **GPU path** samples the frame texture with **nearest-neighbor** filtering into a buffer at **2× the layer's logical size** (`wl_surface.set_buffer_scale(2)`), yielding the crisp, pixelated magnifier look. The **CPU fallback** (`src/render.rs`) renders with **bilinear interpolation** at fractional source offsets, shifting the magnified content continuously. Both paths clamp at the capture edges (edge-extension) rather than showing bars.
* **Scroll-wheel zoom:** The scroll wheel zooms in/out in one of two modes selected by `scroll_zoom_mode` (§4): **`levels`** (**default** — each notch steps to the next zoom level of the `1`–`9` keys, i.e. zoom ±1 on whole-number levels clamped to 1–9; from a fractional zoom it snaps to the nearest whole level in the scroll direction) or **`factor`** (the former behavior — each notch multiplies the zoom by **10 %**, clamped to **1×–32×**). By default the wheel direction is reversed compared to conventional zoom (wheel-up zooms **out**, wheel-down zooms **in**); the `invert_scroll_zoom` option (§4) flips back to the conventional direction (wheel-up zooms in). High-resolution `value120` wheel deltas are supported; continuous touchpad scroll is ignored.
* **Viewport clamping:** The viewport is clamped to the capture bounds so it always fills the screen and sticks at the edges; consequently, the pointer drifts off-center as it approaches the screen edges.
* **Magnified cursor (Implemented):** While the pointer is over the viewport, the **hardware cursor is hidden** (a blank 1×1 transparent ARGB cursor surface is set on `wl_pointer` enter, restored on leave) and the **real system cursor bitmap is drawn magnified**. The frozen frame can never contain a baked copy of the system cursor because the screencopy is requested with **`overlay_cursor = 0`** — the wlr-screencopy protocol requires the compositor to exclude the cursor from the capture (honored by niri, wlroots, KWin, Hyprland, …), independent of pointer focus or timing. The **first capture is requested at the first configure, before anything has been rendered** — the frame appears immediately after launch, contains no feedback copy of the magnifier's own overlay (nothing has rendered yet), and the view centers exactly on the pointer's launch position when the pointer enters the surface. *inside* the view at the exact spot where the content under the pointer lands. On startup the cursor theme is loaded from disk (`$XCURSOR_THEME`, default `default`/`Adwaita`, via the standard icon search paths) and parsed with the `xcursor` crate; the pointer's **hotspot** is honored, so the arrow tip lands exactly on the content under the cursor. The image is **nearest-neighbor upscaled by the zoom level** (and the 2× render buffer on the GPU path), so it magnifies along with the visuals. It **tracks the live pointer on every motion event** (never a static sprite). If no theme is found, it falls back to a stylized reticle (white ring, black center). Rendered on both the GPU path (alpha-blended textured quad via the sprite shader, pre-scaled to the 2× render buffer) and the CPU fallback (alpha-blended blit, `MagnifierWindow::draw_cursor_at`). See §5.
* **No live mode:** Continuous live capture is explicitly **out of scope**; the interactive live-magnifier idea (including an earlier planned `--live` flag) was dropped. Behavior modes that assumed a live view (see §6) are obsolete until redefined.

---

## 4. Configuration System — *Partial*
* **Location:** XDG standard configuration directory (`~/.config/maggie/`).
* **Format:** `.ron` (Rusty Object Notation) configuration file.
* **Storage Requirements:**
  * **Default Zoom Level** (floating point).
  * **Cursor-Follow Mode** (`cursor_follow`): `snap` (default) | `ease` | `inertia` — see §3.
  * **Scroll-Wheel Zoom Mode** (`scroll_zoom_mode`): `levels` (default) | `factor` — see §3.
  * **Invert Scroll-Wheel Zoom** (`invert_scroll_zoom`): boolean, default `false` — see §3.
  * **OSD Visibility** (`show_osd`): boolean, default `true` — see §7.
  * **Keybindings** for all in-app functions.
  * **Screenshot Path** (default: `~/Pictures`).
  * **Screenshot Filename Pattern:** Utilizes standard `strftime` formatting tokens (supporting dynamic variables such as `%Y` for year, `%m` for month, `%d` for day, `%H` for hour, `%M` for minute, etc., defaulting to `maggie_%Y%m%d_%H%M%S.png`).
* **Serde defaults:** Newer options (`cursor_follow`, `scroll_zoom_mode`, `invert_scroll_zoom`, `show_osd`) carry `#[serde(default)]`, so config files written before these options existed remain loadable.
* **Status:** Loading from disk is implemented; the listed `strftime` tokens are evaluated on save. **Write-on-change persistence is not wired up** — `save_config` exists but is unused, so runtime adjustments never reach disk.

---

## 5. Visuals & Rendering — *Implemented* (AA toggle: *Stub*)
* **Zoom & Scaling:** Controlled via numeric keyboard keys `1` through `9` to switch zoom levels, or the scroll wheel (default `levels` mode: steps of 1 per notch on the `1`–`9` levels; `factor` mode: 10 % steps, 1×–32× — see §3); keys and wheel re-scale the frozen frame.
* **Rendering Preference:** The default **GPU path** (EGL/GLES2) renders the viewport with **nearest-neighbor** sampling at a **2× buffer scale** for a crisp, pixelated magnifier look; sub-pixel cursor-following is then quantized to source texels (see §3). The **CPU fallback** uses **bilinear interpolation**, which shifts the magnified content continuously during sub-pixel cursor movement. The active path is selected automatically at startup — GPU if EGL/GLES2 initializes, CPU otherwise (see §2).
* **Anti-Aliasing Toggle:** An optional toggle bound to the `A` key, implemented only if it does not introduce development complexity. — *Stub: key is bound but not yet functional.*
* **Magnified cursor rendering:** `src/cursor.rs` (`MagnifiedCursor`) loads the **system cursor theme bitmap** at startup (`xcursor` crate parses the `.cursor` file; premultiplied ARGB pixel data is converted to straight alpha) and caches it as the base image. Each frame it is **nearest-neighbor upscaled** by `zoom × render_scale` into an RGBA buffer; the image's **hotspot** (scaled identically) is passed to the renderer so the cursor tip tracks the content under the pointer. **Why load the theme file instead of the live cursor:** Wayland exposes no protocol to read the compositor-drawn hardware cursor bitmap, so the app reads the same theme files the compositor uses from disk. The sprite is composited above the magnified frame and below the OSD legend. On the GPU path it is drawn with a dedicated sprite vertex shader (rect placement in `u_rect`, explicit NDC y-flip, full-texture sampling) at `RENDER_SCALE` resolution so it matches the crisp 2× buffer; on the CPU path it is blitted at logical scale (`MagnifierWindow::draw_cursor_at`).

---

## 6. Behavior Modes — *Revised*
* The former **Center Cursor** / **Edge Pan** / **Miniature Window** modes remain **obsolete/removed** in the frozen-frame model; they are pending redefinition or removal. Mode-switch bindings (`Ctrl+C`/`Ctrl+E`/`Ctrl+M`) still exist in code but have no rendering effect.
* **Cursor-following:** The former "Center Cursor" concept is now the **built-in default behavior** of the frozen viewport — the view follows the live cursor over the frozen frame, clamped at the capture edges. See §3.

---

## 7. OSD (On-Screen Display) Key Legend — *Implemented*
* Toggable via the `K` key (configurable via `keybindings.toggle_osd`).
* **On by default**, configurable via the `show_osd` configuration file option (§4).
* Dynamically stays or moves out of the way of the cursor position: the legend box is placed in the **corner farthest from the cursor** (4-corner logic, considering the box center).
* The legend background is **opaque** (solid dark box, alpha 255) so no magnified content or text ghost shows through.
* Rendered with a built-in 5×7 bitmap font (`src/osd.rs`) drawn at **2× scale** (5×7 glyphs scaled 2×, i.e. rendered at 10×14); lists zoom level, `1`–`9` zoom, `K` OSD toggle, `F` fullscreen screenshot, `S`/`W`/`C` (pending), and `Q`/`Esc`/`RMB` quit.
* **Rendering fix:** The OSD quad is drawn with its own dedicated vertex shader that samples textures **top-down**, because the captured frame texture is stored **bottom-up** (screencopy `y_invert`) so the main shader's Y-flip compensates it. Previously the top-down OSD sprite was rendered upside down and outside its box, leaving only the opaque background visible as an empty black box; the legend text now renders upright inside the box.

---

## 8. Screenshot Subsystem — *Partial*
Triggered using dedicated keybindings (`S`, `W`, `F`):
* **Manual Selection (`S`):** Allows dragging a rectangular selection region. Once drawn, the side closest to the mouse cursor can be nudged using the **Arrow keys** (1 px per keypress). — *Stub: not yet implemented.*
* **Window Selection (`W`):** Displays a dynamically sized grid representing all available windows for capture. Clicking an item captures and saves that specific window. — *Stub: not yet implemented.*
* **Fullscreen Capture (`F`):** Saves the currently frozen frame as a PNG. — *Implemented.*
* **Cancellation:** Pressing **Escape** during Manual or Window selection mode cancels the operation. — *Pending together with `S`/`W`; Escape currently quits the app.*
* **Output Path Generation:** Combines the configured `screenshot_path` with the evaluated `strftime` filename pattern (`~` expansion and directory creation included). — *Implemented.*

---

## 9. Configuration Window — *Stub*
* Activated via the `C` key.
* Displays all current and future configuration options.
* **Instant Application:** Value modifications take effect immediately without requiring explicit "Apply" or "Cancel" buttons.
* **Factory Reset:** Each setting includes an adjacent reset button to revert individual values back to factory defaults.
* **Persistence:** All runtime adjustments immediately update the underlying configuration file on disk.
* **Status:** Not yet implemented; the `C` key is bound but inert.

---

## 10. CLI Arguments — *Implemented*
The application supports the following command-line interface arguments:
* `-z` or `--zoom <LEVEL>`: Specifies a preferred initial zoom level (e.g., `3`).
* `-d` or `--debug`: Enables debug mode to output verbose troubleshooting logs to `stdout`.
* `--version`: Prints the program version.
* `--help`: Displays help documentation.
